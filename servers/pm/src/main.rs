//! Process manager (`pm`) — the Minix PM analog.
//!
//! Clients send `M_PM_EXEC` (spawn a registered server, reply carries the
//! new task id) or `M_PM_EXIT` (terminate a task, reply carries the return
//! code).  When a task exits, `pm` also notifies the requester with
//! `M_PM_NOTIFY_EXIT` so process hierarchies can track their children.

#![no_std]
#![no_main]

use tanix_libsys::abi::{
    BootInfo, Message, M_ANY, M_PM_EXEC, M_PM_EXEC_REPLY, M_PM_EXIT,
    M_PM_EXIT_REPLY, M_PM_NOTIFY_EXIT,
};
use tanix_libsys::{fmt::StrBuf, sys};

/// (pid → parent) records for tasks spawned via `M_PM_EXEC`.  A dying task's
/// parent is the one that asked pm to exec it (init for the demo worker).
static mut PARENTS: [(u32, u32); 8] = [(0, 0); 8];

fn record_child(pid: u32, parent: u32) {
    // SAFETY: single-threaded cooperative execution.
    let slots: &mut [(u32, u32); 8] = unsafe { &mut *core::ptr::addr_of_mut!(PARENTS) };
    for slot in slots.iter_mut() {
        if slot.0 == 0 {
            *slot = (pid, parent);
            return;
        }
    }
}

fn parent_of(pid: u32) -> u32 {
    // SAFETY: single-threaded cooperative execution.
    let slots: &[(u32, u32); 8] = unsafe { &*core::ptr::addr_of!(PARENTS) };
    for (child, parent) in slots.iter() {
        if *child == pid {
            return *parent;
        }
    }
    0
}

#[no_mangle]
pub extern "C" fn server_main(_info: *const BootInfo) -> ! {
    sys::log(0, "pm: up");
    loop {
        let (src, msg) = sys::receive(M_ANY);
        match msg.mtype {
            M_PM_EXEC => {
                // data[1..] = NUL-terminated server name (up to 12 bytes).
                let mut name = [0u8; 12];
                for (i, slot) in name.iter_mut().enumerate() {
                    *slot = (msg.data[1 + i / 4] >> (8 * (i % 4))) as u8;
                }
                let name_len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
                let name_str = core::str::from_utf8(&name[..name_len]).unwrap_or("?");

                let pid = sys::spawn(name_str);
                record_child(pid.max(0) as u32, src);
                let mut rep = Message::new(M_PM_EXEC_REPLY);
                rep.data[0] = pid as u32;
                sys::send(src, &rep);
            }
            M_PM_EXIT => {
                let pid = msg.data[0];
                let parent = parent_of(pid);

                // Reply to the dying task *before* reaping it: `exit_task`
                // clears its receive state, so an earlier kill would make the
                // reply queue up and deadlock pm.
                let mut rep = Message::new(M_PM_EXIT_REPLY);
                rep.data[0] = 0;
                sys::send(src, &rep);
                let rc = sys::exit_task(pid);

                // Notify the parent that the child is gone.
                if rc == 0 && parent != 0 {
                    let mut note = Message::new(M_PM_NOTIFY_EXIT);
                    note.data[0] = pid;
                    sys::send(parent, &note);
                }
            }
            _ => {
                let mut buf = StrBuf::new();
                buf.push_str("pm: unknown message type 0x");
                buf.push_hex32(msg.mtype);
                sys::log(1, buf.as_str());
            }
        }
    }
}
