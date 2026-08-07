#![allow(dead_code)]
//! VM management subsystem.
//!
//! Provides the kernel-side lifecycle manager for guest VMs:
//!   • `vm::Manager`  — create, load, start, resume, stop VMs.
//!   • `vm::loader`   — parse and copy flat binaries / ELF images.
//!   • `vm::shmem`    — shared memory region management.
//!
//! Cooperative vCPU model (Phase 3)
//! ────────────────────────────────
//! The bare-metal backend has no EL2, so kernel and guest share EL1 and one
//! address space.  A guest runs as a *cooperative vCPU pair*:
//!
//!   • `Manager::start` / `Manager::resume` switch into the guest with a
//!     `context_switch` (the guest becomes the running "task").
//!   • The guest calls the kernel-provided `vm_yield_entry` function to
//!     hand control back; the kernel resumes right after its switch call.
//!   • Boot arguments (shared-memory base, yield function address, guest
//!     context pointer) are passed in x4/x5/x6 — registers the switch stub
//!     does not touch.  They are caller-saved, so `Manager` re-establishes
//!     them on *every* guest entry (first launch and each resume).
//!
//! This is deliberately shaped like a tiny VMM: on Gunyah (Phase 2b) the
//! same `start`/`resume` calls become GH_VCPU_RUN, and "the guest yielded"
//! becomes "the guest exited" via a doorbell / hypercall exit reason.

pub mod loader;
pub mod shmem;

use crate::hypervisor::{Hypervisor, HvError, VmConfig, VmHandle};
use crate::mem::{PhysAddr, PAGE_SIZE};
use crate::mem::frame::alloc_frames;
use crate::sched::task::context_switch;
use crate::sched::task::Context;

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
    pub running: bool,
}

impl Vm {
    fn name_str(&self) -> &str {
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(16);
        core::str::from_utf8(&self.name[..end]).unwrap_or("?")
    }
}

// ── Cooperative vCPU state ────────────────────────────────────────────────────

/// Contexts backing one kernel ↔ guest switch pair.
pub struct VmRuntime {
    /// Kernel context, saved whenever the kernel switches into the guest.
    pub kernel_ctx: Context,
    /// Guest context, saved whenever the guest yields back to the kernel.
    pub guest_ctx: Context,
}

impl VmRuntime {
    pub const fn new() -> Self {
        Self {
            kernel_ctx: Context::zeroed(),
            guest_ctx: Context::zeroed(),
        }
    }
}

// ── Manager ───────────────────────────────────────────────────────────────────

pub struct Manager {
    vms: [Option<Vm>; MAX_VMS],
    runtime: VmRuntime,
}

impl Manager {
    pub const fn new() -> Self {
        Self {
            vms: [None, None, None, None],
            runtime: VmRuntime::new(),
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
        let config = VmConfig { ram_base, ram_size, entry };
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
            running: false,
        });

        log::info!(
            "vm::Manager: created '{}' handle={:?} ram={:#x}+{} KB entry={:#x}",
            name, handle, ram_base, ram_size / 1024, entry
        );

        Ok(handle)
    }

    /// Enter the guest for the first time.
    ///
    /// Returns when the guest yields control back (its first `yield`).
    pub fn start(&mut self, handle: VmHandle, hv: &mut dyn Hypervisor) -> Result<(), HvError> {
        let vm = self.find_mut(handle).ok_or(HvError::InvalidHandle)?;
        if vm.running {
            return Err(HvError::BadState);
        }
        vm.running = true;
        let entry = vm.entry;
        let stack_top = vm.ram_base + vm.ram_size;
        let boot = vm.boot;

        hv.vm_start(handle)?;

        let Manager { runtime, .. } = self;
        runtime.guest_ctx = Context::new(entry, stack_top);

        log::info!(
            "vm::Manager: entering guest entry={:#x} sp={:#x}",
            entry, stack_top
        );

        unsafe {
            enter_guest(&mut runtime.kernel_ctx, &runtime.guest_ctx, boot);
        }

        log::info!("vm::Manager: guest yielded control");
        Ok(())
    }

    /// Re-enter a guest that previously yielded.
    ///
    /// Returns when the guest yields again.
    pub fn resume(&mut self, handle: VmHandle, _hv: &mut dyn Hypervisor) -> Result<(), HvError> {
        let boot = self
            .find(handle)
            .map(|v| (v.running, v.boot))
            .unwrap_or((false, [0; 2]));
        if !boot.0 {
            return Err(HvError::BadState);
        }
        let boot = boot.1;

        let Manager { runtime, .. } = self;

        unsafe {
            // The guest's boot args are caller-saved, so the kernel clobbers
            // x4/x5/x6 between a yield and its resume.  Re-establish them on
            // every re-entry, exactly as at first launch.
            enter_guest(&mut runtime.kernel_ctx, &runtime.guest_ctx, boot);
        }
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

/// Start a VM: enters the guest, returns after its first yield.
pub unsafe fn start_vm(handle: VmHandle, hv: &mut dyn Hypervisor) -> Result<(), HvError> {
    (*core::ptr::addr_of_mut!(VM_MANAGER)).start(handle, hv)
}

/// Resume a VM that previously yielded.  Returns after its next yield.
pub unsafe fn resume_vm(handle: VmHandle, hv: &mut dyn Hypervisor) -> Result<(), HvError> {
    (*core::ptr::addr_of_mut!(VM_MANAGER)).resume(handle, hv)
}

/// Address of the guest-facing yield entry point.
///
/// The guest receives this in `x5` at launch and calls it as
/// `fn(guest_ctx: usize)` to hand control back to the kernel.
pub fn yield_fn_addr() -> usize {
    vm_yield_entry as *const () as usize
}

/// Called *by the guest* to yield control back to the kernel.
///
/// Saves the guest's current context into `guest_ctx` (whose address was
/// passed to the guest in `x6` at launch) and restores the kernel context,
/// so the kernel continues right after its `context_switch` call.  When the
/// kernel later resumes the guest, this function returns and the guest
/// continues where it left off.
///
/// # Safety
/// `guest_ctx` must point to the manager's `runtime.guest_ctx`.
#[no_mangle]
pub extern "C" fn vm_yield_entry(guest_ctx: *mut Context) {
    unsafe {
        let mgr = core::ptr::addr_of_mut!(VM_MANAGER);
        let kernel_ctx = &mut *core::ptr::addr_of_mut!((*mgr).runtime.kernel_ctx);
        context_switch(guest_ctx, kernel_ctx);
    }
}

/// Switch from the kernel to the guest, re-establishing the guest's boot
/// args in x4/x5/x6 (shared-memory base, yield function, guest-context
/// pointer).
///
/// `context_switch` only saves/restores x19–x28 + fp/lr/sp, so the boot
/// args are caller-saved and must be set on *every* entry — first launch
/// and each resume — because the kernel's own execution between a yield
/// and its resume clobbers them.  Everything happens inside one `asm!`
/// block so the compiler cannot interleave instructions between the
/// register loads and the switch; the block must therefore clobber every
/// caller-saved register, since `bl` lets the guest run arbitrary code.
///
/// Returns when the guest yields control back (guest context saved,
/// kernel context restored).
///
/// # Safety
/// `kernel_ctx` must belong to the calling task and `guest_ctx` to the
/// guest being entered.
unsafe fn enter_guest(kernel_ctx: &mut Context, guest_ctx: &Context, boot: [u64; 2]) {
    core::arch::asm!(
        "mov x4, {s}",
        "mov x5, {y}",
        "mov x6, {c}",
        "mov x0, {k}",
        "mov x1, {c}",
        "bl context_switch",
        s = in(reg) boot[0],
        y = in(reg) boot[1],
        c = in(reg) core::ptr::addr_of!(*guest_ctx) as u64,
        k = in(reg) core::ptr::addr_of_mut!(*kernel_ctx) as u64,
        // `bl` lets the guest run arbitrary code, which clobbers every
        // caller-saved register (x0–x18) — declare them all.  x19–x28,
        // fp and sp are *not* clobbered: `context_switch` saves them into
        // `kernel_ctx` before the guest runs and restores them on yield.
        // (x19 is additionally reserved by LLVM and kept alive on its own.)
        out("x0") _, out("x1") _, out("x2") _, out("x3") _,
        out("x4") _, out("x5") _, out("x6") _, out("x7") _,
        out("x8") _, out("x9") _, out("x10") _, out("x11") _,
        out("x12") _, out("x13") _, out("x14") _, out("x15") _,
        out("x16") _, out("x17") _, out("x18") _,
        out("x30") _,
    );
}