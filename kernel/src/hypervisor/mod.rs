#![allow(dead_code)]
//! Hypervisor abstraction layer.
//!
//! Phase 2 runs our kernel as the primary VM under one of two backends:
//!
//!   `Kvm`    — QEMU KVM with `-machine virt,virtualization=on`.
//!              The kernel runs at EL1 and issues HVC calls to trap into
//!              its own exception handler (acting as a simple VMM).
//!
//!   `Gunyah` — (Phase 2b / real hardware) Gunyah Type-1 hypervisor;
//!              the kernel runs as the Resource Manager VM.  Stubs only
//!              for now — the ABI will be filled in once the QEMU Gunyah
//!              fork is available.
//!
//! The `Hypervisor` trait decouples all VM-management code from the
//! underlying backend.  Higher-level code (vm::Manager, ipc::doorbell)
//! only sees the trait, so swapping backends is a one-line change in
//! `detect()`.

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
    /// The VM is in the wrong state for this operation.
    BadState,
    /// Generic hypercall failure (error code embedded).
    HypercallFailed(u64),
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

// ── VM configuration ──────────────────────────────────────────────────────────

/// Configuration passed to `Hypervisor::vm_create`.
pub struct VmConfig {
    /// Physical base address of the VM's RAM region.
    pub ram_base: PhysAddr,
    /// Size of the VM's RAM region in bytes.
    pub ram_size: usize,
    /// Guest entry point (physical address within the RAM region).
    pub entry: PhysAddr,
}

// ── Trait ─────────────────────────────────────────────────────────────────────

/// Backend-agnostic hypervisor interface.
pub trait Hypervisor {
    /// Return `true` if this backend is available on the current platform.
    fn detect() -> bool where Self: Sized;

    /// Create a new guest VM with the given configuration.
    fn vm_create(&mut self, config: VmConfig) -> Result<VmHandle, HvError>;

    /// Start (or resume) a previously created VM.
    fn vm_start(&mut self, handle: VmHandle) -> Result<(), HvError>;

    /// Stop a running VM without destroying its state.
    fn vm_stop(&mut self, handle: VmHandle) -> Result<(), HvError>;

    /// Destroy a VM and release its resources.
    fn vm_destroy(&mut self, handle: VmHandle) -> Result<(), HvError>;

    /// Ring a doorbell in the target VM — lightweight inter-VM notification.
    fn doorbell_send(&mut self, handle: DoorbellHandle) -> Result<(), HvError>;

    /// Register a shared memory region accessible by both primary and
    /// secondary VMs.
    fn mem_share(
        &mut self,
        phys: PhysAddr,
        size: usize,
    ) -> Result<ShmemHandle, HvError>;
}

// ── Global backend selection ──────────────────────────────────────────────────

use backend::BareMetalBackend;
#[cfg(feature = "gunyah")]
use backend::GunyahBackend;

/// One backend instance per implementation, initialised at build time.
///
/// IMPORTANT: these must be module-level statics (not per-function) — the
/// backend keeps mutable state (VM records, handle counters), and both the
/// boot path (`detect_backend`) and the exception path (`get_backend`) must
/// observe the *same* instance.
static mut BARE: BareMetalBackend = BareMetalBackend::new();

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
