//! Per-CPU state and secondary-core bring-up — Phase 11 (SMP).
//!
//! QEMU's PSCI implementation (v11.0.3, `target/arm/tcg/psci.c`) starts a
//! `CPU_ON` target at the *highest enabled* exception level — EL2 when
//! `virtualization=on` — with the requested entry address in PC and
//! `context_id` in x0 (AArch64).  Each secondary therefore re-runs the
//! kernel's EL2→EL1 drop (`secondary_entry` in main.rs), enables its own
//! MMU / vectors / GIC redistributor / timer, and then parks in its own
//! idle slot, competing for tasks on the single global runqueue.

#![allow(dead_code)]

use crate::arch::aarch64::boot;

/// Maximum number of CPUs this kernel supports (QEMU `-smp 4`).
pub const MAX_CPUS: usize = 4;

/// Stack size per secondary CPU (they never allocate anything on it —
/// the per-CPU idle context + IRQ frames).
pub const SECONDARY_STACK_SIZE: usize = 0x1_0000; // 64 KiB

/// Boot stacks for secondary CPUs (CPU 1..MAX_CPUS-1).  `secondary_entry`
/// (main.rs) picks its stack from `mpidr()`:  base + (cpu-1) * size.
///
/// Lives in BSS (zeroed by the primary before any secondary starts).
#[no_mangle]
pub static mut SECONDARY_STACKS: [u8; SECONDARY_STACK_SIZE * (MAX_CPUS - 1)] =
    [0u8; SECONDARY_STACK_SIZE * (MAX_CPUS - 1)];

/// Per-CPU runtime state.
pub struct PerCpu {
    /// Scheduler slot this CPU is currently executing (0 = boot context
    /// on CPU 0; the CPU's own idle slot on secondaries).
    pub current: usize,
    /// Brought up via PSCI CPU_ON.
    pub online: bool,
}

/// Indexed by `cpu_index()`.
pub static mut CPUS: [PerCpu; MAX_CPUS] = [
    PerCpu { current: 0, online: false },
    PerCpu { current: 0, online: false },
    PerCpu { current: 0, online: false },
    PerCpu { current: 0, online: false },
];

/// Index of the CPU currently executing this code (MPIDR Aff0).
#[inline]
pub fn cpu_index() -> usize {
    (boot::mpidr() & 0xFF) as usize
}

pub fn current_cpu() -> &'static PerCpu {
    unsafe { &*core::ptr::addr_of!(CPUS[cpu_index()]) }
}

pub fn current_cpu_mut() -> &'static mut PerCpu {
    unsafe { &mut *core::ptr::addr_of_mut!(CPUS[cpu_index()]) }
}

/// Scheduler slot this CPU is running right now.
#[inline]
pub fn current_idx() -> usize {
    current_cpu().current
}

#[inline]
pub fn set_current(idx: usize) {
    current_cpu_mut().current = idx;
}

pub fn set_online(cpu: usize) {
    unsafe {
        core::ptr::write_volatile(&raw mut CPUS[cpu].online, true);
    }
}

pub fn is_online(cpu: usize) -> bool {
    unsafe { core::ptr::read_volatile(&raw const CPUS[cpu].online) }
}

/// The scheduler slot that serves as CPU `cpu`'s idle fallback:
///   • CPU 0 → slot 0 (the kernel boot context, which halts when idle).
///   • CPU n → a dedicated per-CPU idle task slot (sched::task::IDLE_SLOT_BASE + n - 1).
pub fn idle_slot(cpu: usize) -> usize {
    if cpu == 0 {
        0
    } else {
        crate::sched::task::IDLE_SLOT_BASE + cpu - 1
    }
}

/// MPIDR value PSCI needs to address CPU `cpu` (QEMU `virt`: Aff0 only).
pub fn mpidr_for(cpu: usize) -> u64 {
    cpu as u64
}

/// Bring up the secondary cores.
///
/// 1. Allocate each secondary's idle task slot (before any secondary can
///    run scheduler code).
/// 2. Release each secondary from QEMU's PSCI power-off state: it starts
///    at EL2, drops to EL1 and idles.
///
/// Safe to call with the system still single-threaded (kmain, IRQs
/// masked); secondaries begin racing for the runqueue as soon as their
/// per-CPU init completes.
///
/// IRQs must stay masked for the whole call: `add_idle` holds SCHED_LOCK,
/// and with the preemption tick armed an interrupt taken mid-critical-
/// section would re-enter the lock on this CPU (`tick_preempt` → spinlock
/// self-deadlock).  The first task switch (`sched::enter`) restores the
/// target task's own unmasked PSTATE.
pub fn bring_up() {
    let daif: u64;
    unsafe {
        core::arch::asm!(
            "mrs {d}, daif",
            "msr daifset, #2",
            d = out(reg) daif,
            options(nomem, nostack)
        );
    }
    let _ = daif; // boot path stays masked until the first context switch

    set_online(0);

    for cpu in 1..MAX_CPUS {
        crate::sched::task::add_idle(cpu);
    }

    for cpu in 1..MAX_CPUS {
        extern "C" {
            fn secondary_entry();
        }
        let entry = secondary_entry as unsafe extern "C" fn() as usize as u64;
        let ret = crate::arch::aarch64::psci::cpu_on(mpidr_for(cpu), entry, 0);
        log::info!("smp: PSCI CPU_ON(cpu {}) → {}", cpu, ret);
        if ret == 0 {
            set_online(cpu);
        } else {
            log::warn!(
                "smp: CPU {} not available ({} — run QEMU with -smp {}?)",
                cpu, ret, MAX_CPUS
            );
        }
    }
}
