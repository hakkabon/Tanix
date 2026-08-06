#![allow(dead_code)]
//! VM management subsystem.
//!
//! Provides the kernel-side lifecycle manager for guest VMs:
//!   • `vm::Manager` — create, load, start, stop VMs.
//!   • `vm::loader`  — parse and copy flat binaries / ELF images.
//!   • `vm::shmem`   — shared memory region management.

pub mod loader;
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
    /// Guest entry point (physical address).
    pub entry: PhysAddr,
    pub running: bool,
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
        Self { vms: [None, None, None, None] }
    }

    /// Allocate `ram_pages` physical frames, load the binary image into
    /// them, create the VM through the backend, and return the handle.
    pub fn create_and_load(
        &mut self,
        name: &str,
        image: &[u8],
        ram_pages: usize,
        hv: &mut dyn Hypervisor,
    ) -> Result<VmHandle, HvError> {
        // 1. Allocate contiguous physical RAM for the guest.
        let ram_base = unsafe { alloc_frames(ram_pages) }
            .ok_or(HvError::NoMemory)?;
        let ram_size = ram_pages * PAGE_SIZE;

        // 2. Load the binary image.
        let entry = loader::load_flat(image, ram_base, ram_size)?;

        // 3. Create VM through backend.
        let config = VmConfig { ram_base, ram_size, entry };
        let handle = hv.vm_create(config)?;

        // 4. Record locally.
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
            running: false,
        });

        log::info!(
            "vm::Manager: created '{}' handle={:?} ram={:#x}+{} KB entry={:#x}",
            name, handle, ram_base, ram_size / 1024, entry
        );

        Ok(handle)
    }

    /// Start a VM.
    pub fn start(&mut self, handle: VmHandle, hv: &mut dyn Hypervisor) -> Result<(), HvError> {
        let vm = self.find_mut(handle).ok_or(HvError::InvalidHandle)?;
        if vm.running {
            return Err(HvError::BadState);
        }
        vm.running = true;
        log::info!("vm::Manager: starting '{}'", vm.name_str());
        hv.vm_start(handle)
    }

    pub fn find_mut(&mut self, handle: VmHandle) -> Option<&mut Vm> {
        self.vms
            .iter_mut()
            .filter_map(|s| s.as_mut())
            .find(|v| v.handle == handle)
    }
}

// ── Global manager ────────────────────────────────────────────────────────────

static mut VM_MANAGER: Manager = Manager::new();

/// Create and load a VM.  Returns its handle.
pub unsafe fn create_vm(
    name: &str,
    image: &[u8],
    ram_pages: usize,
    hv: &mut dyn Hypervisor,
) -> Result<VmHandle, HvError> {
    (*core::ptr::addr_of_mut!(VM_MANAGER)).create_and_load(name, image, ram_pages, hv)
}

/// Start a VM.
pub unsafe fn start_vm(handle: VmHandle, hv: &mut dyn Hypervisor) -> Result<(), HvError> {
    (*core::ptr::addr_of_mut!(VM_MANAGER)).start(handle, hv)
}
