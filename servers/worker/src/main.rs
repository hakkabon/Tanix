//! Demo worker process.
//!
//! Spawned by `pm` at `init`'s request.  It looks up the device server and
//! process manager by *name* (via the `who` syscall), says hello through
//! the device server, then reports its own exit through `pm`.

#![no_std]
#![no_main]

use tanix_libsys::abi::{
    BootInfo, Message, M_ANY, M_DEV_WRITE, M_DEV_WRITE_REPLY, M_PM_EXIT,
    M_PM_EXIT_REPLY,
};
use tanix_libsys::{fmt::StrBuf, sys};

fn pack_str(msg: &mut Message, s: &str) {
    let n = s.len().min(28);
    msg.data[0] = n as u32;
    for (i, &b) in s.as_bytes().iter().take(n).enumerate() {
        msg.data[1 + i / 4] |= (b as u32) << (8 * (i % 4));
    }
}

// Entry convention: the kernel passes BootInfo in x19; dereferencing it is
// the documented hand-off (see libtanix-sys::entry).
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn server_main(info: *const BootInfo) -> ! {
    sys::log(0, "worker: up");

    let dev = sys::who("dev");
    let pm = sys::who("pm");

    if dev >= 0 {
        let mut msg = Message::new(M_DEV_WRITE);
        pack_str(&mut msg, "worker: hello via dev server");
        sys::send(dev as u32, &msg);
        let (_src, rep) = sys::receive(M_ANY);
        if rep.mtype == M_DEV_WRITE_REPLY {
            let mut buf = StrBuf::new();
            buf.push_str("worker: dev server wrote ");
            buf.push_dec32(rep.data[0]);
            buf.push_str(" bytes");
            sys::log(0, buf.as_str());
        }
    }

    if pm >= 0 {
        let mut msg = Message::new(M_PM_EXIT);
        msg.data[0] = unsafe { (*info).task_id };
        sys::send(pm as u32, &msg);
        let (_src, rep) = sys::receive(M_ANY);
        if rep.mtype == M_PM_EXIT_REPLY {
            sys::log(0, "worker: pm acknowledged exit");
        }
    }

    sys::log(0, "worker: done");
    sys::exit();
}
