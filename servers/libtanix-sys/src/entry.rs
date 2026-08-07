//! Server binary entry point and panic handler.

// Every server binary defines this symbol (see `servers/*/src/main.rs`).
extern "C" {
    fn server_main(info: *const crate::abi::BootInfo) -> !;
}

/// Entry point.  The kernel preloads the boot-info pointer into x19
/// (callee-saved: it survives the context switch and the C ABI), so we
/// simply read it and hand it to `server_main`.
#[no_mangle]
pub extern "C" fn _start() -> ! {
    let info = unsafe { crate::sys::boot_info() };
    unsafe { server_main(info as *const _) }
}
