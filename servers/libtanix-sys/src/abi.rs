//! ABI types and protocol constants (mirror of `kernel/src/sched/mod.rs`).

/// A fixed-size IPC message: 32 bytes of payload in 8 u32 words.
///
/// Long strings travel inline (up to 28 chars + NUL), exactly like classic
/// Minix messages.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct Message {
    /// Stamped by the kernel — the sender's task id.
    pub src: u32,
    /// Message type (see constants below).
    pub mtype: u32,
    /// Payload words, interpreted per `mtype`.
    pub data: [u32; 8],
}

impl Message {
    pub const fn new(mtype: u32) -> Self {
        Self { src: 0, mtype, data: [0u32; 8] }
    }
}

/// Sentinel filter for `receive`: accept any sender.
pub const M_ANY: i32 = -1;

/// The kernel's syscall table, handed to every server at boot.
#[repr(C)]
pub struct SyscallTable {
    pub send: unsafe extern "C" fn(u32, *const Message) -> i32,
    pub receive: unsafe extern "C" fn(i32, *mut Message) -> i32,
    pub spawn: unsafe extern "C" fn(*const u8) -> i32,
    pub who: unsafe extern "C" fn(*const u8) -> i32,
    pub exit_task: unsafe extern "C" fn(u32) -> i32,
    pub exit: unsafe extern "C" fn() -> !,
    pub alloc_frames: unsafe extern "C" fn(u32) -> u64,
    pub free_frames: unsafe extern "C" fn(u64, u32) -> i32,
    pub log: unsafe extern "C" fn(u32, *const u8),
}

/// Boot info block: the server receives this in its callee-saved x19.
#[repr(C)]
pub struct BootInfo {
    pub syscalls: *const SyscallTable,
    pub task_id: u32,
}

// ── Per-server protocol constants ─────────────────────────────────────────────

/// Device server (`dev`) — owns UART0.
pub const M_DEV_WRITE: u32 = 0x0100;
pub const M_DEV_WRITE_REPLY: u32 = 0x0101;

/// Memory service (`mem`) — frame allocator behind a message boundary.
pub const M_MEM_ALLOC: u32 = 0x0200;
pub const M_MEM_ALLOC_REPLY: u32 = 0x0201;
pub const M_MEM_FREE: u32 = 0x0202;
pub const M_MEM_FREE_REPLY: u32 = 0x0203;

/// Process manager (`pm`) — spawn / kill.
pub const M_PM_EXEC: u32 = 0x0300;
pub const M_PM_EXEC_REPLY: u32 = 0x0301;
pub const M_PM_EXIT: u32 = 0x0302;
pub const M_PM_EXIT_REPLY: u32 = 0x0303;
pub const M_PM_NOTIFY_EXIT: u32 = 0x0304;

/// Maximum string payload carried inside a message (28 chars + NUL).
pub const MAX_INLINE_STR: usize = 28;
