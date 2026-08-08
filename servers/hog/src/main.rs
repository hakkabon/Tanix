//! Phase-7 demo app: a CPU hog.
//!
//! Spawned by the kernel at the lowest scheduling priority (`hog` = 192),
//! it burns CPU in a tight EL0 spin loop, logging periodically.  This is
//! the preemption proof: without the Phase-7 tick preemption this loop
//! would starve every other server forever (cooperative scheduling has no
//! way to leave a spinning task); with it, higher-priority servers (and
//! the GPU IRQ path) run every millisecond regardless.
//!
//! The hog only ever gives up the CPU voluntarily for a strictly
//! higher-priority runnable task (`yield_cpu`); the real preemption comes
//! from the kernel's tick interrupt.

#![no_std]
#![no_main]

use tanix_libsys::abi::BootInfo;
use tanix_libsys::sys;

const SPIN_LIMIT: u64 = 250_000_000;

#[no_mangle]
pub extern "C" fn server_main(_info: *const BootInfo) -> ! {
    sys::log(0, "hog: up");
    let mut i = 0u64;
    loop {
        i += 1;
        if i % SPIN_LIMIT == 0 {
            sys::log(0, "hog: still hogging");
        }
        sys::yield_cpu();
    }
}
