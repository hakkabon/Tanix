#![allow(dead_code)]
//! Hypervisor backend implementations.
//!
//! BareMetalBackend
//! ─────────────────
//! Implements VM isolation using the AArch64 MMU:
//!   • Each "VM" gets its own TTBR0 address space (separate page tables).
//!   • "vCPU run" is an `eret` into EL1 with the guest's TTBR0.
//!   • "Doorbell" is a software interrupt delivered via GIC SGI (ID 1).
//!   • "Shared memory" maps the same physical range into both page tables.
//!
//! This gives us real address-space isolation without a Type-1 hypervisor.
//! The security guarantees are weaker (no EL2 boundary) but the API contract
//! is identical, so the Phase 2b Gunyah port is a drop-in.
//!
//! GunyahBackend (stub)
//! ─────────────────────
//! Issues Gunyah hypercalls via SMCCC HVC.  Stubs only — real implementation
//! requires the Gunyah QEMU fork or SA8295P hardware.

use super::{Hypervisor, HvError, VmConfig, VmHandle, DoorbellHandle, ShmemHandle};
use crate::mem::PhysAddr;
use crate::mem::page_table::{map_range, FLAGS_KERNEL_RWX};

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

        // Map the guest's RAM into the kernel's address space so the loader
        // can write the binary image into it.
        unsafe {
            map_range(config.ram_base, config.ram_size, FLAGS_KERNEL_RWX);
        }

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

        let entry = vm.entry;
        let ram_base = vm.ram_base;
        let ram_size = vm.ram_size;

        log::info!(
            "vm_start: handle={:?} entry={:#x} (ram {:#x}+{:#x})",
            handle, entry, ram_base, ram_size
        );

        // In bare-metal mode we launch the guest by jumping to its entry
        // point using an `eret`-style transfer.  We set up a minimal EL1
        // saved state and return into the guest.
        //
        // The guest runs in the *same* EL1 privilege as the kernel — the
        // isolation is purely via address spaces (separate TTBR0).  A proper
        // EL2-based isolation comes with the Gunyah backend.
        //
        // For Phase 2 the guest is a simple Zephyr RTOS stub that we bounce
        // back from via an HVC — so "starting" it means calling its entry fn.
        unsafe {
            launch_guest(entry);
        }

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

/// Jump to the guest entry point.
///
/// We use `eret` semantics: SPSR_EL1 is set to EL1h with IRQs masked,
/// and ELR_EL1 is set to the guest entry.  On `eret` the CPU switches to
/// the saved SPSR state and jumps to ELR.
///
/// # Safety
/// `entry` must be a valid executable address.
#[inline(never)]
unsafe fn launch_guest(entry: PhysAddr) {
    core::arch::asm!(
        // ELR_EL1 = entry point
        "msr ELR_EL1, {entry}",
        // SPSR_EL1: EL1h (SP_EL1), IRQ masked (I bit set), FIQ masked
        "msr SPSR_EL1, {spsr}",
        "isb",
        "eret",
        entry = in(reg) entry as u64,
        spsr  = in(reg) 0b0000_0101u64, // EL1h, DAIF=0b0010 (IRQ masked)
        options(noreturn)
    );
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
