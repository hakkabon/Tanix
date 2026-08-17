//! Device server (`dev`) — owns the PL011 UART0.
//!
//! Minix-style: no process touches the hardware directly; they send
//! `M_DEV_WRITE` messages and get a byte count back.
//!
//! Phase 19: the UART window is *not* hardcoded anymore — the kernel
//! grants it through the MMIO capability 0 (SYS_MAP_CAP) and returns the
//! machine-correct base (sbsa-ref puts the PL011 at 0x6000_0000, virt at
//! 0x0900_0000).  The server also demonstrates lazy stack growth: a deep
//! bounded recursion faults pages below the single mapped top page, and
//! the VM-fault resolver maps them in.

#![no_std]
#![no_main]

use tanix_libsys::abi::{
    BootInfo, Message, M_ANY, M_DEV_WRITE, M_DEV_WRITE_REPLY, MAX_INLINE_STR,
};
use tanix_libsys::sys;

/// PL011 base — resolved once via the MMIO capability (0 until map_cap).
static mut UART0: usize = 0;

const UART_DR_OFF: usize = 0x000;
const UART_FR_OFF: usize = 0x018;
const FR_TXFF: u32 = 1 << 5;

fn putc(c: u8) {
    let base = unsafe { UART0 };
    if base == 0 {
        return;
    }
    // Wait until the transmit FIFO is not full (bit 5 of FR).
    while unsafe { core::ptr::read_volatile((base + UART_FR_OFF) as *const u32) } & FR_TXFF != 0 {}
    unsafe { core::ptr::write_volatile((base + UART_DR_OFF) as *mut u8, c) };
}

fn puts(s: &str) {
    for b in s.bytes() {
        putc(b);
    }
}

/// Bounded deep recursion: consumes about `depth * 64` bytes of user
/// stack per frame, forcing the fault resolver to map pages below the
/// single page mapped at spawn.
#[inline(never)]
fn deep(depth: usize) -> usize {
    if depth == 0 {
        return 0x19;
    }
    let mut frame_marker = [0u8; 64];
    frame_marker[0] = depth as u8;
    let r = deep(depth - 1);
    frame_marker[63] = r as u8;
    r.wrapping_add(1)
}

#[no_mangle]
pub extern "C" fn server_main(_info: *const BootInfo) -> ! {
    sys::log(0, "dev: up");

    // ── Phase 19a: capability-gated MMIO — the console UART. ──────────────
    let uart = sys::map_cap(0);
    if uart == 0 {
        sys::log(2, "dev: no UART capability granted");
    } else {
        unsafe { UART0 = uart };
        let _ = sys::log(0, "dev: UART mapped via cap 0");
        puts("\r\ndev(UART-cap): console backing window is capability-granted\r\n");
        // Exercise the M_DEV_WRITE service through the new window.
        puts("dev(UART-cap): M_DEV_WRITE ready\r\n");
    }

    // ── Phase 19b: lazy stack growth. ──────────────────────────────────────
    // 64 B per stack frame × 105 frames ≈ 10 KiB — comfortably past the
    // 2-3 pages the resolver must fault in, but inside the 12 KiB growth
    // budget of the 16 KiB stack window.
    let _ = deep(105);
    sys::log(0, "dev: stack grew downward 105 frames (fault-in)");

    sys::log(0, "dev: ready");
    loop {
        let (src, msg) = sys::receive(M_ANY);
        match msg.mtype {
            M_DEV_WRITE => {
                // Payload: data[0] = byte count, then up to 28 bytes in
                // data[1..] (little-endian words).
                let len = (msg.data[0] as usize).min(MAX_INLINE_STR);
                let mut written: u32 = 0;
                for i in 0..len {
                    let b = (msg.data[1 + i / 4] >> (8 * (i % 4))) as u8;
                    putc(b);
                    written += 1;
                }
                let mut rep = Message::new(M_DEV_WRITE_REPLY);
                rep.data[0] = written;
                sys::send(src, &rep);
            }
            other => {
                sys::log(1, "dev: unknown message type");
                let _ = other;
            }
        }
    }
}