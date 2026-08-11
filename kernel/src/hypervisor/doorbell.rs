#![allow(dead_code)]
//! Inter-VM doorbell + guest-facing VMM service ABI.
//!
//! Doorbells: a doorbell has no payload; it is simply a "ring" that wakes
//! the receiver.  On the bare-metal backend a doorbell is delivered as GIC
//! SGI 1; on the Gunyah backend it maps to `GH_BELL_SEND(cap_id)`.
//!
//! VMM service ABI (`tanix_hvc`): the guest talks to the hypervisor
//! through function calls with the same ABI on every backend:
//!   * real Gunyah - the guest issues SMCCC HVCs; the kernel's EL1 HVC
//!     handler (`EC_HVC64`) dispatches them through `handle_hvc`;
//!   * bare-metal EL1 assist - the guest calls the kernel-published
//!     `vmm_service` function pointer (conveyed in the shared-memory info
//!     block), which dispatches through the very same `handle_hvc`.
//!
//! The function set mirrors Gunyah objects: doorbell send/query plus
//! message-queue send/recv (Gunyah `MSGQ_SEND`/`MSGQ_RECV`).

use super::{Hypervisor, HvError, MsgqHandle, MSGQ_MAX_MSG_SIZE};

// ── Doorbell table ────────────────────────────────────────────────────────────

/// Registration entry for one doorbell.
#[derive(Clone, Copy)]
pub(crate) struct DoorbellEntry {
    pub handle: super::DoorbellHandle,
    /// Which VM handle owns (receives) this doorbell.
    pub owner_vm: u32,
    /// IRQ number used to deliver this doorbell (SGI ID on bare-metal).
    pub irq: u32,
    /// Gunyah `BELL_SET_MASK` flags (bare-metal delivery ignores them).
    pub enable_mask: u64,
    pub ack_mask: u64,
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
    pub fn register(&mut self, owner_vm: u32, irq: u32) -> Option<super::DoorbellHandle> {
        let slot = self.entries.iter_mut().find(|s| s.is_none())?;
        let handle = super::DoorbellHandle(self.next_handle);
        self.next_handle += 1;
        *slot = Some(DoorbellEntry {
            handle,
            owner_vm,
            irq,
            enable_mask: 0,
            ack_mask: 0,
        });
        log::debug!(
            "doorbell: registered handle={:?} vm={} irq={}",
            handle, owner_vm, irq
        );
        Some(handle)
    }

    pub fn set_mask(
        &mut self,
        handle: super::DoorbellHandle,
        enable_mask: u64,
        ack_mask: u64,
    ) -> Result<(), HvError> {
        let e = self.entries.iter_mut().flatten().find(|e| e.handle == handle)
            .ok_or(HvError::InvalidHandle)?;
        e.enable_mask = enable_mask;
        e.ack_mask = ack_mask;
        Ok(())
    }

    pub fn find(&self, handle: super::DoorbellHandle) -> Option<&DoorbellEntry> {
        self.entries
            .iter()
            .filter_map(|s| s.as_ref())
            .find(|e| e.handle == handle)
    }
}

static mut DOORBELL_TABLE: DoorbellTable = DoorbellTable::new();

/// Register a new doorbell.  Returns the handle.
pub fn register(owner_vm: u32, irq: u32) -> Option<super::DoorbellHandle> {
    unsafe { (*core::ptr::addr_of_mut!(DOORBELL_TABLE)).register(owner_vm, irq) }
}

/// Configure the Gunyah-style flag masks of a doorbell.
pub fn set_mask(
    handle: super::DoorbellHandle,
    enable_mask: u64,
    ack_mask: u64,
) -> Result<(), HvError> {
    unsafe { (*core::ptr::addr_of_mut!(DOORBELL_TABLE)).set_mask(handle, enable_mask, ack_mask) }
}

/// Ring a doorbell through the active hypervisor backend.
pub fn send(handle: super::DoorbellHandle, hv: &mut dyn Hypervisor) -> Result<(), HvError> {
    let _entry = unsafe { (*core::ptr::addr_of!(DOORBELL_TABLE)).find(handle) }
        .ok_or(HvError::InvalidHandle)?;
    hv.doorbell_send(handle)
}
// ── Guest → VMM service ABI ───────────────────────────────────────────────────

/// Vendor-specific hypercall numbers for the Tanix VMM services.
///
/// These are in the "Vendor Specific Hypervisor Call" range (OEN 0x6):
/// 0x8600_xxxx (matches the call-type-encoding used by the real Gunyah
/// hypercalls, with function ids in the guest-defined area).
pub mod tanix_hvc {
    /// Guest -> kernel: ring a doorbell.  x1 = DoorbellHandle.
    pub const DOORBELL_SEND: u64 = 0x8600_0001;
    /// Guest -> kernel: query doorbell handle for VM index.  x1 = VM index.
    pub const DOORBELL_QUERY: u64 = 0x8600_0002;
    /// Guest -> kernel: send a message on a queue.
    /// x1 = MsgqHandle, x2 = buffer, x3 = size.
    pub const MSGQ_SEND: u64 = 0x8600_0003;
    /// Guest -> kernel: receive a message from a queue.
    /// x1 = MsgqHandle, x2 = buffer, x3 = buffer capacity.
    /// Returns the received size in x0.
    pub const MSGQ_RECV: u64 = 0x8600_0004;
    /// Return value: success.
    pub const OK: u64 = 0;
    /// Return value: error.
    pub const ERR: u64 = u64::MAX;
}

/// Dispatch one VMM service call.  `args` are the guest's x0..x3 (func id
/// in `args[0]`); returns the value to place in x0.
pub fn handle_hvc(args: [u64; 4], hv: &mut dyn Hypervisor) -> u64 {
    let func = args[0];
    match func {
        tanix_hvc::DOORBELL_SEND => {
            let handle = super::DoorbellHandle(args[1] as u32);
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

        tanix_hvc::MSGQ_SEND => {
            let mq = MsgqHandle(args[1] as u32);
            let size = (args[3] as usize).min(MSGQ_MAX_MSG_SIZE);
            // The guest shares the kernel's address space (EL1 assist) or
            // has the buffer mapped in (lower-EL case) — plain deref.
            let msg = unsafe { core::slice::from_raw_parts(args[2] as *const u8, size) };
            match hv.msgq_send(mq, msg) {
                Ok(_ready) => {
                    log::debug!("HVC msgq_send({:?}, {} B) OK", mq, size);
                    tanix_hvc::OK
                }
                Err(e) => {
                    log::warn!("HVC msgq_send({:?}) failed: {:?}", mq, e);
                    tanix_hvc::ERR
                }
            }
        }

        tanix_hvc::MSGQ_RECV => {
            let mq = MsgqHandle(args[1] as u32);
            let cap = (args[3] as usize).min(MSGQ_MAX_MSG_SIZE);
            let buf = unsafe { core::slice::from_raw_parts_mut(args[2] as *mut u8, cap) };
            match hv.msgq_recv(mq, buf) {
                Ok((n, _ready)) => {
                    log::debug!("HVC msgq_recv({:?}) -> {} B", mq, n);
                    n as u64
                }
                Err(HvError::Empty) => tanix_hvc::ERR,
                Err(e) => {
                    log::warn!("HVC msgq_recv({:?}) failed: {:?}", mq, e);
                    tanix_hvc::ERR
                }
            }
        }

        _ => {
            log::warn!("HVC unknown func={:#x} args={:#x} {:#x} {:#x}", func, args[1], args[2], args[3]);
            tanix_hvc::ERR
        }
    }
}

/// Kernel-published entry point for the cooperative (EL1) guest: the guest
/// receives this address in the shared-memory VMM info block and calls it
/// like a hypercall (x0 = func id, x1..x3 = args).  Same dispatch as the
/// HVC trap path, so the guest ABI is identical on every backend.
#[no_mangle]
pub extern "C" fn vmm_service(arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> u64 {
    let hv = crate::hypervisor::get_backend();
    handle_hvc([arg0, arg1, arg2, arg3], hv)
}
