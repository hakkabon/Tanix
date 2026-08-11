//! Phase-12 SMP stress — "ping" side of a cross-CPU IPC ping/pong pair.
//!
//! Ping drives a tight send → receive round trip against `pong`: every
//! round parks in the kernel's blocking `send` (rendezvous) and again in
//! `receive`, so the two tasks are constantly being context-switched and
//! preempted by the 1 ms ticks.  On a 4-CPU boot they migrate across cores
//! and their rendezvous is delivered cross-CPU — exactly the paths Phase 12
//! hardened (SCHED_LOCK, wakeup pokes, atomic pending bits).
//!
//! The payload carries a round number plus a per-word checksum pattern;
//! both sides verify it, so a lost/corrupted/duplicated message shows up as
//! an error count (or, worst case, a hang — a lost wakeup means ping blocks
//! in `send`/`receive` forever and the stress counter stops advancing).

#![no_std]
#![no_main]

use tanix_libsys::abi::{BootInfo, Message, M_ANY};
use tanix_libsys::{fmt::StrBuf, sys};

/// Stress protocol message type.
const MTYPE: u32 = 0xA11;
/// Log progress every N completed rounds.
const LOG_EVERY: u32 = 50_000;

/// Payload pattern for round `round`, word `i` (1..8).
fn pattern(round: u32, i: usize) -> u32 {
    round
        .wrapping_mul(2_654_435_761)
        .wrapping_add((i as u32).wrapping_mul(0x9E37_79B9))
        .wrapping_add(0xDEAD_BEEF)
}

fn build(round: u32) -> Message {
    let mut m = Message::new(MTYPE);
    m.data[0] = round;
    for i in 1..8 {
        m.data[i] = pattern(round, i);
    }
    m
}

/// Verify a message against the expected pattern for `round`.
fn check(round: u32, m: &Message) -> bool {
    if m.mtype != MTYPE || m.data[0] != round {
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
    sys::log(0, "ping: up");

    // Pong may not be spawned yet — wait for it.
    let mut tries: u32 = 0;
    let pong = loop {
        let id = sys::who("pong");
        if id > 0 {
            break id as u32;
        }
        tries = tries.wrapping_add(1);
        if tries.is_multiple_of(10_000) {
            let mut s = StrBuf::new();
            s.push_str("ping: who(pong)=");
            s.push_dec32(id as u32);
            sys::log(1, s.as_str());
        }
        sys::yield_cpu();
    };
    sys::log(0, "ping: pong found");

    let mut round: u32 = 1;
    let mut errors: u32 = 0;
    loop {
        let m = build(round);
        if sys::send(pong, &m) != 0 {
            errors = errors.wrapping_add(1);
            if errors <= 10 {
                sys::log(1, "ping: send failed");
            }
        } else {
            let (src, reply) = sys::receive(M_ANY);
            if src != pong || !check(round, &reply) {
                errors = errors.wrapping_add(1);
                if errors <= 10 {
                    sys::log(1, "ping: corrupt or mis-stamped reply");
                }
            }
        }

        round = round.wrapping_add(1);
        if round.is_multiple_of(LOG_EVERY) {
            let mut s = StrBuf::new();
            s.push_str("ping: ");
            s.push_dec32(round);
            s.push_str(" rounds, ");
            s.push_dec32(errors);
            s.push_str(" errors");
            sys::log(0, s.as_str());
        }
    }
}
