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

// Phase 17 — secure services (storage / keybox) + attestation.
//
// ABI notes: the monitor runs with the MMU off — every pointer argument
// is a *physical* address (the kernel's identity map makes VA == phys, so
// NS callers pass their normal pointers).  Buffer sizes are enforced in
// the monitor; the data itself is only ever read/written with volatile
// accesses (real silicon would need cache maintenance around these
// copies — QEMU's memory model is coherent).

/// Secure storage: write/overwrite blob `name` ← `data`.  `name` is an
/// 8-byte key, `data` ≤ 232 B (copy happens inside the secure world).
/// a1=name ptr, a2=data ptr, a3=len → 0 | -1.
pub const SEC_STORAGE_PUT: u64 = 0x8300_0007;
/// Secure storage: read blob `name` into `out` (≤ `cap` bytes).
/// a1=name ptr, a2=out ptr, a3=cap → stored length | -1 (missing) | -2 (too small).
pub const SEC_STORAGE_GET: u64 = 0x8300_0008;
/// Keybox: generate a new key in slot `a1` (16 bytes from the secure
/// RNG).  Keys never leave the secure world.  → 0 | -1.
pub const SEC_KEYBOX_GEN: u64 = 0x8300_0009;
/// Keybox: seal `a3` bytes at `a2` in place with key `a1` (XTEA-CFB,
/// IV = the key's secure use counter).  → 0 | -1.
pub const SEC_KEYBOX_SEAL: u64 = 0x8300_000A;
/// Keybox: unseal `a3` bytes at `a2` in place with key `a1` (inverse of
/// SEAL).  → 0 | -1.
pub const SEC_KEYBOX_UNSEAL: u64 = 0x8300_000B;
/// Attestation quote for `[a1, a1+a2)`: FNV-1a digest + keyed MAC over
/// (secret ‖ nonce ‖ digest), written as two u64s to `a4` (≤ `a5` bytes).
/// The secret is EL3-only, so only the secure world can produce a valid
/// quote and the nonce binds it to a challenge.  → 0 | -1.
pub const SEC_ATTEST: u64 = 0x8300_000C;

const PSCI_SUCCESS: u64 = 0;
const PSCI_NOT_SUPPORTED: u64 = 0xFFFF_FFFF;
const PSCI_INVALID_PARAMS: u64 = 0xFFFF_FFFE;

/// Magic written into a secondary's PSCI command slot by CPU_ON.
const CMD_MAGIC: u64 = 0x434D_4454;

/// EL3 scratch register number for SGI wakeups (any spare SGI).
const WAKE_SGI: u32 = 15;

// ── Phase 17 secure-service layout (mirrors sec_payload.s) ──────────────────

/// Secure-storage slot count / size (32 × 256 B at +0x8000 of the blob).
const STORE_SLOTS: usize = 32;
const STORE_SLOT_SIZE: usize = 256;
/// Slot magic ("STOR") written when a slot is occupied.
const STORE_MAGIC: u64 = 0x5354_4F52;
/// Longest stored blob (slot → 8 name + 8 len + data).
const STORE_DATA_MAX: usize = 232;
/// Largest blob sealed/unsealed with one keybox call.
const SEAL_MAX: usize = 256;
/// Keybox capacity (16 × 32 B at +0xA000 of the blob).
const KEYBOX_KEYS: usize = 16;
/// Attestation quote output (digest + MAC).
const ATTEST_OUT_WORDS: usize = 2;
/// Cap the measured region (a wild NS length would spin EL3 for ages).
const ATTEST_REGION_MAX: u64 = 16 * 1024 * 1024;

// ── Monitor-owned state (survives the kernel's BSS zeroing) ──────────────────

/// Context slots for the NS kernel (see `monitor_entry.s` layout).
#[no_mangle]
pub static mut __ns_ctx: [[u64; 34]; 8] = [[0; 34]; 8];
/// Context slots for the secure payload.
#[no_mangle]
pub static mut __sec_ctx: [[u64; 34]; 8] = [[0; 34]; 8];
/// Per-CPU PSCI command slots: { magic, entry }.
#[no_mangle]
pub static mut __psci_cmd_slots: [[u64; 2]; 8] = [[0; 2]; 8];
/// EL3 fault record (ESR_EL3, ELR_EL3) written by `el3_error`.
#[no_mangle]
pub static mut __el3_fault: [u64; 2] = [0; 2];
/// NS EL1 state parked by `enter_secure` and restored on the payload's
/// return — the secure world runs with the EL1 MMU off and its own
/// vector table: [0] SCTLR_EL1, [2] VBAR_EL1.
#[no_mangle]
pub static mut __el3_saved_sctlr_el1: [u64; 3] = [0; 3];

/// Runtime base of the secure payload blob (secure RAM on sbsa-ref, its
/// link address on virt).  Written by the monitor at boot — BEFORE the
/// kernel zeroes BSS, hence the `.data.monitor` placement.
#[no_mangle]
#[link_section = ".data.monitor"]
static mut __sec_runtime_base: u64 = 0;

/// CPU count (from the DT) — also used by the kernel via `monitor::cpu_count`.
#[no_mangle]
#[link_section = ".data.monitor"]
static mut __monitor_cpu_count: u64 = 0;

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
    static sec_data_store: u8;
    static sec_data_keybox: u8;
    static sec_data_secret: u8;
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
    let base = unsafe { __sec_runtime_base } as usize;
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

/// Debug: print a hex value on the NS console from EL3 (temporary).
fn el3_hex(v: u64) {
    let mut buf = [0u8; 18];
    buf[0] = b'0';
    buf[1] = b'x';
    for i in 0..16 {
        let nib = ((v >> (60 - i * 4)) & 0xF) as u8;
        buf[2 + i] = if nib < 10 { b'0' + nib } else { b'a' + nib - 10 };
    }
    el3_puts(core::str::from_utf8(&buf).unwrap());
}

/// Debug (temporary): dump the just-saved SMC context from the vectors.
/// x0 = &ctx (272-byte slot, x0..x30 @ 0..240, sp_el1 @ 248, elr @ 256,
/// spsr @ 264).
#[no_mangle]
pub extern "C" fn el3_dbg_ctx(ctx: *const u64) {
    unsafe {
        el3_puts("EL3 SMC fn=");
        el3_hex(core::ptr::read_volatile(ctx));
        el3_puts(" a1=");
        el3_hex(core::ptr::read_volatile(ctx.add(1)));
        el3_puts(" a2=");
        el3_hex(core::ptr::read_volatile(ctx.add(2)));
        el3_puts(" a3=");
        el3_hex(core::ptr::read_volatile(ctx.add(3)));
        el3_puts(" x30=");
        el3_hex(core::ptr::read_volatile(ctx.add(30)));
        el3_puts(" elr=");
        el3_hex(core::ptr::read_volatile(ctx.add(32)));
        el3_puts(" sps=");
        el3_hex(core::ptr::read_volatile(ctx.add(33)));
        el3_puts(" x8=");
        el3_hex(core::ptr::read_volatile(ctx.add(8)));
        el3_puts(" x9=");
        el3_hex(core::ptr::read_volatile(ctx.add(9)));
        el3_puts(" x10=");
        el3_hex(core::ptr::read_volatile(ctx.add(10)));
        el3_puts(" x21=");
        el3_hex(core::ptr::read_volatile(ctx.add(21)));
        el3_puts("\r\n");
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
            "msr SCR_EL3, {v}", // NS | HCE | RW — SMD kept 0 so SMC traps
            v = in(reg) 0x501u64,
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
    //
    //    IMPORTANT (sbsa-ref, QEMU GICv3 with security extensions, DS=0):
    //    the SGI/PPI bank (GICR_IGROUPR0 / NSACR) is secure-only — NS
    //    writes to GICR_IGROUPR0 are ignored, NS ISENABLER0 writes are
    //    masked by the group bits, and NS ICC_SGI1R writes are dropped
    //    unless GICR_NSACR grants access.  Everything the NS kernel's
    //    `gic::init` does to the redistributor silently no-ops here (it
    //    works on `virt`, whose GICv3 has DS=1).  So this monitor is the
    //    ONLY place the bank can be configured for the NS kernel:
    //      * SGIs 0..14  → Group 1NS + enabled (kernel IPIs/doorbells)
    //      * PPI 30      → Group 1NS + enabled (NS EL1 preemption tick)
    //      * SGI 15      → stays Group 0 (this monitor's WFI wakeup)
    //      * PPI 29      → stays Group 0 (secure payload secure timer)
    //      * NSACR       → all SGIs fully NS-accessible (SGI generation
    //                      from NS requires NSACR >= 0b10 per SGI)
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
        // GICR_IGROUPR0: SGIs 0..14 + PPI 30 → Group 1NS; SGI 15, PPI 29
        // stay Group 0 (secure).  0x4000_7FFF = bits 0..14 | bit 30.
        core::ptr::write_volatile((gicr + 0x1_0000 + 0x080) as *mut u32, 0x4000_7FFFu32);
        // GICR_NSACR: grant NS access to all SGIs (2 bits per SGI).
        core::ptr::write_volatile((gicr + 0x1_0000 + 0x0E00) as *mut u32, 0xFFFF_FFFFu32);
        // GICR_ISENABLER0: enable SGIs 0..15, PPI 29, PPI 30 (set-bitmap
        // semantics — 1s set, 0s ignored).
        core::ptr::write_volatile(
            (gicr + 0x1_0000 + 0x100) as *mut u32,
            0xFFFFu32 | (1 << 29) | (1 << 30),
        );
    }

    // 4. CPU count from the DT (sbsa-ref lists /cpus/cpu@N).
    let ncpus = super::fdt::cpu_count(dtb as usize) as u64;
    unsafe { __monitor_cpu_count = if ncpus != 0 { ncpus } else { 1 } };

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
            __sec_runtime_base = target as u64;
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

            // Phase 17: mint the attestation secret INSIDE the secure
            // world (timer entropy + a per-install mix), and zero the
            // storage/keybox regions so repeated boots never inherit
            // stale slots (the image itself is zeroed, this is defensive
            // against a hostile NS boot image).
            let cntpct: u64;
            core::arch::asm!("mrs {v}, CNTPCT_EL0", v = out(reg) cntpct, options(nomem, nostack));
            let secret = splitmix64(cntpct ^ 0x9E37_79B9_7F4A_7C15)
                .rotate_left(17)
                .wrapping_add(0x5BEE_6EFE_9408_A19C);
            sec_write_u64(core::ptr::addr_of!(sec_data_secret), secret);
            let store = sec_addr(core::ptr::addr_of!(sec_data_store));
            if store != 0 {
                unsafe {
                    core::ptr::write_bytes(store as *mut u8, 0, STORE_SLOTS * STORE_SLOT_SIZE);
                }
            }
            let kbox = sec_addr(core::ptr::addr_of!(sec_data_keybox));
            if kbox != 0 {
                unsafe {
                    core::ptr::write_bytes(kbox as *mut u8, 0, KEYBOX_KEYS * 32);
                }
            }
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
            // Primary: EL3 → EL2 → EL1.  The direct EL3→EL1 `eret` does
            // not work on this QEMU (the target never leaves EL3), while
            // the EL3→EL2 leg demonstrably does (the parked secondaries
            // take exactly that eret).  Cascading through EL2 also mirrors
            // the `virt` boot path (EL3→EL2→EL1 in `_start`), so the EL1
            // entry state here is identical to the known-good one.
            core::arch::asm!(
                "adr x11, 1f",
                "msr ELR_EL3, x11",
                "msr SPSR_EL3, {spsr3}", // EL2h, DAIF masked
                "isb",
                "eret",
"1:",
                "mov x10, #1",
                "lsl x10, x10, #31",
                "msr HCR_EL2, x10",    // RW=1: force AArch64 at EL1
                "msr SCTLR_EL2, xzr",
                "isb",
                "msr ELR_EL2, {elr}",
                "mov x10, {spsr1}",
                "msr SPSR_EL2, x10",   // EL1h, DAIF masked
                "mov x1, xzr",         // Phase 18: `_start` 1: stashes x1 as
                                       // the EFI system table — this is a
                                       // `-kernel` boot, so hand over 0.
                "isb",
                "eret",
                elr = in(reg) el1_entry,
                spsr3 = in(reg) 0x3c9u64,
                spsr1 = in(reg) 0x3c5u64,
                options(noreturn)
            );
        }
    }
}

/// CPU count discovered by the monitor (kernel side reads this after boot).
pub fn cpu_count() -> usize {
    unsafe { __monitor_cpu_count as usize }
}

// ── SMC dispatch (called from `monitor_entry.s`) ─────────────────────────────

// ── Phase 17 crypto primitives (my-free-impl, demo grade) ────────────────────

/// FNV-1a-64 over `[base, base+size)` — the kernel's shared measurement
/// primitive (deterministic, comparable across worlds).
fn fnv1a64(base: u64, size: u64) -> u64 {
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
    hash
}

/// Extend `hash` with one u64 (little-endian bytes) — used to chain
/// measurement components into a keyed MAC.
fn fnv1a_extend(hash: u64, word: u64) -> u64 {
    const FNV_PRIME: u64 = 0x100_0000_01B3;
    let mut h = hash;
    for b in word.to_le_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// splitmix64 PRNG step — small, fast, good-enough for a demo keybox.
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// EL3-visible entropy source: the physical counter clocked at boot
/// (never exposed to the NS world as a readable monotonic either).
fn el3_entropy() -> u64 {
    let cntpct: u64;
    unsafe { core::arch::asm!("mrs {v}, CNTPCT_EL0", v = out(reg) cntpct, options(nomem, nostack)) };
    cntpct
}

/// One XTEA block encryption (64-bit block, 128-bit key, 32 Feistel
/// rounds).  Standard XTEA; enough for a demo keybox whose whole point is
/// that the key never leaves the secure world.
fn xtea_enc_block(key: &[u32; 4], block: &mut [u8; 8]) {
    let mut v0 = u32::from_le_bytes([block[0], block[1], block[2], block[3]]);
    let mut v1 = u32::from_le_bytes([block[4], block[5], block[6], block[7]]);
    let mut sum: u32 = 0;
    for _ in 0..32 {
        v0 = v0.wrapping_add((((v1 << 4) ^ (v1 >> 5)).wrapping_add(v1)) ^ (sum.wrapping_add(key[(sum & 3) as usize])));
        sum = sum.wrapping_add(0x9E37_79B9);
        v1 = v1.wrapping_add((((v0 << 4) ^ (v0 >> 5)).wrapping_add(v0)) ^ (sum.wrapping_add(key[((sum >> 11) & 3) as usize])));
    }
    block.copy_from_slice(&[
        v0 as u8, (v0 >> 8) as u8, (v0 >> 16) as u8, (v0 >> 24) as u8,
        v1 as u8, (v1 >> 8) as u8, (v1 >> 16) as u8, (v1 >> 24) as u8,
    ]);
}

/// XTEA-CFB: confidentiality + length preservation, any length.  The
/// running state starts at `iv` and steps to the *ciphertext* per block
/// (CFB is self-synchronizing; the keybox stores the IV so seal/unseal
/// agree without ever exposing the key).  `encrypt=false` inverts it.
fn xtea_cfb(key: &[u32; 4], iv: u64, buf: &mut [u8], encrypt: bool) {
    let mut state = iv;
    let mut off = 0;
    while off < buf.len() {
        let mut e = [0u8; 8];
        e.copy_from_slice(&state.to_le_bytes());
        xtea_enc_block(key, &mut e);
        let avail = core::cmp::min(8, buf.len() - off);
        let mut c = state;
        let mut cbytes = [0u8; 8];
        cbytes.copy_from_slice(&c.to_le_bytes());
        if encrypt {
            for i in 0..avail {
                cbytes[i] = buf[off + i] ^ e[i];
            }
            buf[off..off + avail].copy_from_slice(&cbytes[..avail]);
        } else {
            for i in 0..avail {
                let t = buf[off + i];
                buf[off + i] = t ^ e[i];
                cbytes[i] = t;
            }
        }
        state = u64::from_le_bytes(cbytes);
        off += avail;
    }
}

// ── Phase 17 secure services ─────────────────────────────────────────────────

/// Copy an 8-byte name from NS memory into a u64 (0-padded).
unsafe fn read_name64(ptr: u64) -> u64 {
    let mut n = [0u8; 8];
    for (i, b) in n.iter_mut().enumerate() {
        *b = core::ptr::read_volatile((ptr + i as u64) as *const u8);
    }
    u64::from_le_bytes(n)
}

/// Storage PUT: find the slot named `name` (or the first free one) and
/// copy `data` in.  All copies happen inside the EL3/secure world; the NS
/// caller only ever supplies plaintext in and gets nothing back.
fn sec_svc_storage_put(name_ptr: u64, data_ptr: u64, len: u64) -> u64 {
    let store = sec_addr(core::ptr::addr_of!(sec_data_store));
    if store == 0 || len > STORE_DATA_MAX as u64 {
        return !0; // 0xFFFF_FFFF_FFFF_FFFF = -1
    }
    let name = unsafe { read_name64(name_ptr) };
    let mut target: Option<usize> = None;
    let mut free: Option<usize> = None;
    for i in 0..STORE_SLOTS {
        let slot = store + i * STORE_SLOT_SIZE;
        let magic = unsafe {
            core::ptr::read_volatile(slot as *const u64)
        };
        if magic == STORE_MAGIC {
            let n = unsafe { core::ptr::read_volatile((slot + 8) as *const u64) };
            if n == name {
                target = Some(i);
                break;
            }
        } else if free.is_none() {
            free = Some(i);
        }
    }
    let idx = target.or(free);
    let Some(idx) = idx else { return !0 };
    let slot = store + idx * STORE_SLOT_SIZE;
    unsafe {
        core::ptr::write_volatile(slot as *mut u64, STORE_MAGIC);
        core::ptr::write_volatile((slot + 8) as *mut u64, name);
        core::ptr::write_volatile((slot + 16) as *mut u64, len);
        for i in 0..len as usize {
            let b = core::ptr::read_volatile((data_ptr + i as u64) as *const u8);
            core::ptr::write_volatile((slot + 24 + i) as *mut u8, b);
        }
    }
    0
}

/// Storage GET: copy the named blob into the NS buffer (≤ cap).  Returns
/// the stored length on success, -1 if absent, -2 if the buffer is too
/// small (caller re-runs with a bigger one).
fn sec_svc_storage_get(name_ptr: u64, out_ptr: u64, cap: u64) -> u64 {
    let store = sec_addr(core::ptr::addr_of!(sec_data_store));
    if store == 0 {
        return !0;
    }
    let name = unsafe { read_name64(name_ptr) };
    for i in 0..STORE_SLOTS {
        let slot = store + i * STORE_SLOT_SIZE;
        let magic = unsafe { core::ptr::read_volatile(slot as *const u64) };
        if magic != STORE_MAGIC {
            continue;
        }
        let n = unsafe { core::ptr::read_volatile((slot + 8) as *const u64) };
        if n != name {
            continue;
        }
        let len = unsafe { core::ptr::read_volatile((slot + 16) as *const u64) };
        if len > cap {
            return 0xFFFF_FFFF_FFFF_FFFE; // -2
        }
        for j in 0..len as usize {
            let b = unsafe { core::ptr::read_volatile((slot + 24 + j) as *const u8) };
            unsafe { core::ptr::write_volatile((out_ptr + j as u64) as *mut u8, b) };
        }
        return len;
    }
    !0 // -1
}

/// Keybox slots: base + i*32 → { valid u64, key0 u64, key1 u64, ctr u64 }.
fn keybox_base() -> usize {
    sec_addr(core::ptr::addr_of!(sec_data_keybox))
}

/// Generate a 16-byte key in slot `id` from EL3 timer entropy.  The key
/// is written only into the (secure) keybox region and returned never.
fn secure_keybox_gen(id: u64) -> u64 {
    let kbox = keybox_base();
    if kbox == 0 || id >= KEYBOX_KEYS as u64 {
        return !0;
    }
    let mut rng = splitmix64(el3_entropy()) ^ secure_secret();
    let slot = kbox + id as usize * 32;
    let k0 = splitmix64(rng);
    rng = splitmix64(rng);
    let k1 = splitmix64(rng);
    unsafe {
        core::ptr::write_volatile(slot as *mut u64, 1); // valid
        core::ptr::write_volatile((slot + 8) as *mut u64, k0);
        core::ptr::write_volatile((slot + 16) as *mut u64, k1);
        core::ptr::write_volatile((slot + 24) as *mut u64, 0); // ctr
    }
    0
}

fn secure_secret() -> u64 {
    let a = sec_addr(core::ptr::addr_of!(sec_data_secret));
    if a == 0 {
        return 0;
    }
    unsafe { core::ptr::read_volatile(a as *const u64) }
}

fn keybox_slot(id: u64) -> Option<*mut u8> {
    let kbox = keybox_base();
    if kbox == 0 || id >= KEYBOX_KEYS as u64 {
        return None;
    }
    let slot = (kbox + id as usize * 32) as *mut u8;
    let valid = unsafe { core::ptr::read_volatile(slot as *const u64) };
    if valid != 1 {
        return None;
    }
    Some(slot)
}

fn keybox_cipher(id: u64, buf: *mut u8, len: u64, encrypt: bool) -> u64 {
    if len as usize > SEAL_MAX {
        return !0;
    }
    let Some(slot) = keybox_slot(id) else { return !0 };
    let k0 = unsafe { core::ptr::read_volatile((slot as usize + 8) as *const u64) };
    let k1 = unsafe { core::ptr::read_volatile((slot as usize + 16) as *const u64) };
    let mut ctr = unsafe { core::ptr::read_volatile((slot as usize + 24) as *const u64) };
    // The IV/counter advances only on success, so a failed call never
    // desynchronizes the key's stream.
    if ctr == <u64>::MAX {
        return !0; // wrap-to-zero would reuse the stream
    }
    let mut data = [0u8; SEAL_MAX];
    for i in 0..len as usize {
        data[i] = unsafe { core::ptr::read_volatile((buf as usize + i) as *const u8) };
    }
    let key = [k0 as u32, (k0 >> 32) as u32, k1 as u32, (k1 >> 32) as u32];
    xtea_cfb(&key, ctr, &mut data[..len as usize], encrypt);
    for i in 0..len as usize {
        unsafe { core::ptr::write_volatile((buf as usize + i) as *mut u8, data[i]) };
    }
    ctr += 1;
    unsafe { core::ptr::write_volatile((slot as usize + 24) as *const u64 as *mut u64, ctr) };
    0
}

/// Attestation quote: digest `[base, base+size)`, then key it with the
/// EL3-only secret and the caller's nonce into out[0]=digest, out[1]=mac.
fn secure_attest(base: u64, size: u64, nonce: u64, out_ptr: u64, cap: u64) -> u64 {
    if cap < (ATTEST_OUT_WORDS * 8) as u64 || size > ATTEST_REGION_MAX {
        return !0;
    }
    let digest = fnv1a64(base, size);
    // MAC = FNV over (secret ‖ nonce ‖ digest) — keyed, nonce-bound.
    let mut mac = fnv1a64(0, 0);
    mac = fnv1a_extend(mac, secure_secret());
    mac = fnv1a_extend(mac, nonce);
    mac = fnv1a_extend(mac, digest);
    unsafe {
        core::ptr::write_volatile(out_ptr as *mut u64, digest);
        core::ptr::write_volatile((out_ptr + 8) as *mut u64, mac);
    }
    0
}

/// Handle an SMC64 from the NS kernel: PSCI + Tanix secure services.
/// Returns the value to hand back in x0.
#[no_mangle]
pub extern "C" fn monitor_smc_dispatch(fn_id: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> u64 {
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
        SEC_STORAGE_PUT => sec_svc_storage_put(a1, a2, a3),
        SEC_STORAGE_GET => sec_svc_storage_get(a1, a2, a3),
        SEC_KEYBOX_GEN => secure_keybox_gen(a1),
        SEC_KEYBOX_SEAL => keybox_cipher(a1, a2 as *mut u8, a3, true),
        SEC_KEYBOX_UNSEAL => keybox_cipher(a1, a2 as *mut u8, a3, false),
        SEC_ATTEST => secure_attest(a1, a2, a3, a4, a5),
        _ => PSCI_NOT_SUPPORTED,
    }
}

/// PSCI CPU_ON: fill the target's command slot and wake it with an SGI.
/// The target CPU wakes from its EL3 park loop, erets to `entry` at EL1
/// (the kernel's `secondary_entry`), and clears the slot itself.
fn psci_cpu_on(target_aff0: u64, entry: u64, _context_id: u64) -> u64 {
    let cpu = (target_aff0 & 0xff) as usize;
    if cpu >= 8 || cpu >= unsafe { __monitor_cpu_count } as usize || cpu == 0 {
        return PSCI_INVALID_PARAMS;
    }
    unsafe {
        __psci_cmd_slots[cpu] = [CMD_MAGIC, entry];
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
    let hash = fnv1a64(base, size);
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

/// `smc` with two extra arguments (x4, x5) — the Phase-17 services
/// (attestation) pass an output buffer this way.
pub fn smc6(fn_id: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    let r: u64;
    unsafe {
        core::arch::asm!(
            "smc #0",
            inlateout("x0") fn_id => r,
            in("x1") a0,
            in("x2") a1,
            in("x3") a2,
            in("x4") a3,
            in("x5") a4,
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

// ── Phase 17: kernel side of the secure services ─────────────────────────────

/// `secure_storage_put`: hand `data` (≤ 232 B) to the secure store under
/// the 8-byte `name`.  Returns 0, or -1 on failure.  The plaintext is
/// copied into secure RAM by the monitor.
///
/// # Safety
/// `name` / `data` must point to `name.len()` / `data.len()` readable
/// bytes (identity-mapped NS memory).
pub unsafe fn secure_storage_put(name: &[u8; 8], data: &[u8]) -> i64 {
    if data.len() > 232 {
        return -1;
    }
    smc(
        SEC_STORAGE_PUT,
        name.as_ptr() as u64,
        data.as_ptr() as u64,
        data.len() as u64,
    ) as i64
}

/// `secure_storage_get`: read the blob stored under `name` into `out`.
/// Returns the stored length, -1 if absent, -2 if `out` is too small.
///
/// # Safety
/// `out` must be `out.len()` writable bytes.
pub unsafe fn secure_storage_get(name: &[u8; 8], out: &mut [u8]) -> i64 {
    smc(
        SEC_STORAGE_GET,
        name.as_ptr() as u64,
        out.as_mut_ptr() as u64,
        out.len() as u64,
    ) as i64
}

/// `keybox`: generate a 16-byte key in slot `key_id` inside the secure
/// world.  The key is never exported.  Returns 0, or -1.
pub fn keybox_gen(key_id: u64) -> i64 {
    smc(SEC_KEYBOX_GEN, key_id, 0, 0) as i64
}

/// `keybox`: seal `buf` in place with key `key_id` (XTEA-CFB inside the
/// monitor).  Returns 0, or -1 (unknown key / too large / counter wrap).
///
/// # Safety
/// `buf` must be `buf.len()` readable+writable bytes.
pub unsafe fn keybox_seal(key_id: u64, buf: &mut [u8]) -> i64 {
    smc(SEC_KEYBOX_SEAL, key_id, buf.as_mut_ptr() as u64, buf.len() as u64) as i64
}

/// `keybox`: inverse of `keybox_seal`.
///
/// # Safety
/// Same as `keybox_seal`.
pub unsafe fn keybox_unseal(key_id: u64, buf: &mut [u8]) -> i64 {
    smc(SEC_KEYBOX_UNSEAL, key_id, buf.as_mut_ptr() as u64, buf.len() as u64) as i64
}

/// `attest`: EL3 quote for the image `[base, base+size)` bound to `nonce`.
/// On success `out[0]` = FNV-1a digest and `out[1]` = keyed MAC over
/// (secret ‖ nonce ‖ digest) — a verifier holding the secret rejects any
/// forged/foreign image.  Returns 0, or -1.
///
/// # Safety
/// `out` must be 16 writable bytes; `base..base+size` readable NS memory.
pub unsafe fn attest(base: usize, size: usize, nonce: u64, out: &mut [u64; 2]) -> i64 {
    smc6(
        SEC_ATTEST,
        base as u64,
        size as u64,
        nonce,
        out.as_mut_ptr() as u64,
        16,
    ) as i64
}
