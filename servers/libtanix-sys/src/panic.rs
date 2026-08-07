//! Panic handler for server binaries: raw UART output, then spin.

use core::fmt::Write;
use core::panic::PanicInfo;

const UART0_DR: *mut u8 = 0x0900_0000 as *mut u8;

struct Uart;

impl Write for Uart {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &b in s.as_bytes() {
            unsafe { core::ptr::write_volatile(UART0_DR, b) };
        }
        Ok(())
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let _ = writeln!(Uart, "\n\n[server PANIC] ");
    if let Some(loc) = info.location() {
        let _ = writeln!(Uart, "{}:{}", loc.file(), loc.line());
    }
    let _ = writeln!(Uart, "{}", info.message());
    loop {
        core::hint::spin_loop();
    }
}
