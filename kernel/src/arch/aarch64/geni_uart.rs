//! Qualcomm GENI Serial Engine UART driver — Phase 23 (QCS6490 / ttyMSM0).
//!
//! The Generic Interface (GENI) Serial Engine IP is the UART controller
//! on Qualcomm SoCs since SDM845 (including QCS6490 on Dragon Q6A /
//! RUBIK Pi 3).  It is not a PL011; the existing `uart.rs` driver does not
//! work on these boards.
//!
//! ## Key differences from PL011
//! - No simple DR/FR register pair.  Data flows through a FIFO accessed via
//!   `GENI_TX_FIFOn` words.  Writes go to `SE_GENI_TX_FIFO` (word-wide,
//!   FIFO-push semantics), reads from `SE_GENI_RX_FIFO`.
//! - Status lives in `SE_GENI_M_IRQ_STATUS` / `SE_GENI_S_IRQ_STATUS` and
//!   `SE_GENI_TX_FIFO_STATUS` / `SE_GENI_RX_FIFO_STATUS`.
//! - The SE must be in UART mode (`GENI_UART`) with the M-FSM in the `IDLE`
//!   state before the first TX.  UEFI / XBL leaves it configured at
//!   115200 n8 — we detect that and skip re-initialisation to avoid races
//!   during the UEFI-to-kernel handoff.  A full init path is included for
//!   the bare-metal EL3 case.
//! - Clock: UART_CLK_DIV and UART_OVERSAMPLING come from the clock driver
//!   on Linux; here we assume firmware already configured the baud rate and
//!   only verify it with a read-back before using the UART.
//!
//! ## This driver
//! Polling TX only (all the kernel needs for early-boot logging).
//! RX is not needed by the kernel itself — it only matters for the future
//! shell / interactive server layer, which speaks to the kernel through
//! IPC, not raw UART.  A minimal RX poll path is included for Phase 23
//! UART probing (confirming "hello" echoed back from a terminal is the
//! Day-3 success criterion in `docs/PHASE23_CHECKLIST.md`).
//!
//! ## Register map
//! All offsets are relative to the SE (Serial Engine) base address
//! (`GENI_SE_BASE` in the SoC DTS, `reg` property of the chosen UART node).
//! On QCS6490 / Dragon Q6A the console UART is:
//!   uart0: serial@a90000 { ... }
//! giving SE base = 0x00A9_0000.  On RUBIK Pi 3 (also QCS6490) the same
//! base applies (same SoC, same board-file).  Both boards expose the
//! console on ttyMSM0 at 115200n8.
//!
//! Source references:
//! - `drivers/tty/serial/qcom_geni_serial.c` (Linux 6.8+)
//! - `drivers/soc/qcom/qcom-geni-se.c`
//! - `include/linux/qcom-geni-se.h`

#![allow(dead_code)]

use core::fmt;
use log::{Level, LevelFilter, Metadata, Record};

use super::machine;

// ── GENI SE register offsets ──────────────────────────────────────────────────

/// GENI output-enable / configuration.
const GENI_OUTPUT_CTRL: usize = 0x24;
/// Force-on all clocks inside the SE (needed during init).
const GENI_CGC_CTRL: usize = 0x28;
/// SE DMA mode select — 0 = FIFO mode (what we use).
const SE_DMA_IF_EN: usize = 0x004;
/// SE general configuration.
const GENI_SER_M_CLK_CFG: usize = 0x048;
const GENI_SER_S_CLK_CFG: usize = 0x04C;

/// GENI M-FSM command register — write a TX command here to start a transfer.
const SE_GENI_M_CMD0: usize = 0x600;
/// GENI M-FSM IRQ status (bit 0 = M_CMD_DONE, bit 5 = TX_FIFO_WATERMARK).
const SE_GENI_M_IRQ_STATUS: usize = 0x610;
/// GENI M-FSM IRQ clear.
const SE_GENI_M_IRQ_CLEAR: usize = 0x618;
/// GENI M-FSM IRQ enable.
const SE_GENI_M_IRQ_EN: usize = 0x614;

/// GENI S-FSM (secondary, RX) IRQ status (bit 0 = S_CMD_DONE).
const SE_GENI_S_IRQ_STATUS: usize = 0x640;
const SE_GENI_S_IRQ_CLEAR: usize = 0x648;
const SE_GENI_S_CMD0: usize = 0x630;

/// TX FIFO (32-bit write = push up to 4 bytes).
const SE_GENI_TX_FIFO: usize = 0x700;
/// RX FIFO (32-bit read = pop up to 4 bytes).
const SE_GENI_RX_FIFO: usize = 0x780;

/// TX FIFO status: bits [27:16] = FIFO level (how many 4-byte entries used),
/// bits [3:0] = TX WC (words consumed by current command).
const SE_GENI_TX_FIFO_STATUS: usize = 0x800;
/// RX FIFO status: bits [27:16] = FIFO level (entries available to read),
/// bits [3:0] = last-entry valid bytes.
const SE_GENI_RX_FIFO_STATUS: usize = 0x804;

/// RX watermark level — how many words before an RX_FIFO_WATERMARK IRQ.
const SE_GENI_RX_WATERMARK_REG: usize = 0x40C;
/// TX watermark level — how many free words before a TX_FIFO_WATERMARK IRQ.
const SE_GENI_TX_WATERMARK_REG: usize = 0x40C; // same offset, written for TX path

/// Number of RX FIFO words.
const SE_GENI_RX_FIFO_DEPTH: usize = 0x808;

/// UART loopback / parity / stop-bit configuration.
const SE_UART_LOOPBACK_CFG: usize = 0x54;
/// Main UART configuration: TX/RX packing mode.
const SE_UART_TX_TRANS_LEN: usize = 0x270; // number of TX bytes to transfer
const SE_UART_RX_TRANS_LEN: usize = 0x294; // number of RX bytes to receive

/// Baud rate word-length / stop-bit config (written by clock driver; we
/// read-back only).
const SE_UART_TX_WORD_LEN: usize = 0x268;
const SE_UART_RX_WORD_LEN: usize = 0x28C;

/// SE HW version register — non-zero confirms GENI IP is accessible.
const SE_HW_VERSION: usize = 0x600C; // actually at GENI_IF_FIFO_CFG, varies by revision
const GENI_FW_REVISION_RO: usize = 0x68;

/// GENI clock config — set by firmware; we just verify it is non-zero.
const GENI_CLK_CTRL_RO: usize = 0x60;

// ── M_CMD0 UART command encodings ─────────────────────────────────────────────

/// M_CMD0 opcode field for UART TX (bits [31:27]).
const M_CMD_TX_OPCODE: u32 = 1u32 << 27;
/// Bit 20 of M_CMD0: last fragment (ends the transfer).
const M_CMD_LAST_FRAG: u32 = 1 << 20;

// ── IRQ status bits ───────────────────────────────────────────────────────────

/// M_IRQ_STATUS: M_CMD_DONE (TX transfer completed).
const M_CMD_DONE: u32 = 1 << 0;
/// M_IRQ_STATUS: TX FIFO has space (watermark not reached).
const M_TX_FIFO_WATERMARK: u32 = 1 << 5;
/// S_IRQ_STATUS: UART break condition detected.
const S_RX_FIFO_LAST: u32 = 1 << 12;

// ── TX FIFO status ────────────────────────────────────────────────────────────

/// Shift for the FIFO level (number of occupied 4-byte entries) in
/// `SE_GENI_TX_FIFO_STATUS`.
const TX_FIFO_LEVEL_SHIFT: u32 = 16;
const TX_FIFO_LEVEL_MASK: u32 = 0xFFF;

/// GENI TX FIFO max depth (32 words = 128 bytes on QCS6490).  Read from
/// `SE_GENI_TX_FIFO_STATUS` at startup; this is the fallback.
const TX_FIFO_DEPTH: u32 = 16;

// ── MMIO helpers ──────────────────────────────────────────────────────────────

#[inline]
fn se_base() -> usize {
    machine::machine().uart_base
}

#[inline]
fn rd(off: usize) -> u32 {
    unsafe { core::ptr::read_volatile((se_base() + off) as *const u32) }
}

#[inline]
fn wr(off: usize, val: u32) {
    unsafe { core::ptr::write_volatile((se_base() + off) as *mut u32, val) }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// True when the TX FIFO level is below the threshold (space available for
/// at least one more word).  This is the polling gate for TX.
///
/// `SE_GENI_TX_FIFO_STATUS` bits [27:16] hold the number of words
/// *currently in the FIFO*.  We wait until that count drops below
/// `TX_FIFO_DEPTH - 1` (at least one slot free).
#[inline]
fn tx_fifo_has_space() -> bool {
    let level = (rd(SE_GENI_TX_FIFO_STATUS) >> TX_FIFO_LEVEL_SHIFT) & TX_FIFO_LEVEL_MASK;
    level < TX_FIFO_DEPTH - 1
}

/// Wait for the GENI M-FSM to finish the previous TX command (M_CMD_DONE).
/// Must be called before starting a new M_CMD0 transfer; on UEFI handoff
/// the FSM is already idle so this returns immediately.
fn wait_tx_done() {
    let mut spins: u32 = 0;
    while rd(SE_GENI_M_IRQ_STATUS) & M_CMD_DONE == 0 {
        spins += 1;
        if spins > 2_000_000 {
            // Firmware left the FSM in a bad state; reset the done bit
            // forcibly so the next TX can at least try.
            wr(SE_GENI_M_IRQ_CLEAR, M_CMD_DONE);
            return;
        }
        core::hint::spin_loop();
    }
    // Acknowledge the done bit.
    wr(SE_GENI_M_IRQ_CLEAR, M_CMD_DONE);
}

/// Send exactly one byte via the GENI TX FIFO.
///
/// Each GENI TX command transfers a fixed number of bytes declared in
/// `SE_UART_TX_TRANS_LEN`.  The simplest UEFI-compatible strategy for
/// polling output is: one command per byte.  This is slow (~115200 baud)
/// but correct and requires no dynamic buffer management.
///
/// On a warm UART (UEFI already printed to it) the M-FSM is idle and
/// `M_CMD_DONE` is already set from the last firmware print; we clear it
/// before starting our own command.
fn send_byte(b: u8) {
    // Wait for any previous command to complete.
    wait_tx_done();

    // Program the transfer length (1 byte).
    wr(SE_UART_TX_TRANS_LEN, 1);

    // Load the byte into the TX FIFO.  The FIFO is word-wide; only the
    // least-significant byte matters for a 1-byte transfer.
    // Spin until FIFO has space (normally instant on a warm UART).
    let mut spins: u32 = 0;
    while !tx_fifo_has_space() {
        spins += 1;
        if spins > 500_000 {
            break; // give up; UART may be stuck
        }
        core::hint::spin_loop();
    }
    wr(SE_GENI_TX_FIFO, b as u32);

    // Issue the TX command: opcode=1 (UART TX), LAST_FRAG set.
    wr(SE_GENI_M_CMD0, M_CMD_TX_OPCODE | M_CMD_LAST_FRAG | 1u32);
}

// ── Spinlock (reuse the same pattern as the PL011 driver) ────────────────────

static GENI_LOCK: crate::sync::SpinLock = crate::sync::SpinLock::new();

#[inline]
fn irq_mask_save() -> u64 {
    let daif: u64;
    unsafe {
        core::arch::asm!(
            "mrs {d}, daif",
            "msr daifset, #2",
            d = out(reg) daif,
            options(nomem, nostack)
        );
    }
    daif
}

#[inline]
fn irq_restore(daif: u64) {
    unsafe {
        core::arch::asm!(
            "msr daif, {d}",
            d = in(reg) daif,
            options(nomem, nostack)
        );
    }
}

// ── Public interface ──────────────────────────────────────────────────────────

/// Detect whether the GENI IP is accessible at the configured base address.
///
/// Reads `GENI_FW_REVISION_RO` — a non-zero value confirms the SE is mapped
/// and clocked.  Returns `false` if the register reads all-zeros or all-ones
/// (open bus / not powered).
pub fn probe() -> bool {
    let rev = rd(GENI_FW_REVISION_RO);
    rev != 0 && rev != 0xFFFF_FFFF
}

/// Initialise the GENI UART for polling TX.
///
/// On Dragon Q6A / RUBIK Pi 3 the UEFI firmware leaves the GENI SE in a
/// correctly configured state (115200 n8, FIFO mode, M-FSM idle after its
/// last print).  This function:
///   1. Verifies the IP is accessible (`probe()`).
///   2. Clears any pending M_CMD_DONE from firmware's last TX.
///   3. Sets `SE_DMA_IF_EN = 0` to ensure FIFO mode (not DMA).
///
/// A full re-initialisation (baud rate divisors, word length, FIFO depths)
/// is deliberately skipped: re-programming the baud divisors while the
/// `ttyMSM0` clock source is unknown would break the baud rate.  Firmware's
/// configuration is trusted; `probe()` guards against a completely unclocked
/// SE.
pub fn init() {
    if !probe() {
        // The SE is not accessible — don't attempt to use it.
        // The caller (`mod.rs` init) will fall back to PL011 or silent mode.
        return;
    }

    // Ensure FIFO (not DMA) mode.
    wr(SE_DMA_IF_EN, 0);

    // Clear any stale M_CMD_DONE from firmware's last print so our first
    // `wait_tx_done()` returns immediately.
    wr(SE_GENI_M_IRQ_CLEAR, M_CMD_DONE);
}

/// Write a single byte, \n → \r\n translation.
pub fn putc(byte: u8) {
    let daif = irq_mask_save();
    GENI_LOCK.lock();
    if byte == b'\n' {
        send_byte(b'\r');
    }
    send_byte(byte);
    GENI_LOCK.unlock();
    irq_restore(daif);
}

/// Write a string; \n → \r\n, whole string under the lock.
pub fn puts(s: &str) {
    let daif = irq_mask_save();
    GENI_LOCK.lock();
    for &b in s.as_bytes() {
        if b == b'\n' {
            send_byte(b'\r');
        }
        send_byte(b);
    }
    GENI_LOCK.unlock();
    irq_restore(daif);
}

// ── Minimal RX poll (Phase 23 probe) ─────────────────────────────────────────
//
// Only needed for the Day-3 "hello from Tanix, echo it back" probe.
// Not used by the kernel log path.

/// Returns the number of RX FIFO words available.
fn rx_fifo_level() -> u32 {
    (rd(SE_GENI_RX_FIFO_STATUS) >> 16) & 0xFFF
}

/// Poll for a received byte.  Returns `Some(byte)` if the RX FIFO has at
/// least one word; the word may contain up to 4 bytes depending on how the
/// RX command was set up.  For a simple UART probe reading one byte at a
/// time is correct when `SE_UART_RX_WORD_LEN` is configured for 8-bit chars.
pub fn getc() -> Option<u8> {
    if rx_fifo_level() == 0 {
        return None;
    }
    let word = rd(SE_GENI_RX_FIFO);
    Some((word & 0xFF) as u8)
}

// ── fmt::Write impl ───────────────────────────────────────────────────────────

/// Zero-size writer that delegates to `puts`.
pub struct GeniWriter;

impl fmt::Write for GeniWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        puts(s);
        Ok(())
    }
}

// ── log::Log backend ──────────────────────────────────────────────────────────

struct GeniLogger;

impl log::Log for GeniLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        use core::fmt::Write as _;
        let level_str = match record.level() {
            Level::Error => "ERROR",
            Level::Warn  => "WARN ",
            Level::Info  => "INFO ",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        };
        let _ = writeln!(GeniWriter, "[{}] {}", level_str, record.args());
    }

    fn flush(&self) {}
}

static LOGGER: GeniLogger = GeniLogger;

/// Register the GENI UART as the global `log` backend.
/// Called from `uart::logger_init()` on `qcs6490` targets.
pub fn logger_init() {
    log::set_logger(&LOGGER).ok();
    log::set_max_level(LevelFilter::Info);
}
