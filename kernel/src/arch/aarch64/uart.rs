//! PL011 UART driver — minimal polling output for kernel logging.
//!
//! QEMU `virt` exposes a PL011 at physical address 0x0900_0000; the
//! `sbsa-ref` machine places its PL011 at 0x6000_0000 (Phase 16 — see
//! `machine.rs`).  QEMU pre-initialises the UART, so we can write bytes
//! immediately without programming the baud-rate divisors.  A full init
//! sequence is included anyway so the driver works on real hardware.
//!
//! This module also implements the `log::Log` trait so the kernel can use the
//! standard `log::{info, warn, error, …}` macros.
//!
//! Phase 11 (SMP): `puts` / `putc` take a spinlock so lines from concurrent
//! cores do not interleave.  Safe: every log call happens with IRQs masked
//! (exception entry masks DAIF; the only unmasked kernel windows — the
//! `SYS_WAIT_IRQ` wait loop and the secondary idle loop — never log).

use core::fmt;
use log::{Level, LevelFilter, Metadata, Record};

use super::machine;

// ── Register map ─────────────────────────────────────────────────────────────

fn uart0_base() -> usize {
    machine::machine().uart_base
}

const DR: usize = 0; // Data Register
const FR: usize = 0x18; // Flag Register
const IBRD: usize = 0x24; // Integer Baud Rate Divisor
const FBRD: usize = 0x28; // Fractional Baud Rate Divisor
const LCR_H: usize = 0x2C; // Line Control Register
const CR: usize = 0x30; // Control Register

/// Absolute address of a PL011 register on the current machine.
#[inline]
fn uart_reg(off: usize) -> usize {
    uart0_base() + off
}

// FR bits
const FR_TXFF: u32 = 1 << 5; // TX FIFO full
const FR_BUSY: u32 = 1 << 3; // UART busy

// CR bits
const CR_UARTEN: u32 = 1 << 0; // UART enable
const CR_TXE: u32 = 1 << 8; // TX enable
const CR_RXE: u32 = 1 << 9; // RX enable

// LCR_H bits
const LCR_H_FEN: u32 = 1 << 4; // FIFO enable
const LCR_H_WLEN_8: u32 = 0b11 << 5; // 8-bit word length

#[inline]
#[inline]
fn mmio_write(addr: usize, val: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, val) }
}

#[inline]
fn mmio_read(addr: usize) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

/// Read a PL011 register (offset from the machine's UART base).
#[inline]
fn rd_reg(off: usize) -> u32 {
    mmio_read(uart_reg(off))
}

/// Write a PL011 register (offset from the machine's UART base).
#[inline]
fn wr_reg(off: usize, val: u32) {
    mmio_write(uart_reg(off), val);
}

// ── Low-level byte output ─────────────────────────────────────────────────────

/// Initialise the PL011.  Safe to call multiple times (idempotent).
pub fn init() {
    // Disable the UART before reconfiguring.
    wr_reg(CR, 0);

    // Wait for any in-progress transmission to finish.
    while rd_reg(FR) & FR_BUSY != 0 {
        core::hint::spin_loop();
    }

    // Baud rate: 115200 at 24 MHz UART clock (QEMU default for virt).
    //   IBRD = 13, FBRD = 1  →  115384 baud (close enough)
    wr_reg(IBRD, 13);
    wr_reg(FBRD, 1);

    // 8-N-1, FIFO enabled.
    wr_reg(LCR_H, LCR_H_WLEN_8 | LCR_H_FEN);

    // Enable UART, TX, RX.
    wr_reg(CR, CR_UARTEN | CR_TXE | CR_RXE);
}

/// Serializes `puts` / `putc` across cores (Phase 11).
static UART_LOCK: crate::sync::SpinLock = crate::sync::SpinLock::new();

/// Write a single byte, blocking until the TX FIFO has space.
#[inline]
fn putc_locked(byte: u8) {
    while rd_reg(FR) & FR_TXFF != 0 {
        core::hint::spin_loop();
    }
    wr_reg(DR, byte as u32);
}

/// Mask IRQs and return the previous DAIF register (so the caller can
/// restore it).  `puts` / `putc` hold the UART lock for the whole string,
/// so an IRQ taken mid-print must not re-enter the lock on the same CPU
/// (the tick handler logs — a re-entrant spinlock would self-deadlock).
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

/// Write a single byte, blocking until the TX FIFO has space.
#[inline]
pub fn putc(byte: u8) {
    let daif = irq_mask_save();
    let lock = &UART_LOCK;
    lock.lock();
    putc_locked(byte);
    lock.unlock();
    irq_restore(daif);
}

/// Write a string slice, converting `\n` to `\r\n` for serial terminals.
/// The whole string is emitted under the UART lock (Phase 11).
pub fn puts(s: &str) {
    let daif = irq_mask_save();
    let lock = &UART_LOCK;
    lock.lock();
    for &b in s.as_bytes() {
        if b == b'\n' {
            putc_locked(b'\r');
        }
        putc_locked(b);
    }
    lock.unlock();
    irq_restore(daif);
}

// ── fmt::Write impl ───────────────────────────────────────────────────────────

/// Zero-size writer that delegates to `puts`.
pub struct UartWriter;

impl fmt::Write for UartWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        puts(s);
        Ok(())
    }
}

/// Print a formatted string to the UART without allocation.
#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = write!($crate::arch::aarch64::uart::UartWriter, $($arg)*);
    }};
}

/// Print a formatted string followed by a newline.
#[macro_export]
macro_rules! kprintln {
    () => { $crate::kprint!("\n") };
    ($($arg:tt)*) => { $crate::kprint!("{}\n", format_args!($($arg)*)) };
}

// ── log::Log backend ──────────────────────────────────────────────────────────

struct UartLogger;

impl log::Log for UartLogger {
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
        let _ = writeln!(
            UartWriter,
            "[{}] {}",
            level_str,
            record.args()
        );
    }

    fn flush(&self) {}
}

static LOGGER: UartLogger = UartLogger;

/// Register the UART as the global `log` backend.
/// Call once during `arch::aarch64::init()`.
pub fn logger_init() {
    log::set_logger(&LOGGER).ok();
    log::set_max_level(LevelFilter::Trace);
}
