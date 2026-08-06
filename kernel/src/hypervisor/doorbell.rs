#![allow(dead_code)]
//! Inter-VM doorbell — lightweight notification channel.
//!
//! A doorbell has no payload; it is simply a "ring" that wakes the
//! receiver.  The kernel maintains a table of registered doorbells.
//!
//! On the bare-metal backend a doorbell is delivered as GIC SGI 1.
//! On the Gunyah backend it maps to `gh_hypercall_doorbell_send(cap_id)`.
//!
//! Flow:
//!   1. Primary VM calls `doorbell_send(handle)`.
//!   2. Backend delivers an interrupt (SGI / Gunyah doorbell interrupt) to
//!      the target vCPU.
//!   3. Target VM's IRQ handler acks the doorbell and resumes execution.

use super::{DoorbellHandle, HvError, Hypervisor};

/// Registration entry for one doorbell.
#[derive(Clone, Copy)]
pub(crate) struct DoorbellEntry {
    pub handle: DoorbellHandle,
    /// Which VM handle owns (receives) this doorbell.
    pub owner_vm: u32,
    /// IRQ number used to deliver this doorbell (SGI ID on bare-metal).
    pub irq: u32,
}

const MAX_DOORBELLS: usize = 8;

pub struct DoorbellTable {
    entries: [Option<DoorbellEntry>; MAX_DOORBELLS],
    next_handle: u32,
}

impl DoorbellTable {
    pub const fn new() -> Self {
        Self {
            entries: [None; MAX_DOORBELLS],
            next_handle: 1,
        }
    }

    /// Register a new doorbell for `owner_vm`, backed by GIC IRQ `irq`.
    /// Returns the handle.
    pub fn register(&mut self, owner_vm: u32, irq: u32) -> Option<DoorbellHandle> {
        let slot = self.entries.iter_mut().find(|s| s.is_none())?;
        let handle = DoorbellHandle(self.next_handle);
        self.next_handle += 1;
        *slot = Some(DoorbellEntry { handle, owner_vm, irq });
        log::debug!(
            "doorbell: registered handle={:?} vm={} irq={}",
            handle, owner_vm, irq
        );
        Some(handle)
    }

    pub fn find(&self, handle: DoorbellHandle) -> Option<&DoorbellEntry> {
        self.entries
            .iter()
            .filter_map(|s| s.as_ref())
            .find(|e| e.handle == handle)
    }
}

static mut DOORBELL_TABLE: DoorbellTable = DoorbellTable::new();

/// Register a new doorbell.  Returns the handle.
pub fn register(owner_vm: u32, irq: u32) -> Option<DoorbellHandle> {
    unsafe { (*core::ptr::addr_of_mut!(DOORBELL_TABLE)).register(owner_vm, irq) }
}

/// Ring a doorbell through the active hypervisor backend.
pub fn send(handle: DoorbellHandle, hv: &mut dyn Hypervisor) -> Result<(), HvError> {
    let _entry = unsafe { (*core::ptr::addr_of!(DOORBELL_TABLE)).find(handle) }
        .ok_or(HvError::InvalidHandle)?;
    hv.doorbell_send(handle)
}

// ── HVC trap handler for guest-initiated doorbells ────────────────────────────

/// SMCCC vendor call numbers for Tanix inter-VM doorbells.
///
/// These are in the "Vendor Specific Hypervisor Call" range (OEN 0x6,
/// 32-bit calls): 0x8600_xxxx.
pub mod tanix_hvc {
    /// Guest → kernel: ring a doorbell.
    /// x1 = DoorbellHandle value.
    pub const DOORBELL_SEND: u64 = 0x8600_0001;
    /// Guest → kernel: query doorbell handle for VM index.
    /// x1 = VM index.
    pub const DOORBELL_QUERY: u64 = 0x8600_0002;
    /// Return value: success.
    pub const OK: u64 = 0;
    /// Return value: error.
    pub const ERR: u64 = u64::MAX;
}

/// Handle an HVC synchronous exception from a guest VM.
///
/// Called from the exception vector when a guest issues `hvc #0`.
/// Returns the value to place in x0 (return code to the guest).
pub fn handle_hvc(func: u64, arg1: u64, hv: &mut dyn Hypervisor) -> u64 {
    match func {
        tanix_hvc::DOORBELL_SEND => {
            let handle = DoorbellHandle(arg1 as u32);
            match send(handle, hv) {
                Ok(()) => {
                    log::debug!("HVC doorbell_send({:?}) OK", handle);
                    tanix_hvc::OK
                }
                Err(e) => {
                    log::warn!("HVC doorbell_send({:?}) failed: {:?}", handle, e);
                    tanix_hvc::ERR
                }
            }
        }
        _ => {
            log::warn!("HVC unknown func={:#x} arg1={:#x}", func, arg1);
            tanix_hvc::ERR
        }
    }
}
