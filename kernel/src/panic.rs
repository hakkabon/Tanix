use core::fmt::Write;
use core::panic::PanicInfo;

/// Minimal UART write for panic messages.
///
/// QEMU `virt` exposes a PL011 UART at 0x0900_0000.  Writing a byte to the
/// Data Register (offset 0) outputs it immediately — no init needed because
/// QEMU pre-initialises the UART for us.
fn uart_write_bytes(bytes: &[u8]) {
    const UART0_DR: *mut u8 = 0x0900_0000 as *mut u8;
    for &b in bytes {
        unsafe { core::ptr::write_volatile(UART0_DR, b) };
    }
}

fn uart_print(s: &str) {
    uart_write_bytes(s.as_bytes());
}

/// `core::fmt::Write` backend so formatted panic messages render on the UART.
struct UartWriter;

impl Write for UartWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        uart_print(s);
        Ok(())
    }
}

/// Semihosting SYS_EXIT — tells QEMU to terminate the simulation.
/// Application exit code 1 signals an abnormal exit.
///
/// Calling convention: HLT #0xF000  (AArch64 semihosting trap)
#[inline(never)]
fn semihosting_exit(code: u64) -> ! {
    // AArch64 semihosting: w0 = operation (0x20 = SYS_EXIT_EXTENDED),
    // x1 = pointer to parameter block { reason, subcode }
    let block: [u64; 2] = [
        0x20026, // ADP_Stopped_ApplicationExit
        code,
    ];
    unsafe {
        core::arch::asm!(
            "hlt #0xF000",
            in("x0") 0x20u64,          // SYS_EXIT_EXTENDED
            in("x1") block.as_ptr(),
            options(noreturn)
        );
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    uart_print("\n\n[PANIC] ");

    if let Some(location) = info.location() {
        // Print "file:line" without allocations
        uart_print(location.file());
        uart_print(":");
        // Convert line number to decimal manually (no alloc / format!)
        let mut line = location.line();
        let mut buf = [0u8; 10];
        let mut pos = buf.len();
        loop {
            pos -= 1;
            buf[pos] = b'0' + (line % 10) as u8;
            line /= 10;
            if line == 0 {
                break;
            }
        }
        uart_write_bytes(&buf[pos..]);
        uart_print(" — ");
    }

    // `message()` is a `&str` only for literal messages; formatted panics
    // (format_args!) must be rendered through `fmt::Write`.
    match info.message().as_str() {
        Some(msg) if !msg.is_empty() => uart_print(msg),
        Some(_) => uart_print("(no message)"),
        None => {
            let _ = core::fmt::write(&mut UartWriter, format_args!("{}", info.message()));
        }
    }

    uart_print("\n");

    // Terminate QEMU cleanly so CI / test runs don't hang.
    semihosting_exit(1);
}
