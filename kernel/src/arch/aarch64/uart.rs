//! PL011 UART driver — minimal polling output for kernel logging.
//!
//! QEMU `virt` exposes a PL011 at physical address 0x0900_0000.
//! QEMU pre-initialises the UART, so we can write bytes immediately without
//! programming the baud-rate divisors.  A full init sequence is included
//! anyway so the driver works on real hardware.
//!
//! This module also implements the `log::Log` trait so the kernel can use the
//! standard `log::{info, warn, error, …}` macros.

use core::fmt;
use log::{Level, LevelFilter, Metadata, Record};

// ── Register map ─────────────────────────────────────────────────────────────

const UART0_BASE: usize = 0x0900_0000;

const DR: usize = UART0_BASE; // Data Register
const FR: usize = UART0_BASE + 0x18; // Flag Register
const IBRD: usize = UART0_BASE + 0x24; // Integer Baud Rate Divisor
const FBRD: usize = UART0_BASE + 0x28; // Fractional Baud Rate Divisor
const LCR_H: usize = UART0_BASE + 0x2C; // Line Control Register
const CR: usize = UART0_BASE + 0x30; // Control Register

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
fn mmio_write(addr: usize, val: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, val) }
}

#[inline]
fn mmio_read(addr: usize) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

// ── Low-level byte output ─────────────────────────────────────────────────────

/// Initialise the PL011.  Safe to call multiple times (idempotent).
pub fn init() {
    // Disable the UART before reconfiguring.
    mmio_write(CR, 0);

    // Wait for any in-progress transmission to finish.
    while mmio_read(FR) & FR_BUSY != 0 {
        core::hint::spin_loop();
    }

    // Baud rate: 115200 at 24 MHz UART clock (QEMU default for virt).
    //   IBRD = 13, FBRD = 1  →  115384 baud (close enough)
    mmio_write(IBRD, 13);
    mmio_write(FBRD, 1);

    // 8-N-1, FIFO enabled.
    mmio_write(LCR_H, LCR_H_WLEN_8 | LCR_H_FEN);

    // Enable UART, TX, RX.
    mmio_write(CR, CR_UARTEN | CR_TXE | CR_RXE);
}

/// Write a single byte, blocking until the TX FIFO has space.
#[inline]
pub fn putc(byte: u8) {
    while mmio_read(FR) & FR_TXFF != 0 {
        core::hint::spin_loop();
    }
    mmio_write(DR, byte as u32);
}

/// Write a string slice, converting `\n` to `\r\n` for serial terminals.
pub fn puts(s: &str) {
    for &b in s.as_bytes() {
        if b == b'\n' {
            putc(b'\r');
        }
        putc(b);
    }
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
