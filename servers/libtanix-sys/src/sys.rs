//! Safe wrappers over the raw syscall table.
//!
//! Strings passed to the kernel are copied into a NUL-terminated stack
//! buffer first — the kernel scans up to the NUL byte, so this keeps the
//! boundary safe even for `&str` inputs.

use core::sync::atomic::{AtomicU32, Ordering};
use crate::abi::{BootInfo, Message, SyscallTable};

/// Read the current task's boot info (syscall table + own task id).
///
/// # Safety
/// Called once from the entry stub, which received the pointer in x19.
///
/// x19 is only trustworthy at the very first call (`_start` runs before any
/// function prologue has had a chance to reuse it), so the pointer is
/// cached in `.bss` on the first read.
pub unsafe fn boot_info() -> &'static BootInfo {
    static mut CACHE: *const BootInfo = core::ptr::null();
    if CACHE.is_null() {
        let ptr: usize;
        core::arch::asm!("mov {0}, x19", out(reg) ptr, options(nomem, nostack));
        CACHE = ptr as *const BootInfo;
    }
    // SAFETY: written once above, single-threaded cooperative execution.
    unsafe { &*CACHE }
}

/// The syscall table for this task (from boot info).
pub fn table() -> &'static SyscallTable {
    // Boot info is written once at spawn; reading it after the entry stub
    // ran is sound.
    unsafe { &*boot_info().syscalls }
}

/// `send(dst, msg)` — blocking rendezvous.  Returns `0` on success.
pub fn send(dst: u32, msg: &Message) -> i32 {
    unsafe { (table().send)(dst, msg as *const Message) }
}

/// `receive(filter)` — blocks until a message arrives; returns (src, msg).
pub fn receive(filter: i32) -> (u32, Message) {
    let mut msg = Message::new(0);
    let rc = unsafe { (table().receive)(filter, &mut msg) };
    ((rc.max(0) as u32), msg)
}

/// `spawn(name)` — start a registered server.  Returns its task id or -errno.
pub fn spawn(name: &str) -> i32 {
    let mut buf = [0u8; 16];
    let n = name.len().min(buf.len() - 1);
    buf[..n].copy_from_slice(&name.as_bytes()[..n]);
    unsafe { (table().spawn)(buf.as_ptr()) }
}

/// `who(name)` — resolve a server name to its task id (-1 if unknown).
pub fn who(name: &str) -> i32 {
    let mut buf = [0u8; 16];
    let n = name.len().min(buf.len() - 1);
    buf[..n].copy_from_slice(&name.as_bytes()[..n]);
    unsafe { (table().who)(buf.as_ptr()) }
}

/// `exit_task(pid)` — kill another task.  Returns `0` on success.
pub fn exit_task(pid: u32) -> i32 {
    unsafe { (table().exit_task)(pid) }
}

/// `exit()` — terminate this task.
pub fn exit() -> ! {
    unsafe { (table().exit)() }
}

/// `alloc_frames(n)` — physical base of n contiguous frames (0 = OOM).
pub fn alloc_frames(n: u32) -> u64 {
    unsafe { (table().alloc_frames)(n) }
}

/// `free_frames(base, n)`.
pub fn free_frames(base: u64, n: u32) -> i32 {
    unsafe { (table().free_frames)(base, n) }
}

/// `log(level, msg)` — kernel log line, prefixed with this server's name.
pub fn log(level: u32, msg: &str) {
    let mut buf = [0u8; 128];
    let n = msg.len().min(buf.len() - 1);
    buf[..n].copy_from_slice(&msg.as_bytes()[..n]);
    unsafe { (table().log)(level, buf.as_ptr()) };
}

/// Own-task id, cached from boot info (cheap).
pub fn task_id() -> u32 {
    static CACHE: AtomicU32 = AtomicU32::new(0);
    let cached = CACHE.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    let id = unsafe { boot_info().task_id };
    CACHE.store(id, Ordering::Relaxed);
    id
}
