#![no_std]
#![no_main]

// Init server — Phase 1 stub.
//
// The init server is the first userspace process spawned by the kernel.
// In Phase 1 it simply loops; Phase 4 will expand it into the Minix-style
// process hierarchy root (PM, VFS, DS, etc.).

use core::panic::PanicInfo;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // TODO Phase 1: send a "hello" IPC message to the kernel log endpoint.
    // TODO Phase 4: spawn PM, VFS, DS servers.
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
