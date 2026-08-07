//! Device server (`dev`) — owns the PL011 UART0.
//!
//! Minix-style: no process touches the hardware directly; they send
//! `M_DEV_WRITE` messages and get a byte count back.

#![no_std]
#![no_main]

use tanix_libsys::abi::{
    BootInfo, Message, M_ANY, M_DEV_WRITE, M_DEV_WRITE_REPLY, MAX_INLINE_STR,
};
use tanix_libsys::sys;

const UART0_DR: *mut u8 = 0x0900_0000 as *mut u8;
const UART0_FR: *mut u8 = 0x0900_0018 as *mut u8;

fn putc(c: u8) {
    // Wait until the transmit FIFO is not full (bit 5 of FR).
    while unsafe { core::ptr::read_volatile(UART0_FR) } & (1 << 5) != 0 {}
    unsafe { core::ptr::write_volatile(UART0_DR, c) };
}

#[no_mangle]
pub extern "C" fn server_main(_info: *const BootInfo) -> ! {
    sys::log(0, "dev: up");
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
