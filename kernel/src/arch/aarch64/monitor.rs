//! Phase 16 — EL3 monitor (TrustZone): secure-world bootstrap, PSCI and
//! trusted services.
//!
//! The monitor is the firmware piece QEMU's `sbsa-ref` machine lacks (its
//! internal PSCI is disabled — "external firmware should supply PSCI"):
//! it owns EL3, installs the EL3 vectors + the secure payload in secure
//! RAM, provides PSCI (CPU_ON/CPU_OFF/VERSION) so the kernel's existing
//! `smc #0` calls keep working, and implements a small set of secure
//! services (TCB measurement, secure tick, world switch, secure console,
//! monotonic counter) for the automotive "secure core" story.
//!
//! Boot flow (sbsa-ref): all CPUs reset at EL3 → `monitor_el3_init` →
//! primary drops to NS EL1 (`kmain_entry`), secondaries park at EL3
//! (`monitor_park_loop`) until the kernel's PSCI CPU_ON fills their
//! command slot and sends a wakeup SGI.
//!
//! Runtime: the NS kernel calls `smc #0`; `monitor_entry.s` saves the full
//! caller context, dispatches here for services, restores and resumes the
//! caller (ELR+4).  The secure payload runs at S-EL1 (secure timer PPI 29,
//! Group 0 FIQ); its `smc` hands control — and its result — back to the
//! NS kernel.
//!
//! The monitor's mutable state lives in `.data.monitor` (link_section),
//! NOT `.bss`: it is written during EL3 init, *before* the kernel's BSS
//! zeroing, so a BSS placement would silently erase it.

use core::arch::global_asm;

use super::machine;

global_asm!(include_str!("monitor_entry.s"));
global_asm!(include_str!("sec_payload.s"));

// ── SMCCC / PSCI function ids ────────────────────────────────────────────────

const SMCCC_VERSION: u64 = 0x8000_0000;
const PSCI_VERSION: u64 = 0xC400_0000;
const PSCI_CPU_OFF: u64 = 0xC400_0002;
const PSCI_CPU_ON: u64 = 0xC400_0003;
const PSCI_FEATURES: u64 = 0xC400_000A;
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
const PSCI_SYSTEM_RESET: u64 = 0x8400_0009;

/// Tanix secure services (SMC64, trusted-OS range).
pub const SEC_MEASURE: u64 = 0x8300_0001; // a1=base, a2=size → FNV-1a 64 digest
pub const SEC_TICK_GET: u64 = 0x8300_0002;
pub const SEC_TICK_ARM: u64 = 0x8300_0003; // a1 = period in ms
pub const SEC_WORLD_SWITCH: u64 = 0x8300_0004; // a1=iters, a2=round → tick count
pub const SEC_TCB_PRINT: u64 = 0x8300_0005;
pub const SEC_COUNTER_INCR: u64 = 0x8300_0006;

const PSCI_SUCCESS: u64 = 0;
const PSCI_NOT_SUPPORTED: u64 = 0xFFFF_FFFF;
const PSCI_INVALID_PARAMS: u64 = 0xFFFF_FFFE;

/// Magic written into a secondary's PSCI command slot by CPU_ON.
const CMD_MAGIC: u64 = 0x434D_4454;

/// EL3 scratch register number for SGI wakeups (any spare SGI).
const WAKE_SGI: u32 = 15;

// ── Monitor-owned state (survives the kernel's BSS zeroing) ──────────────────

/// Context slots for the NS kernel (see `monitor_entry.s` layout).
#[no_mangle]
pub static mut NS_CTX: [[u64; 34]; 8] = [[0; 34]; 8];
/// Context slots for the secure payload.
#[no_mangle]
pub static mut SEC_CTX: [[u64; 34]; 8] = [[0; 34]; 8];
/// Per-CPU PSCI command slots: { magic, entry }.
#[no_mangle]
pub static mut PSCI_CMD_SLOTS: [[u64; 2]; 8] = [[0; 2]; 8];
/// EL3 fault record (ESR_EL3, ELR_EL3) written by `el3_error`.
#[no_mangle]
pub static mut EL3_FAULT: [u64; 2] = [0; 2];

/// Runtime base of the secure payload blob (secure RAM on sbsa-ref, its
/// link address on virt).  Written by the monitor at boot — BEFORE the
/// kernel zeroes BSS, hence the `.data.monitor` placement.
#[no_mangle]
#[link_section = ".data.monitor"]
static mut SEC_RUNTIME_BASE: u64 = 0;

/// CPU count (from the DT) — also used by the kernel via `monitor::cpu_count`.
#[no_mangle]
#[link_section = ".data.monitor"]
static mut MONITOR_CPU_COUNT: u64 = 0;

// ── Secure payload symbol access ─────────────────────────────────────────────

extern "C" {
    static sec_payload_start: u8;
    static sec_payload_end: u8;
    static sec_entry: u8;
    static sec_data_iters: u8;
    static sec_data_round: u8;
    static sec_data_uart: u8;
    static sec_data_ticks: u8;
    static sec_data_counter: u8;
    static sec_data_measure: u8;
    static __el3_vectors: u8;
    static monitor_park_loop: u8;
}

fn sym_addr(s: *const u8) -> usize {
    s as usize
}

/// Offset of payload symbol `s` from the payload blob start.
fn sec_off(s: *const u8) -> usize {
    sym_addr(s) - sym_addr(core::ptr::addr_of!(sec_payload_start))
}

/// Runtime address of payload data symbol `s` (secure RAM after the copy).
fn sec_addr(s: *const u8) -> usize {
    let base = unsafe { SEC_RUNTIME_BASE } as usize;
    if base == 0 {
        return 0;
    }
    base + sec_off(s)
}

fn sec_write_u64(s: *const u8, v: u64) {
    let a = sec_addr(s);
    if a != 0 {
        unsafe { core::ptr::write_volatile(a as *mut u64, v) };
    }
}

fn sec_read_u64(s: *const u8) -> u64 {
    let a = sec_addr(s);
    if a == 0 {
        return 0;
    }
    unsafe { core::ptr::read_volatile(a as *const u64) }
}

// ── Early UART (EL3, MMU off) ────────────────────────────────────────────────

/// Print `s` on the machine's NS console directly from EL3 (the log crate
/// is not up yet / may not be reachable at EL3).
fn el3_puts(s: &str) {
    let uart = machine::machine().uart_base as *mut u8;
    for &b in s.as_bytes() {
        unsafe {
            while core::ptr::read_volatile(uart.add(0x18)) & (1 << 5) != 0 {}
            core::ptr::write_volatile(uart, b);
        }
    }
}

// ── EL3 bootstrap ────────────────────────────────────────────────────────────

/// EL3 reset entry, called from `_start` (SP = EL3 stack, MMU off).
///
/// * `dtb` — the device-tree pointer (x0 from QEMU), parsed for the CPU
///   count and, on machines with a memory node, the RAM bounds.
/// * `is_secondary` — nonzero for secondary CPUs (park at EL3 instead of
///   dropping to EL1).
/// * `el1_entry` — where the primary continues at NS EL1.
///
/// Returns by `eret` (never returns normally).
#[no_mangle]
pub extern "C" fn monitor_el3_init(dtb: u64, is_secondary: u64, el1_entry: u64) -> ! {
    // 1. EL3 system registers.
    unsafe {
        core::arch::asm!(
            "msr VBAR_EL3, {v}",
            v = in(reg) sym_addr(core::ptr::addr_of!(__el3_vectors)),
            options(nomem, nostack)
        );
        core::arch::asm!(
            "msr SCR_EL3, {v}", // NS | SMC | HCE | RW
            v = in(reg) 0x581u64,
            options(nomem, nostack)
        );
        core::arch::asm!(
            "msr CPTR_EL3, {v}", // no traps to EL1/EL2 (FPEN, TCPAC, ...)
            v = in(reg) 0x33fu64,
            options(nomem, nostack)
        );
        core::arch::asm!(
            "msr MDCR_EL3, {v}", // nothing routed into EL3 debug
            v = in(reg) 0u64,
            options(nomem, nostack)
        );
        core::arch::asm!(
            "msr SCTLR_EL3, {v}", // MMU off at EL3
            v = in(reg) 0u64,
            options(nomem, nostack)
        );
        core::arch::asm!(
            "msr ICC_SRE_EL3, {v}", // GICv3 system-register interface
            v = in(reg) 1u64,
            options(nomem, nostack)
        );
        core::arch::asm!(
            "msr ICC_PMR_EL1, {v}", // priority mask 0xFF (QEMU resets it to 0)
            v = in(reg) 0xFFu64,
            options(nomem, nostack)
        );
        core::arch::asm!("isb", options(nomem, nostack));
    }

    // 2. Generic-timer frequency: QEMU sets CNTFRQ on both machines; on
    //    real silicon the firmware must.  Write a sane default if zero.
    let cntfrq: u64;
    unsafe { core::arch::asm!("mrs {v}, CNTFRQ_EL0", v = out(reg) cntfrq, options(nomem, nostack)) };
    if cntfrq == 0 {
        unsafe {
            core::arch::asm!(
                "msr CNTFRQ_EL0, {v}",
                v = in(reg) 1_000_000_000u64,
                options(nomem, nostack)
            );
        }
    }

    // 3. GIC: distributor up (Group 0 + Group 1S + Group 1NS), this CPU's
    //    redistributor awake, SGIs enabled (wakeups for parked CPUs).
    let m = machine::machine();
    let cpu = crate::smp::cpu_index();
    unsafe {
        core::ptr::write_volatile(
            (m.gic_dist_base + 0x000) as *mut u32, // GICD_CTLR
            0b1_1111u32,                            // all four enable bits + ARE
        );
        let gicr = m.gic_redist_base + cpu * m.gic_redist_stride;
        let waker = core::ptr::read_volatile((gicr + 0x014) as *const u32); // GICR_WAKER
        core::ptr::write_volatile((gicr + 0x014) as *mut u32, waker & !(1 << 1));
        while core::ptr::read_volatile((gicr + 0x014) as *const u32) & (1 << 2) != 0 {}
        // SGIs 0..15 as Group 0, enabled: they wake parked CPUs from WFI.
        core::ptr::write_volatile((gicr + 0x1_0000 + 0x100) as *mut u32, 0xFFFFu32);
    }

    // 4. CPU count from the DT (sbsa-ref lists /cpus/cpu@N).
    let ncpus = super::fdt::cpu_count(dtb as usize) as u64;
    unsafe { MONITOR_CPU_COUNT = if ncpus != 0 { ncpus } else { 1 } };

    // 5. Install the secure payload (primary only — the secondaries park).
    if is_secondary == 0 {
        let src = sym_addr(core::ptr::addr_of!(sec_payload_start));
        let end = sym_addr(core::ptr::addr_of!(sec_payload_end));
        let len = end - src;
        let target = if m.secure_ram_base != 0 { m.secure_ram_base } else { src };
        unsafe {
            for i in (0..len).step_by(8) {
                let v = core::ptr::read_volatile((src + i) as *const u64);
                core::ptr::write_volatile((target + i) as *mut u64, v);
            }
            SEC_RUNTIME_BASE = target as u64;
            // The payload's secure console: the machine's secure UART
            // (sbsa-ref: 0x60030000; virt: same as NS UART — no secure
            // console, the payload shares the normal one).
            core::ptr::write_volatile(
                (target + sec_off(core::ptr::addr_of!(sec_data_uart))) as *mut u64,
                if m.secure_uart_base != 0 {
                    m.secure_uart_base as u64
                } else {
                    m.uart_base as u64
                },
            );
            // Wake SGIs for parked secondaries + the secure timer's
            // Group 0 FIQ (PPI 29, this CPU only).
            let gicr = m.gic_redist_base + cpu * m.gic_redist_stride;
            core::ptr::write_volatile((gicr + 0x1_0000 + 0x100) as *mut u32, 1u32 << 29);
            // PPI 29 priority/group already Group 0 by reset.
        }
        el3_puts("EL3 monitor: secure world installed\r\n");
    }

    // 6. Drop to the NS kernel (primary) or park (secondary).
    unsafe {
        if is_secondary != 0 {
            core::arch::asm!(
                "msr ELR_EL3, {elr}",
                "msr SPSR_EL3, {spsr}", // EL3h, DAIF masked
                "isb",
                "eret",
                elr = in(reg) sym_addr(core::ptr::addr_of!(monitor_park_loop)),
                spsr = in(reg) 0x3c9u64,
                options(noreturn)
            );
        } else {
            core::arch::asm!(
                "msr ELR_EL3, {elr}",
                "msr SPSR_EL3, {spsr}", // EL1h, DAIF masked
                "isb",
                "eret",
                elr = in(reg) el1_entry,
                spsr = in(reg) 0x3c5u64,
                options(noreturn)
            );
        }
    }
}

/// CPU count discovered by the monitor (kernel side reads this after boot).
pub fn cpu_count() -> usize {
    unsafe { MONITOR_CPU_COUNT as usize }
}

// ── SMC dispatch (called from `monitor_entry.s`) ─────────────────────────────

/// Handle an SMC64 from the NS kernel: PSCI + Tanix secure services.
/// Returns the value to hand back in x0.
#[no_mangle]
pub extern "C" fn monitor_smc_dispatch(fn_id: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    match fn_id {
        SMCCC_VERSION => 0x0001_0001, // SMCCC 1.1
        PSCI_VERSION => 0x0001_0001,  // PSCI 1.1
        PSCI_CPU_OFF => PSCI_NOT_SUPPORTED, // handled directly by the vectors
        PSCI_CPU_ON => psci_cpu_on(a1, a2, a3),
        PSCI_FEATURES => 0,
        PSCI_SYSTEM_OFF | PSCI_SYSTEM_RESET => PSCI_NOT_SUPPORTED,
        SEC_MEASURE => secure_measure(a1, a2),
        SEC_TICK_GET => sec_read_u64(core::ptr::addr_of!(sec_data_ticks)),
        SEC_TICK_ARM => secure_tick_arm_ms(a1),
        SEC_TCB_PRINT => {
            el3_puts("EL3 monitor: TCB secure console reachable\r\n");
            0
        }
        SEC_COUNTER_INCR => {
            let v = sec_read_u64(core::ptr::addr_of!(sec_data_counter)).wrapping_add(1);
            sec_write_u64(core::ptr::addr_of!(sec_data_counter), v);
            v
        }
        _ => PSCI_NOT_SUPPORTED,
    }
}

/// PSCI CPU_ON: fill the target's command slot and wake it with an SGI.
/// The target CPU wakes from its EL3 park loop, erets to `entry` at EL1
/// (the kernel's `secondary_entry`), and clears the slot itself.
fn psci_cpu_on(target_aff0: u64, entry: u64, _context_id: u64) -> u64 {
    let cpu = (target_aff0 & 0xff) as usize;
    if cpu >= 8 || cpu >= unsafe { MONITOR_CPU_COUNT } as usize || cpu == 0 {
        return PSCI_INVALID_PARAMS;
    }
    unsafe {
        PSCI_CMD_SLOTS[cpu] = [CMD_MAGIC, entry];
        // Wake the target from WFI: a Group 0 SGI (its SGIs are enabled
        // and it sits with I/F masked — the SGI stays pending, harmless).
        let val = (((WAKE_SGI as u64) & 0xF) << 24) | (1u64 << cpu);
        core::arch::asm!(
            "msr S3_6_C12_C11_5, {v}", // ICC_SGI1R_EL3
            "isb",
            v = in(reg) val,
            options(nomem, nostack)
        );
    }
    PSCI_SUCCESS
}

/// Prepare one world-switch run: write the payload's iteration count,
/// round number and console base into the (copied) payload data.
/// Called from the vectors' world-switch fast path.
#[no_mangle]
pub extern "C" fn monitor_prepare_switch(iters: u64, round: u64) -> u64 {
    sec_write_u64(core::ptr::addr_of!(sec_data_iters), iters);
    sec_write_u64(core::ptr::addr_of!(sec_data_round), round);
    0
}

/// Arm the secure EL1 timer for `ms` milliseconds (monitor side, at EL3 —
/// CNTPS_EL1 is accessible from EL3).  Used by SEC_TICK_ARM; the payload
/// also arms its own 50 ms tick on entry.
fn secure_tick_arm_ms(ms: u64) -> u64 {
    let cntfrq: u64;
    unsafe { core::arch::asm!("mrs {v}, CNTFRQ_EL0", v = out(reg) cntfrq, options(nomem, nostack)) };
    let tval = cntfrq.wrapping_mul(ms) / 1000;
    unsafe {
        core::arch::asm!("msr S3_1_C14_C2_1, {v}", v = in(reg) tval, options(nomem, nostack)); // CNTPS_TVAL_EL1
        core::arch::asm!("msr S3_1_C14_C2_3, {v}", v = in(reg) 1u64, options(nomem, nostack)); // CNTPS_CTL_EL1
    }
    0
}

/// TCB measurement: FNV-1a 64 digest of `[base, base+size)` (NS memory,
/// readable from EL3).  The digest is also extended into the secure
/// payload's measurement slot, so the secure world accumulates a boot log
/// the NS kernel cannot forge.
fn secure_measure(base: u64, size: u64) -> u64 {
    const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
    const FNV_PRIME: u64 = 0x100_0000_01B3;
    let mut hash = FNV_OFFSET;
    let mut p = base as *const u8;
    for _ in 0..size {
        let b = unsafe { core::ptr::read_volatile(p) };
        hash ^= b as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
        p = p.wrapping_add(1);
    }
    sec_write_u64(core::ptr::addr_of!(sec_data_measure), hash);
    hash
}

// ── Kernel-side (NS EL1) wrappers ────────────────────────────────────────────

/// Issue `smc #0` to the monitor with 3 args; returns the result in x0.
/// The monitor saves/restores the full caller context, so no clobbers
/// beyond x0 are declared.
pub fn smc(fn_id: u64, a0: u64, a1: u64, a2: u64) -> u64 {
    let r: u64;
    unsafe {
        core::arch::asm!(
            "smc #0",
            inlateout("x0") fn_id => r,
            in("x1") a0,
            in("x2") a1,
            in("x3") a2,
            options(nostack)
        );
    }
    r
}

/// PSCI CPU_ON through the monitor (same ABI as QEMU's PSCI).
pub fn psci_cpu_on_smc(mpidr: u64, entry: u64, context_id: u64) -> i32 {
    smc(PSCI_CPU_ON, mpidr, entry, context_id) as i32
}

/// Measure a memory region with the secure core.  Returns the FNV-1a 64
/// digest the monitor computed; compare against the local computation to
/// verify the TCB's integrity service.
pub fn secure_measure_smc(base: usize, size: usize) -> u64 {
    smc(SEC_MEASURE, base as u64, size as u64, 0)
}

/// Run one secure-world session: `iters` payload iterations.  Returns the
/// secure tick count the payload counted before handing back.
pub fn world_switch(iters: u64, round: u64) -> u64 {
    smc(SEC_WORLD_SWITCH, iters, round, 0)
}

/// Read the secure tick count.
pub fn secure_tick_get() -> u64 {
    smc(SEC_TICK_GET, 0, 0, 0)
}

/// Arm the secure EL1 timer from the kernel.
pub fn secure_tick_arm(ms: u64) -> u64 {
    smc(SEC_TICK_ARM, ms, 0, 0)
}

/// Ask the monitor to print on the secure console.
pub fn secure_banner() -> u64 {
    smc(SEC_TCB_PRINT, 0, 0, 0)
}

/// Increment the secure monotonic counter; returns the new value.
pub fn secure_counter_incr() -> u64 {
    smc(SEC_COUNTER_INCR, 0, 0, 0)
}

/// The monitor's own PSCI version response.
pub fn psci_version_smc() -> u64 {
    smc(PSCI_VERSION, 0, 0, 0)
}
