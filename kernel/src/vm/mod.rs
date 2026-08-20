#![allow(dead_code)]
//! VM management subsystem.
//!
//! Provides the kernel-side lifecycle manager for guest VMs:
//!   • `vm::Manager`  — create, load, start, resume VMs.
//!   • `vm::loader`   — parse and copy flat binaries / ELF images.
//!   • `vm::shmem`    — shared memory region management.
//!
//! Gunyah-style vCPU model (Phase 13)
//! ────────────────────────────────
//! The manager is a thin policy layer over the `Hypervisor` trait: it
//! allocates and zeroes guest RAM, loads the image, and drives the VM
//! through the backend's `vcpu_run`.  All vCPU execution state (contexts,
//! exit reasons) lives in the backend implementation:
//!
//!   • `Manager::start`  — the VM's first `vcpu_run` (the backend primes
//!     the vCPU context on first entry).
//!   • `Manager::resume` — another `vcpu_run` of a VM that previously
//!     exited (yielded / trapped).
//!
//! On the bare-metal backend `vcpu_run` is a cooperative context switch
//! into the guest and "the guest yielded" is the exit; on Gunyah it is
//! GH_VCPU_RUN and the exit reason comes back from the hypervisor.
//! The guest-facing yield entry (`vm_yield_entry`) lives in the bare-metal
//! backend; `yield_fn_addr()` publishes it as a boot argument.

pub mod loader;
pub mod sched;
pub mod shmem;

use crate::hypervisor::{Hypervisor, HvError, VmConfig, VmHandle};
use crate::mem::{PhysAddr, PAGE_SIZE};
use crate::mem::frame::alloc_frames;

// ── VM descriptor ─────────────────────────────────────────────────────────────

/// Maximum number of VMs managed simultaneously.
pub const MAX_VMS: usize = 4;

/// Internal VM record.
pub struct Vm {
    pub handle: VmHandle,
    /// Name (for debug output).
    pub name: [u8; 16],
    /// Physical address of the VM's RAM region.
    pub ram_base: PhysAddr,
    pub ram_size: usize,
    /// Guest entry point (physical address within the RAM region).
    pub entry: PhysAddr,
    /// Boot arguments passed to the guest in registers at launch:
    ///   boot[0] → x4 (shared-memory physical base)
    ///   boot[1] → x5 (kernel yield-function address)
    pub boot: [u64; 2],
}

impl Vm {
    fn name_str(&self) -> &str {
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(16);
        core::str::from_utf8(&self.name[..end]).unwrap_or("?")
    }
}

// ── Manager ───────────────────────────────────────────────────────────────────

pub struct Manager {
    vms: [Option<Vm>; MAX_VMS],
}

impl Manager {
    pub const fn new() -> Self {
        Self {
            vms: [None, None, None, None],
        }
    }

    /// Allocate `ram_pages` physical frames, zero them, load the binary
    /// image, create the VM through the backend, and return the handle.
    ///
    /// `boot` is passed to the guest in registers at launch (x4, x5).
    pub fn create_and_load(
        &mut self,
        name: &str,
        image: &[u8],
        ram_pages: usize,
        boot: [u64; 2],
        hv: &mut dyn Hypervisor,
    ) -> Result<VmHandle, HvError> {
        // 1. Allocate contiguous physical RAM for the guest.
        let ram_base = unsafe { alloc_frames(ram_pages) }
            .ok_or(HvError::NoMemory)?;
        let ram_size = ram_pages * PAGE_SIZE;

        // 2. Zero the whole region first — the guest expects its BSS and
        //    stack area to start zeroed (its link script is base-0, so the
        //    ELF loader only zeroes per-segment BSS, not the tail).
        unsafe {
            core::ptr::write_bytes(ram_base as *mut u8, 0, ram_size);
        }

        // 3. Load the binary image.
        let entry = loader::load_flat(image, ram_base, ram_size)?;

        // 4. Create VM through backend.
        let config = VmConfig { ram_base, ram_size, entry, boot };
        let handle = hv.vm_create(config)?;

        // 5. Record locally.
        let slot = self.vms.iter_mut().find(|s| s.is_none())
            .ok_or(HvError::NoMemory)?;

        let mut name_buf = [0u8; 16];
        let n = name.len().min(15);
        name_buf[..n].copy_from_slice(&name.as_bytes()[..n]);

        *slot = Some(Vm {
            handle,
            name: name_buf,
            ram_base,
            ram_size,
            entry,
            boot,
        });

        log::info!(
            "vm::Manager: created '{}' handle={:?} ram={:#x}+{} KB entry={:#x}",
            name, handle, ram_base, ram_size / 1024, entry
        );

        Ok(handle)
    }

    /// Run the VM's first vCPU.  Returns when the guest exits (yields /
    /// traps) — the backend's `vcpu_run`.
    pub fn start(&mut self, handle: VmHandle, hv: &mut dyn Hypervisor) -> Result<(), HvError> {
        if self.find(handle).is_none() {
            return Err(HvError::InvalidHandle);
        }
        let exit = hv.vcpu_run(handle, 0)?;
        log::info!("vm::Manager: vCPU 0 exited: {:?}", exit);
        Ok(())
    }

    /// Re-enter a VM that previously exited.  Returns on its next exit.
    pub fn resume(&mut self, handle: VmHandle, hv: &mut dyn Hypervisor) -> Result<(), HvError> {
        if self.find(handle).is_none() {
            return Err(HvError::InvalidHandle);
        }
        let exit = hv.vcpu_run(handle, 0)?;
        log::info!("vm::Manager: vCPU 0 re-exited: {:?}", exit);
        Ok(())
    }

    pub fn find_mut(&mut self, handle: VmHandle) -> Option<&mut Vm> {
        self.vms
            .iter_mut()
            .filter_map(|s| s.as_mut())
            .find(|v| v.handle == handle)
    }

    pub fn find(&self, handle: VmHandle) -> Option<&Vm> {
        self.vms
            .iter()
            .filter_map(|s| s.as_ref())
            .find(|v| v.handle == handle)
    }
}

// ── Global manager ────────────────────────────────────────────────────────────

static mut VM_MANAGER: Manager = Manager::new();

/// Create and load a VM.  Returns its handle.
///
/// `boot` is passed to the guest at launch:
///   boot[0] = shared-memory physical base (guest's `x4`)
///   boot[1] = kernel yield-function address (guest's `x5`)
pub unsafe fn create_vm(
    name: &str,
    image: &[u8],
    ram_pages: usize,
    hv: &mut dyn Hypervisor,
    boot: [u64; 2],
) -> Result<VmHandle, HvError> {
    (*core::ptr::addr_of_mut!(VM_MANAGER)).create_and_load(name, image, ram_pages, boot, hv)
}

/// Start a VM: runs its first vCPU, returns after it exits.
pub unsafe fn start_vm(handle: VmHandle, hv: &mut dyn Hypervisor) -> Result<(), HvError> {
    (*core::ptr::addr_of_mut!(VM_MANAGER)).start(handle, hv)
}

/// Resume a VM that previously exited.  Returns after its next exit.
pub unsafe fn resume_vm(handle: VmHandle, hv: &mut dyn Hypervisor) -> Result<(), HvError> {
    (*core::ptr::addr_of_mut!(VM_MANAGER)).resume(handle, hv)
}

/// Address of the guest-facing yield entry point (bare-metal backend).
///
/// The address is the assembly `vm_yield_entry` prologue in vectors.s —
/// it masks IRQ (guests run with IRQ enabled under Phase 21 co-tenancy;
/// the kernel-side switch must never be interrupted by a preemption tick)
/// and branches into `backend::vm_yield_entry_masked`.  The guest receives
/// this in `x5` at launch and calls it as `fn(guest_ctx: usize)`.
pub fn yield_fn_addr() -> usize {
    extern "C" {
        fn vm_yield_entry();
    }
    vm_yield_entry as *const () as usize
}