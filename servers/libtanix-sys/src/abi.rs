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

/// Boot info block: the server receives this in its callee-saved x19.
///
/// Phase 6: servers run at EL0 and call the kernel with `svc #0` — the
/// syscall number goes in x0, arguments in x1-x3, result back in x0 (see
/// `sys.rs` for the numbers).  There is no function-pointer table anymore.
#[repr(C)]
pub struct BootInfo {
    /// This task's own id.
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

/// Display server (`display`) — owns the virtio-gpu framebuffer and the
/// virtio-tablet pointer.  Clients draw with FILL_RECT, present with FLUSH,
/// and sample the pointer with TICK (the display server drains the tablet
/// event queue on every TICK and reports the latest position).  Phase 8
/// adds BLIT (the compositor copies window canvases into the framebuffer)
/// and DRAW_TEXT (title bars — the font lives in libtanix-ui).
pub const M_DISPLAY_GET_MODE: u32 = 0x0500;
pub const M_DISPLAY_MODE_REPLY: u32 = 0x0501; // data[0]=width, data[1]=height
pub const M_DISPLAY_FILL_RECT: u32 = 0x0502; // data[0..3]=x,y,w,h, data[4..6]=r,g,b
pub const M_DISPLAY_DONE: u32 = 0x0503; // reply to FILL_RECT / FLUSH / BLIT / DRAW_TEXT (data[0]=ok)
pub const M_DISPLAY_FLUSH: u32 = 0x0504;
pub const M_DISPLAY_TICK: u32 = 0x0505;
pub const M_DISPLAY_TICK_REPLY: u32 = 0x0506; // data[0]=pointer x, data[1]=pointer y, data[2]=buttons
pub const M_DISPLAY_BLIT: u32 = 0x0507; // data[0,1]=src base, data[2..4]=src x,y,w,h, data[5,6]=dst x,y
pub const M_DISPLAY_DRAW_TEXT: u32 = 0x0508; // data[0]=x, data[1]=y, data[2]=rgb, data[3]=len, data[4..8]=chars

/// Window manager (`wm`) — Phase 8 compositor.  Apps create a window, get a
/// winid + screen placement, draw into their own canvas (allocated and
/// shared with the display server by the app), then FLUSH to composite and
/// TICK to receive routed pointer events in window coordinates.
pub const M_WM_CREATE: u32 = 0x0600; // data[0]=w, data[1]=h, data[2,3]=canvas base, data[4]=pages, data[5..8]=title (12 chars)
pub const M_WM_CREATE_REPLY: u32 = 0x0601; // data[0]=winid, data[1]=x, data[2]=y, data[3]=w, data[4]=h, data[5]=ok
pub const M_WM_CLOSE: u32 = 0x0602; // data[0]=winid
pub const M_WM_DONE: u32 = 0x0603; // reply to CLOSE / FLUSH (data[0]=ok)
pub const M_WM_FLUSH: u32 = 0x0604; // data[0]=winid — composite + present
pub const M_WM_TICK: u32 = 0x0605; // data[0]=winid — sample pointer, route events
pub const M_WM_TICK_REPLY: u32 = 0x0606; // data[0,1]=px,py (window-local, 0xFFFFFFFF=no event), data[2]=buttons, data[3]=focused
pub const M_WM_NOTIFY: u32 = 0x0607; // wm → app (unsolicited, receive-time): data[0]=winid, data[1]=x, data[2]=y, data[3]=w, data[4]=h

/// Maximum string payload carried inside a message (28 chars + NUL).
pub const MAX_INLINE_STR: usize = 28;
