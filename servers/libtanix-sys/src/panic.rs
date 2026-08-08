//! Panic handler for server binaries (Phase 6).
//!
//! Servers run at EL0 and no longer have UART MMIO mapped, so the panic
//! message goes through the `log` syscall instead.  The handler then
//! executes an undefined instruction on purpose: the kernel catches the
//! resulting EL0 fault and kills the task — the isolation test — and the
//! rest of the system keeps running.

use core::fmt::Write;
use core::panic::PanicInfo;

use crate::sys;

/// Log sink that accumulates into a stack buffer and flushes it through the
/// `log` syscall when it fills (a panic message is small — one flush).
struct LogSink {
    buf: [u8; 128],
    len: usize,
}

impl Write for LogSink {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &b in s.as_bytes() {
            self.buf[self.len % self.buf.len()] = b;
            self.len += 1;
            if self.len == self.buf.len() {
                self.flush();
            }
        }
        Ok(())
    }
}

impl LogSink {
    fn flush(&mut self) {
        let n = self.len.min(self.buf.len() - 1);
        let msg = core::str::from_utf8(&self.buf[..n]).unwrap_or("?");
        sys::log(2, msg);
        self.len = 0;
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let mut sink = LogSink { buf: [0u8; 128], len: 0 };
    let _ = write!(sink, "\n\n[server PANIC] ");
    if let Some(loc) = info.location() {
        let _ = write!(sink, "{}:{} ", loc.file(), loc.line());
    }
    let _ = write!(sink, "{}", info.message());
    sink.flush();

    // Deliberate undefined instruction → EL0 fault → the kernel kills this
    // task (Phase-6 isolation proof) instead of hanging the system.
    loop {
        unsafe { core::arch::asm!("udf #0", options(noreturn, nostack)) }
    }
}
