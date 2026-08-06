#![allow(dead_code)]
//! Hypervisor backend implementations.
//!
//! BareMetalBackend
//! ─────────────────
//! A minimal VMM that runs guest VMs as *cooperative vCPU pairs* inside a
//! single EL1 address space:
//!   • Each "VM" gets a region of guest RAM carved from the frame allocator.
//!   • "vCPU run" is a context switch into the guest; the guest returns
//!     control by calling a kernel-provided yield function (vm::vm_yield).
//!   • "Doorbell" is a software interrupt delivered via GIC SGI (ID 1) —
//!     kept for the Gunyah path; the cooperative demo does not need it.
//!   • "Shared memory" is a physical range handed to the guest via boot
//!     registers; both sides are identity-mapped, so addresses are shared
//!     directly.
//!
//! This gives us the same *API contract* as a Type-1 hypervisor without
//! requiring EL2.  The security guarantees are weaker (no EL2 boundary),
//! but the design maps 1:1 onto Gunyah: vCPU run = GH_VCPU_RUN, doorbell =
//! GH_DOORBELL_SEND, shared memory = GH_MEMEXTENT_DONATE.
//!
//! GunyahBackend (stub)
//! ─────────────────────
//! Issues Gunyah hypercalls via SMCCC HVC.  Stubs only — real
//! implementation requires the Gunyah QEMU fork or SA8295P hardware.
//! Enable probing with the `gunyah` cargo feature; see hypervisor/mod.rs.

use super::{Hypervisor, HvError, VmConfig, VmHandle, DoorbellHandle, ShmemHandle};
use crate::mem::PhysAddr;

// ── Bare-metal backend ────────────────────────────────────────────────────────

/// Maximum number of simultaneously active guest VMs.
const MAX_VMS: usize = 4;

/// Internal VM record tracked by the bare-metal backend.
struct VmRecord {
    handle: VmHandle,
    /// Physical base address of this VM's RAM.
    ram_base: PhysAddr,
    ram_size: usize,
    /// Guest entry point.
    entry: PhysAddr,
    /// True if the VM has been started at least once.
    started: bool,
}

pub struct BareMetalBackend {
    vms: [Option<VmRecord>; MAX_VMS],
    next_handle: u32,
    /// Next SGI doorbell target (cycles 0–7 for SMP cores; we use logical
    /// target = 0b0000_0001 to target CPU 0 in a loopback test).
    next_doorbell: u32,
}

impl BareMetalBackend {
    pub const fn new() -> Self {
        Self {
            vms: [None, None, None, None],
            next_handle: 1,
            next_doorbell: 1,
        }
    }

    fn alloc_handle(&mut self) -> VmHandle {
        let h = VmHandle(self.next_handle);
        self.next_handle += 1;
        h
    }

    fn find_vm(&mut self, handle: VmHandle) -> Option<&mut VmRecord> {
        self.vms
            .iter_mut()
            .filter_map(|s| s.as_mut())
            .find(|v| v.handle == handle)
    }
}

impl Hypervisor for BareMetalBackend {
    fn detect() -> bool {
        true // always available
    }

    fn vm_create(&mut self, config: VmConfig) -> Result<VmHandle, HvError> {
        // Allocate handle and find a free slot before any borrows overlap.
        let handle = self.alloc_handle();

        let slot = self.vms.iter_mut().find(|s| s.is_none())
            .ok_or(HvError::NoMemory)?;

        // NOTE: the guest RAM is not mapped here.  The whole DDR window is
        // pre-mapped as 2 MiB blocks at MMU enable time (mem::page_table),
        // so frames handed out by the allocator are always accessible.
        // The vm::Manager zeroes and loads the image into RAM before this
        // call, so no mapping work remains by the time we get here.

        log::info!(
            "vm_create: handle={:?} ram={:#x}+{:#x} entry={:#x}",
            handle, config.ram_base, config.ram_size, config.entry
        );

        *slot = Some(VmRecord {
            handle,
            ram_base: config.ram_base,
            ram_size: config.ram_size,
            entry: config.entry,
            started: false,
        });

        Ok(handle)
    }

    fn vm_start(&mut self, handle: VmHandle) -> Result<(), HvError> {
        let vm = self.find_vm(handle).ok_or(HvError::InvalidHandle)?;

        if vm.started {
            return Err(HvError::BadState);
        }
        vm.started = true;

        log::info!(
            "vm_start: handle={:?} entry={:#x} (ram {:#x}+{:#x})",
            handle, vm.entry, vm.ram_base, vm.ram_size
        );

        // The actual CPU transfer into the guest is performed by
        // vm::Manager::start via a cooperative context switch (see
        // vm/mod.rs).  This backend only tracks VM lifecycle state; on
        // Gunyah this method would issue GH_VCPU_RUN instead.
        Ok(())
    }

    fn vm_stop(&mut self, handle: VmHandle) -> Result<(), HvError> {
        let vm = self.find_vm(handle).ok_or(HvError::InvalidHandle)?;
        vm.started = false;
        log::info!("vm_stop: handle={:?}", handle);
        Ok(())
    }

    fn vm_destroy(&mut self, handle: VmHandle) -> Result<(), HvError> {
        let slot = self.vms
            .iter_mut()
            .find(|s| s.as_ref().map(|v| v.handle == handle).unwrap_or(false))
            .ok_or(HvError::InvalidHandle)?;
        log::info!("vm_destroy: handle={:?}", handle);
        *slot = None;
        Ok(())
    }

    fn doorbell_send(&mut self, handle: DoorbellHandle) -> Result<(), HvError> {
        // Deliver SGI 1 to CPU 0 (ourselves) — in a real multi-core setup
        // this would target the vCPU of the guest VM.
        // ICC_SGI1R_EL1 format: TargetList[15:0]=1, Aff1[23:16]=0, INTID[27:24]=1
        let sgi1r: u64 = (1u64 << 24) | 0b1; // SGI ID=1, target CPU0

        unsafe {
            core::arch::asm!(
                "msr S3_0_C12_C11_5, {v}", // ICC_SGI1R_EL1
                "isb",
                v = in(reg) sgi1r,
                options(nomem, nostack)
            );
        }

        log::debug!("doorbell_send: handle={:?} SGI1 dispatched", handle);
        Ok(())
    }

    fn mem_share(
        &mut self,
        phys: PhysAddr,
        size: usize,
    ) -> Result<ShmemHandle, HvError> {
        // In bare-metal mode "sharing" means the physical range is already
        // in the kernel's address space (identity-mapped by the MMU setup).
        // We just record the handle and return.
        let handle = ShmemHandle(phys as u32 ^ (size as u32));
        log::info!("mem_share: phys={:#x} size={:#x} → handle={:?}", phys, size, handle);
        Ok(handle)
    }
}

// ── Gunyah backend (stub) ─────────────────────────────────────────────────────

/// Gunyah hypercall function identifiers (SMCCC vendor calls, OEN 0x6).
///
/// These match the ABI defined in the Gunyah hypervisor source and the
/// upstream Linux kernel driver (drivers/virt/gunyah/).
mod gh_hvc {
    /// Identify the Gunyah hypervisor (returns a UID).
    pub const HYP_IDENTIFY: u64 = 0x6000_0000;
    /// Send a doorbell notification to a capability.
    pub const DOORBELL_SEND: u64 = 0x6004_8000;
    /// Configure a vCPU.
    pub const VCPU_RUN: u64 = 0x6004_8001;
    /// Share a memory extent.
    pub const MEMEXTENT_DONATE: u64 = 0x6004_8010;
}

pub struct GunyahBackend {
    next_handle: u32,
}

impl GunyahBackend {
    pub const fn new() -> Self {
        Self { next_handle: 1 }
    }

    /// Issue an SMCCC HVC call and return (x0, x1, x2, x3).
    #[inline]
    unsafe fn hvc(func: u64, arg1: u64, arg2: u64, arg3: u64) -> (u64, u64, u64, u64) {
        let r0: u64;
        let r1: u64;
        let r2: u64;
        let r3: u64;
        core::arch::asm!(
            "hvc #0",
            inout("x0") func => r0,
            inout("x1") arg1 => r1,
            inout("x2") arg2 => r2,
            inout("x3") arg3 => r3,
            options(nomem)
        );
        (r0, r1, r2, r3)
    }
}

impl Hypervisor for GunyahBackend {
    fn detect() -> bool {
        // Issue HYP_IDENTIFY — if Gunyah is present it returns a specific UID.
        // UID bytes: {0x19, 0x47, 0x55, 0x4e} ("GUY\x19" in little-endian).
        let (r0, _r1, _r2, _r3) = unsafe {
            Self::hvc(gh_hvc::HYP_IDENTIFY, 0, 0, 0)
        };
        // Check for Gunyah UID in r0 (simplified — real check inspects all 4).
        r0 == 0x4755_5919
    }

    fn vm_create(&mut self, _config: VmConfig) -> Result<VmHandle, HvError> {
        // TODO Phase 2b: allocate a Gunyah VM capability via RM hypercalls.
        Err(HvError::NotSupported)
    }

    fn vm_start(&mut self, _handle: VmHandle) -> Result<(), HvError> {
        // TODO Phase 2b: GH_VCPU_RUN hypercall.
        Err(HvError::NotSupported)
    }

    fn vm_stop(&mut self, _handle: VmHandle) -> Result<(), HvError> {
        Err(HvError::NotSupported)
    }

    fn vm_destroy(&mut self, _handle: VmHandle) -> Result<(), HvError> {
        Err(HvError::NotSupported)
    }

    fn doorbell_send(&mut self, handle: DoorbellHandle) -> Result<(), HvError> {
        let (r0, _, _, _) = unsafe {
            Self::hvc(gh_hvc::DOORBELL_SEND, handle.0 as u64, 0, 0)
        };
        if r0 == 0 {
            Ok(())
        } else {
            Err(HvError::HypercallFailed(r0))
        }
    }

    fn mem_share(&mut self, phys: PhysAddr, size: usize) -> Result<ShmemHandle, HvError> {
        let (r0, cap_id, _, _) = unsafe {
            Self::hvc(gh_hvc::MEMEXTENT_DONATE, phys as u64, size as u64, 0)
        };
        if r0 == 0 {
            Ok(ShmemHandle(cap_id as u32))
        } else {
            Err(HvError::HypercallFailed(r0))
        }
    }
}
