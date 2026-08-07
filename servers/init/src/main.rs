//! Init server — the root of the Minix-style process hierarchy (Phase 4).
//!
//! Boot flow (mirrors Minix):
//!   1. spawn the system servers: pm, mem, dev;
//!   2. exercise each service over IPC: dev prints a banner, mem allocates
//!      and frees frames, pm execs the `worker` binary;
//!   3. print the boot-complete banner and exit.
//!
//! Everything here talks to the kernel only through the syscall table it
//! received at boot (no kernel symbols, no direct hardware access).

#![no_std]
#![no_main]

use tanix_libsys::abi::{
    BootInfo, Message, M_ANY, M_DEV_WRITE, M_DEV_WRITE_REPLY, M_MEM_ALLOC,
    M_MEM_ALLOC_REPLY, M_MEM_FREE, M_MEM_FREE_REPLY, M_PM_EXEC,
    M_PM_EXEC_REPLY, M_PM_NOTIFY_EXIT,
};
use tanix_libsys::{fmt::StrBuf, sys};

/// Pack up to 28 chars of `s` into a message payload (data[1..] words).
fn pack_str(msg: &mut Message, s: &str) {
    let n = s.len().min(28);
    msg.data[0] = n as u32;
    for (i, &b) in s.as_bytes().iter().take(n).enumerate() {
        msg.data[1 + i / 4] |= (b as u32) << (8 * (i % 4));
    }
}

/// Request/response helper: send, block for the reply, return it.
fn call(dst: u32, msg: &Message) -> Message {
    let rc = sys::send(dst, msg);
    if rc != 0 {
        let mut buf = StrBuf::new();
        buf.push_str("init: send to ");
        buf.push_dec32(dst);
        buf.push_str(" failed (rc=");
        buf.push_dec32(rc as u32);
        buf.push_str(")");
        sys::log(2, buf.as_str());
    }
    let (_src, rep) = sys::receive(M_ANY);
    rep
}

#[no_mangle]
pub extern "C" fn server_main(_info: *const BootInfo) -> ! {
    sys::log(0, "init: up");

    // 1. Spawn the system server set (Minix boot: init starts the servers).
    let pm = sys::spawn("pm");
    let mem = sys::spawn("mem");
    let dev = sys::spawn("dev");
    {
        let mut buf = StrBuf::new();
        buf.push_str("init: spawned pm=");
        buf.push_dec32(pm.max(0) as u32);
        buf.push_str(" mem=");
        buf.push_dec32(mem.max(0) as u32);
        buf.push_str(" dev=");
        buf.push_dec32(dev.max(0) as u32);
        sys::log(0, buf.as_str());
    }

    // 2. Device service: print a banner through the dev server.
    {
        let mut msg = Message::new(M_DEV_WRITE);
        pack_str(&mut msg, "init: banner via dev server");
        let rep = call(dev as u32, &msg);
        if rep.mtype == M_DEV_WRITE_REPLY {
            let mut buf = StrBuf::new();
            buf.push_str("init: dev wrote ");
            buf.push_dec32(rep.data[0]);
            buf.push_str(" bytes");
            sys::log(0, buf.as_str());
        }
    }

    // 3. Memory service: allocate 2 frames, then free them.
    {
        let mut msg = Message::new(M_MEM_ALLOC);
        msg.data[0] = 2;
        let rep = call(mem as u32, &msg);
        if rep.mtype == M_MEM_ALLOC_REPLY {
            let base = ((rep.data[1] as u64) << 32) | rep.data[0] as u64;
            let pages = rep.data[2];
            let mut buf = StrBuf::new();
            buf.push_str("init: mem granted base=");
            buf.push_hex32((base >> 32) as u32);
            buf.push_hex32(base as u32);
            buf.push_str(" pages=");
            buf.push_dec32(pages);
            sys::log(0, buf.as_str());

            let mut free = Message::new(M_MEM_FREE);
            free.data[0] = base as u32;
            free.data[1] = (base >> 32) as u32;
            free.data[2] = pages;
            let rep = call(mem as u32, &free);
            if rep.mtype == M_MEM_FREE_REPLY {
                sys::log(0, "init: mem freed");
            }
        }
    }

    // 4. Process management: ask pm to exec the worker, then wait for its
    //    exit notice (init is the worker's parent).
    {
        let mut msg = Message::new(M_PM_EXEC);
        pack_str(&mut msg, "worker");
        let rep = call(pm as u32, &msg);
        if rep.mtype == M_PM_EXEC_REPLY {
            let mut buf = StrBuf::new();
            buf.push_str("init: pm exec'd worker as task ");
            buf.push_dec32(rep.data[0]);
            sys::log(0, buf.as_str());

            let (_src, note) = sys::receive(M_ANY);
            if note.mtype == M_PM_NOTIFY_EXIT {
                let mut buf = StrBuf::new();
                buf.push_str("init: worker task ");
                buf.push_dec32(note.data[0]);
                buf.push_str(" exited");
                sys::log(0, buf.as_str());
            }
        }
    }

    // 5. Final banner through the device server, then terminate.
    {
        let mut msg = Message::new(M_DEV_WRITE);
        pack_str(&mut msg, "init: boot complete");
        let _ = call(dev as u32, &msg);
    }
    sys::log(0, "init: done, exiting");
    sys::exit();
}
