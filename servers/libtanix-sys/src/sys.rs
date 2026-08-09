//! Safe wrappers over the `svc #0` syscall interface (Phase 6).
//!
//! Syscall numbers match `kernel/src/ipc/syscall.rs`.  The kernel clobbers
//! x1-x18 in the SVC handler, so the inline assembly must list them as
//! clobbered; x30 is preserved by the handler, so it is *not* clobbered.
//! The kernel returns the result in x0.
//!
//! Strings passed to the kernel are copied into a NUL-terminated stack
//! buffer first — the kernel scans up to the NUL byte, so this keeps the
//! boundary safe even for `&str` inputs.

use core::sync::atomic::{AtomicU32, Ordering};
use crate::abi::{BootInfo, Message};

// ── Syscall numbers (must match `kernel/src/ipc/syscall.rs`) ─────────────────

pub const SYS_SEND: u64 = 0;
pub const SYS_RECEIVE: u64 = 1;
pub const SYS_SPAWN: u64 = 2;
pub const SYS_WHO: u64 = 3;
pub const SYS_EXIT_TASK: u64 = 4;
pub const SYS_EXIT: u64 = 5;
pub const SYS_ALLOC_FRAMES: u64 = 6;
pub const SYS_FREE_FRAMES: u64 = 7;
pub const SYS_LOG: u64 = 8;
pub const SYS_WAIT_IRQ: u64 = 9; // Phase 7: block until a device IRQ fires
pub const SYS_YIELD: u64 = 10; // Phase 7: cooperative yield
pub const SYS_SHARE_FRAMES: u64 = 11; // Phase 8: map frames into another task's table
pub const SYS_UNSHARE_FRAMES: u64 = 12; // Phase 8: demote frames in another task's table
pub const SYS_SLEEP: u64 = 13; // Phase 8: block for `ms` scheduler ticks
pub const SYS_EXEC: u64 = 14; // Phase 9: exec an embedded app image (replaces a live instance)
pub const SYS_MAP_DEVICE: u64 = 15; // Phase 10: identity-map a device-MMIO window (PCI ECAM/BARs)
pub const SYS_IRQ_PENDING: u64 = 16; // Phase 10: non-blocking "device IRQ delivered?" poll

/// Invoke syscall `nr` with arguments `a0..a2`; returns the kernel's x0.
///
/// # Safety
/// `a0..a2` must match the syscall's ABI (pointer arguments must be valid
/// in this task's address space for the duration of the call).
#[inline]
unsafe fn raw_syscall(nr: u64, a0: u64, a1: u64, a2: u64) -> u64 {
    let r: u64;
    core::arch::asm!(
        "svc #0",
        inlateout("x0") nr => r,
        in("x1") a0,
        in("x2") a1,
        in("x3") a2,
        lateout("x1") _,
        lateout("x2") _,
        lateout("x3") _,
        clobber_abi("C"),
        options(nostack),
    );
    r
}

/// Read the current task's boot info (own task id).
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

/// `send(dst, msg)` — blocking rendezvous.  Returns `0` on success.
pub fn send(dst: u32, msg: &Message) -> i32 {
    unsafe { raw_syscall(SYS_SEND, dst as u64, msg as *const Message as u64, 0) as i32 }
}

/// `receive(filter)` — blocks until a message arrives; returns (src, msg).
pub fn receive(filter: i32) -> (u32, Message) {
    let mut msg = Message::new(0);
    let rc = unsafe {
        raw_syscall(SYS_RECEIVE, filter as u64, &mut msg as *mut Message as u64, 0)
    };
    ((rc as i32).max(0) as u32, msg)
}

/// `spawn(name)` — start a registered server.  Returns its task id or -errno.
pub fn spawn(name: &str) -> i32 {
    let mut buf = [0u8; 16];
    let n = name.len().min(buf.len() - 1);
    buf[..n].copy_from_slice(&name.as_bytes()[..n]);
    unsafe { raw_syscall(SYS_SPAWN, buf.as_ptr() as u64, 0, 0) as i32 }
}

/// `who(name)` — resolve a server name to its task id (-1 if unknown).
pub fn who(name: &str) -> i32 {
    let mut buf = [0u8; 16];
    let n = name.len().min(buf.len() - 1);
    buf[..n].copy_from_slice(&name.as_bytes()[..n]);
    unsafe { raw_syscall(SYS_WHO, buf.as_ptr() as u64, 0, 0) as i32 }
}

/// `exit_task(pid)` — kill another task.  Returns `0` on success.
pub fn exit_task(pid: u32) -> i32 {
    unsafe { raw_syscall(SYS_EXIT_TASK, pid as u64, 0, 0) as i32 }
}

/// `exit()` — terminate this task.
pub fn exit() -> ! {
    unsafe { raw_syscall(SYS_EXIT, 0, 0, 0) };
    unreachable!()
}

/// `alloc_frames(n)` — physical base of n contiguous frames (0 = OOM).
///
/// The kernel maps the frames into this task's address space (identity
/// VA == phys), so the returned base is directly dereferenceable — same
/// contract as before the EL0 split.
pub fn alloc_frames(n: u32) -> u64 {
    unsafe { raw_syscall(SYS_ALLOC_FRAMES, n as u64, 0, 0) }
}

/// `free_frames(base, n)`.
pub fn free_frames(base: u64, n: u32) -> i32 {
    unsafe { raw_syscall(SYS_FREE_FRAMES, base, n as u64, 0) as i32 }
}

/// `share_frames(base, pages, task)` — Phase 8.  Maps the frame run
/// `base .. base+pages*4096` into `task`'s address space (identity VA ==
/// phys), so the two servers can share a buffer.  The caller must own the
/// frames (`alloc_frames`).
pub fn share_frames(base: u64, pages: u32, task: u32) -> i32 {
    unsafe { raw_syscall(SYS_SHARE_FRAMES, base, pages as u64, task as u64) as i32 }
}

/// `unshare_frames(base, pages, task)` — Phase 8 mirror: demote the run in
/// `task`'s table (the owning task keeps its own mapping; free the frames
/// only after unsharing).
pub fn unshare_frames(base: u64, pages: u32, task: u32) -> i32 {
    unsafe { raw_syscall(SYS_UNSHARE_FRAMES, base, pages as u64, task as u64) as i32 }
}

/// `sleep(ms)` — Phase 8.  Blocks this task for roughly `ms` milliseconds
/// (scheduler ticks are 1 ms).  Returns `0` on success.
pub fn sleep(ms: u32) -> i32 {
    unsafe { raw_syscall(SYS_SLEEP, ms as u64, 0, 0) as i32 }
}

/// `exec(name)` — Phase 9.  Start an app from the kernel's embedded image
/// registry, replacing any live instance of the same app.  Returns the new
/// task id, or a negative error.
pub fn exec(name: &str) -> i32 {
    let mut buf = [0u8; 16];
    let n = name.len().min(buf.len() - 1);
    buf[..n].copy_from_slice(&name.as_bytes()[..n]);
    unsafe { raw_syscall(SYS_EXEC, buf.as_ptr() as u64, 0, 0) as i32 }
}

/// `map_device(phys, pages)` — Phase 10.  Identity-maps the device-MMIO
/// window `[phys .. phys + pages*4096)` into this task's address space as
/// EL0-visible Device-nGnRnE memory (also into the kernel's table).  Use
/// for PCI windows the kernel does not pre-map (ECAM, BARs).  Returns `0`
/// on success, negative errno otherwise.
pub fn map_device(phys: u64, pages: u32) -> i32 {
    unsafe { raw_syscall(SYS_MAP_DEVICE, phys, pages as u64, 0) as i32 }
}

/// `log(level, msg)` — kernel log line, prefixed with this server's name.
pub fn log(level: u32, msg: &str) {
    let mut buf = [0u8; 128];
    let n = msg.len().min(buf.len() - 1);
    buf[..n].copy_from_slice(&msg.as_bytes()[..n]);
    unsafe { raw_syscall(SYS_LOG, level as u64, buf.as_ptr() as u64, 0) };
}

/// `wait_irq(irq)` — Phase 7.  Blocks until the device interrupt `irq` has
/// been delivered; the kernel enables it in the GIC on the first wait.
/// Returns `0` on success (negative = invalid IRQ).
///
/// Intended for virtio-mmio devices: `irq = 48 + slot` (see the display
/// server's virtio driver).
pub fn wait_irq(irq: u32) -> i32 {
    unsafe { raw_syscall(SYS_WAIT_IRQ, irq as u64, 0, 0) as i32 }
}

/// `irq_pending(irq) -> 1|0` — Phase 10.  Non-blocking poll: arms the
/// interrupt (like `wait_irq`) and returns whether it has been delivered
/// since the last call, without sleeping.  For event-loop servers that
/// keep their own timing (the net server's virtio-pci INTx line).
pub fn irq_pending(irq: u32) -> i32 {
    unsafe { raw_syscall(SYS_IRQ_PENDING, irq as u64, 0, 0) as i32 }
}

/// `yield_cpu()` — Phase 7.  Cooperative yield: hand the CPU to a strictly
/// higher-priority runnable task, if any (equal-priority rotation happens
/// on the preemption tick).
pub fn yield_cpu() {
    unsafe { raw_syscall(SYS_YIELD, 0, 0, 0) };
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
