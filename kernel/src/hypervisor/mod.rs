#![allow(dead_code)]
//! Hypervisor abstraction layer — Phase 13: Gunyah-style object model.
//!
//! The trait models Gunyah's capability object set the way the primary VM
//! (here: the Tanix kernel) sees it:
//!
//!   * VM objects      — one per guest, created via the Resource Manager;
//!                       lifecycle: create / destroy.
//!   * vCPU objects    — a guest VM exposes one vCPU per core; `vcpu_run`
//!                       donates CPU time until the vCPU exits (IRQ,
//!                       trapped guest call, power-off).
//!   * Message queues  — Gunyah's IPC: fixed-size messages (96 B),
//!                       non-blocking send/recv with a "ready" hint.
//!                       The primary communication channel between VMs.
//!   * Doorbells       — zero-payload interruption: ring a doorbell and an
//!                       IRQ fires on the target vCPU.
//!   * Memory extents  — memory shared/donated between VMs.
//!
//! Two backends implement the trait:
//!
//!   `BareMetalBackend` — the kernel acts as its own VMM without EL2:
//!   "vCPU run" is a cooperative context switch into the guest; the guest
//!   hands control back through the kernel-provided yield function (an
//!   exit); message queues and doorbells are real in-kernel objects.  Same
//!   API contract as Gunyah, weaker isolation (no EL2 boundary).
//!
//!   `GunyahBackend` — issues the real Gunyah hypercalls (SMCCC vendor-hyp
//!   encoding, function IDs per the upstream Linux driver).  Object
//!   *creation* flows through the RM (out of scope without a Gunyah
//!   environment); the hypercall layer itself (msgq_send/recv, doorbell,
//!   vcpu_run) is implemented against the ABI.

pub mod backend;
pub mod doorbell;

use crate::mem::PhysAddr;

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HvError {
    /// The requested operation is not supported by this backend.
    NotSupported,
    /// Out of memory / no more capability slots.
    NoMemory,
    /// The capability / handle is invalid.
    InvalidHandle,
    /// The object is in the wrong state for this operation.
    BadState,
    /// A receive on an empty message queue (Gunyah `MSGQ_EMPTY`).
    Empty,
    /// A send to a full message queue (Gunyah `MSGQ_FULL`).
    Full,
    /// A message that does not fit the queue's message size (Gunyah
    /// `MSGQ_BADMSG`).
    BadMessage,
    /// Generic hypercall failure (error code embedded).
    HypercallFailed(u64),
}

/// Gunyah hypercall error codes — match the upstream Linux driver
/// (`drivers/virt/gunyah/gunyah.h`).
#[repr(u64)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GunyahError {
    Ok = 0,
    Fail = 1,
    NoMem = 2,
    BadParam = 3,
    NoSuchCap = 4,
    Denied = 5,
    Busy = 6,
    NotReady = 7,
    MsgqEmpty = 8,
    MsgqFull = 9,
    MsgqBadMsg = 10,
}

// ── Handle types ──────────────────────────────────────────────────────────────

/// Opaque handle for a guest VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmHandle(pub u32);

/// Opaque handle for a doorbell notification object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoorbellHandle(pub u32);

/// Opaque handle for a shared memory region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShmemHandle(pub u32);

/// Opaque handle for a message queue object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsgqHandle(pub u32);

// ── Message-queue limits (Gunyah ABI) ─────────────────────────────────────────

/// Maximum size of a single message-queue message, in bytes.
pub const MSGQ_MAX_MSG_SIZE: usize = 96;

/// Default queue depth for message queues created without an explicit one.
pub const MSGQ_DEFAULT_DEPTH: u32 = 16;

// ── vCPU exit reasons ─────────────────────────────────────────────────────────

/// Why a vCPU stopped running — mirrors Gunyah's `VCPU_RUN` response set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VcpuExit {
    /// vCPU stopped because an interrupt needs servicing (Gunyah
    /// `VCPU_RUN_RESP_IRQ`); the VMM may run the vCPU again afterwards.
    Irq,
    /// The guest executed a hypercall / trapped guest call (Gunyah
    /// `VCPU_RUN_RESP_TCG`).  The arguments are the guest's x0-x3.
    TrappedCall {
        func: u64,
        arg1: u64,
        arg2: u64,
        arg3: u64,
    },
    /// The guest requested power-off (Gunyah `VCPU_RUN_RESP_POWEROFF`).
    PowerOff,
    /// Bare-metal surrogate: the guest called the kernel's yield function
    /// (the EL1 cooperative stand-in for a trap exit).
    Yielded,
}

// ── VM configuration ──────────────────────────────────────────────────────────

/// Configuration passed to `Hypervisor::vm_create`.
pub struct VmConfig {
    /// Physical base address of the VM's RAM region.
    pub ram_base: PhysAddr,
    /// Size of the VM's RAM region in bytes.
    pub ram_size: usize,
    /// Guest entry point (physical address within the RAM region).
    pub entry: PhysAddr,
    /// Boot arguments delivered to the guest's first vCPU in registers:
    ///   boot[0] -> x4, boot[1] -> x5.
    pub boot: [u64; 2],
}

// ── Trait ─────────────────────────────────────────────────────────────────────

/// Backend-agnostic hypervisor interface (Gunyah-style object model).
pub trait Hypervisor {
    /// Return `true` if this backend is available on the current platform.
    fn detect() -> bool where Self: Sized;

    // ── VM objects ──────────────────────────────────────────────────────────

    /// Create a new guest VM with the given configuration.
    fn vm_create(&mut self, config: VmConfig) -> Result<VmHandle, HvError>;

    /// Destroy a VM and release its resources (fails if a vCPU is running).
    fn vm_destroy(&mut self, handle: VmHandle) -> Result<(), HvError>;

    // ── vCPU objects ────────────────────────────────────────────────────────

    /// Run vCPU `vcpu` of the VM until it exits.  Returns the exit reason.
    fn vcpu_run(&mut self, vm: VmHandle, vcpu: u32) -> Result<VcpuExit, HvError>;

    /// Stop a vCPU that is not currently running.
    fn vcpu_stop(&mut self, vm: VmHandle, vcpu: u32) -> Result<(), HvError>;

    // ── Message queues ──────────────────────────────────────────────────────

    /// Create a message queue owned by `vm` with the given depth (1..16).
    /// Both the owner VM and the primary kernel can send/recv on it.
    fn msgq_create(&mut self, vm: VmHandle, depth: u32) -> Result<MsgqHandle, HvError>;

    /// Send a message (<= `MSGQ_MAX_MSG_SIZE` bytes).  Returns `Ok(true)`
    /// if the queue still has room ("ready" hint, Gunyah `MSGQ_TX_FLAGS`).
    fn msgq_send(&mut self, mq: MsgqHandle, msg: &[u8]) -> Result<bool, HvError>;

    /// Receive a message.  Returns `(size, ready)`: the message size and
    /// whether more messages remain queued.
    fn msgq_recv(&mut self, mq: MsgqHandle, buf: &mut [u8]) -> Result<(usize, bool), HvError>;

    // ── Doorbells ───────────────────────────────────────────────────────────

    /// Register a doorbell object delivering IRQ `irq` to `owner_vm`.
    fn doorbell_create(
        &mut self,
        owner_vm: VmHandle,
        irq: u32,
    ) -> Result<DoorbellHandle, HvError>;

    /// Ring a doorbell — raises its IRQ on the target vCPU.
    fn doorbell_send(&mut self, handle: DoorbellHandle) -> Result<(), HvError>;

    /// Configure which doorbell flags are enabled / auto-acked (Gunyah
    /// `BELL_SET_MASK`).  Bare-metal delivery ignores the masks.
    fn doorbell_set_mask(
        &mut self,
        handle: DoorbellHandle,
        enable_mask: u64,
        ack_mask: u64,
    ) -> Result<(), HvError>;

    // ── Memory extents ──────────────────────────────────────────────────────

    /// Share a physical memory range with a VM (Gunyah memory extent).
    fn mem_share(&mut self, phys: PhysAddr, size: usize) -> Result<ShmemHandle, HvError>;
}

// ── Global backend selection ──────────────────────────────────────────────────

use backend::BARE;
#[cfg(feature = "gunyah")]
use backend::GunyahBackend;

/// One backend instance per implementation, initialised at build time.
///
/// IMPORTANT: these must be module-level statics (not per-function) — the
/// backend keeps mutable state (VM records, handle counters), and both the
/// boot path (`detect_backend`) and the exception path (`get_backend`) must
/// observe the *same* instance.  `BARE` lives in `backend` so the guest's
/// yield entry (which must not go through the trait object) can reach the
/// exact instance `vcpu_run` switched out of.
#[cfg(feature = "gunyah")]
static mut GUNYAH: GunyahBackend = GunyahBackend::new();

/// Backend selection: 0 = undetected, 1 = bare-metal, 2 = Gunyah.
static mut SELECTED: u8 = 0;

/// Decide which backend is present.  Run once at boot.
fn select_backend() -> u8 {
    #[cfg(feature = "gunyah")]
    if GunyahBackend::detect() {
        return 2;
    }
    1
}

/// Detect and return the appropriate backend as a trait object.
///
/// Detection order (most specific first):
///   1. Gunyah — enabled via the `gunyah` cargo feature.  Probing issues an
///      SMCCC HVC; on bare-metal EL1 an HVC is UNDEFINED, so the probe must
///      never run unless we are genuinely hosted by Gunyah.
///   2. Bare-metal — always available as a fallback; the kernel acts as its
///      own VMM using cooperative vCPU switching.
pub fn detect_backend() -> &'static mut dyn Hypervisor {
    unsafe {
        if SELECTED == 0 {
            SELECTED = select_backend();
        }
        #[cfg(feature = "gunyah")]
        if SELECTED == 2 {
            log::info!("hypervisor: Gunyah detected");
            return &mut *core::ptr::addr_of_mut!(GUNYAH);
        }
        log::info!("hypervisor: bare-metal backend selected");
        &mut *core::ptr::addr_of_mut!(BARE)
    }
}

/// Return the already-detected backend.
///
/// Used by the exception dispatcher which cannot receive the backend as a
/// parameter.  Same instance as `detect_backend` — never panics after boot.
pub fn get_backend() -> &'static mut dyn Hypervisor {
    detect_backend()
}
