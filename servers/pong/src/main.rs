//! Phase-12 SMP stress — "pong" side of a cross-CPU IPC ping/pong pair.
//!
//! Echoes every message back to its sender, verifying the payload pattern
//! first (a corrupt message is reported but still echoed, so ping's error
//! counter is the single source of truth).  Receives with M_ANY — no other
//! server sends to pong.

#![no_std]
#![no_main]

use tanix_libsys::abi::{BootInfo, Message, M_ANY};
use tanix_libsys::{fmt::StrBuf, sys};

const MTYPE: u32 = 0xA11;
const LOG_EVERY: u32 = 50_000;

fn pattern(round: u32, i: usize) -> u32 {
    round
        .wrapping_mul(2_654_435_761)
        .wrapping_add((i as u32).wrapping_mul(0x9E37_79B9))
        .wrapping_add(0xDEAD_BEEF)
}

fn check(round: u32, m: &Message) -> bool {
    if m.mtype != MTYPE {
        return false;
    }
    for i in 1..8 {
        if m.data[i] != pattern(round, i) {
            return false;
        }
    }
    true
}

#[no_mangle]
pub extern "C" fn server_main(_info: *const BootInfo) -> ! {
    sys::log(0, "pong: up");

    let mut served: u32 = 0;
    let mut errors: u32 = 0;
    loop {
        let (src, m) = sys::receive(M_ANY);
        let ok = check(m.data[0], &m);
        if !ok {
            errors = errors.wrapping_add(1);
            if errors <= 10 {
                sys::log(1, "pong: corrupt message");
            }
        }
        served = served.wrapping_add(1);

        if served.is_multiple_of(LOG_EVERY) {
            let mut s = StrBuf::new();
            s.push_str("pong: ");
            s.push_dec32(served);
            s.push_str(" served, ");
            s.push_dec32(errors);
            s.push_str(" corrupt");
            sys::log(0, s.as_str());
        }

        let _ = sys::send(src, &m);
    }
}
