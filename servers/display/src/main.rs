//! Display server (`display`) — owns the QEMU virtio-gpu framebuffer and
//! the virtio-tablet pointer (Phase 5).
//!
//! Minix-style: no process touches the GPU or input devices directly; UI
//! clients send `M_DISPLAY_*` messages and get replies.  Rendering happens
//! into the server's framebuffer; the server owns the whole display stack:
//! virtio-mmio transport, virtio-gpu driver, bitmap text, and input events.
//!
//! The server is fully receive-driven: it parks on `receive` and serves one
//! request per wake-up.  On every `M_DISPLAY_TICK` it drains the tablet's
//! event queue and reports the latest pointer position, so the client's
//! own request/response cadence doubles as the input polling loop.

#![no_std]
#![no_main]

mod gpu;
mod input;
mod virtio;

use tanix_libsys::abi::{
    BootInfo, Message, M_ANY, M_DISPLAY_BLIT, M_DISPLAY_DONE, M_DISPLAY_DRAW_TEXT,
    M_DISPLAY_FILL_RECT, M_DISPLAY_FLUSH, M_DISPLAY_GET_MODE, M_DISPLAY_MODE_REPLY,
    M_DISPLAY_TICK, M_DISPLAY_TICK_REPLY,
};
use tanix_libsys::sys;

/// Send a reply to `dst`.  Blocks until the receiver picks it up, which it
/// always does — the client is parked in `receive` waiting for it.
fn reply(dst: u32, mtype: u32, data: &[u32]) {
    let mut m = Message::new(mtype);
    for (i, v) in data.iter().take(8).enumerate() {
        m.data[i] = *v;
    }
    sys::send(dst, &m);
}

#[no_mangle]
pub extern "C" fn server_main(_info: *const BootInfo) -> ! {
    sys::log(0, "display: up");

    let mut gpu = match gpu::Gpu::open() {
        Some(g) => g,
        None => {
            sys::log(1, "display: no virtio-gpu found — idling");
            loop {
                let _ = sys::receive(M_ANY);
            }
        }
    };
    sys::log(0, "display: virtio-gpu online");

    let mode = match gpu.display_info() {
        Some(m) => m,
        None => {
            sys::log(1, "display: no enabled scanout — idling");
            loop {
                let _ = sys::receive(M_ANY);
            }
        }
    };

    if gpu.setup_framebuffer(&mode).is_none() {
        sys::log(1, "display: framebuffer setup failed — idling");
        loop {
            let _ = sys::receive(M_ANY);
        }
    }
    sys::log(0, "display: framebuffer ready");

    // Boot screen: plain slate, presented before any client draws.
    gpu.fill_rect(0, 0, mode.width, mode.height, (0x20, 0x22, 0x2a));
    let _ = gpu.flush();

    let mut tablet = match input::Tablet::open() {
        Some(t) => {
            sys::log(0, "display: virtio-tablet online");
            Some(t)
        }
        None => {
            sys::log(1, "display: no virtio-tablet found — pointer reports (0,0)");
            None
        }
    };

    // Service loop.
    loop {
        let (src, msg) = sys::receive(M_ANY);
        match msg.mtype {
            M_DISPLAY_GET_MODE => {
                reply(src, M_DISPLAY_MODE_REPLY, &[mode.width, mode.height]);
            }

            M_DISPLAY_FILL_RECT => {
                gpu.fill_rect(
                    msg.data[0],
                    msg.data[1],
                    msg.data[2],
                    msg.data[3],
                    (msg.data[4] as u8, msg.data[5] as u8, msg.data[6] as u8),
                );
                reply(src, M_DISPLAY_DONE, &[1]);
            }

            M_DISPLAY_FLUSH => {
                let ok = gpu.flush().is_some() as u32;
                reply(src, M_DISPLAY_DONE, &[ok]);
            }

            M_DISPLAY_TICK => {
                let p = match tablet.as_mut() {
                    Some(t) => t.poll(),
                    None => input::Pointer { x: 0, y: 0, buttons: 0 },
                };
                let px = p.x * mode.width / input::ABS_MAX;
                let py = p.y * mode.height / input::ABS_MAX;
                reply(src, M_DISPLAY_TICK_REPLY, &[px, py, p.buttons]);
            }

            M_DISPLAY_BLIT => {
                // Phase 8 compositor: copy a shared window canvas rect into
                // the framebuffer.  data[0,1]=src base, data[2..4]=src
                // x,y,w,h, data[5,6]=dst x,y.  The src canvas width (row
                // stride) is not carried — the wm always blits full-window
                // rows, so the stride equals the src width × 4.
                let base = ((msg.data[1] as u64) << 32) | msg.data[0] as u64;
                let (sx, sy, sw, sh) = (msg.data[2], msg.data[3], msg.data[4], msg.data[5]);
                let (dx, dy) = (msg.data[6], msg.data[7]);
                gpu.blit(
                    base as *const u8,
                    sw as usize * 4,
                    sx,
                    sy,
                    sw,
                    sh,
                    dx,
                    dy,
                );
                reply(src, M_DISPLAY_DONE, &[1]);
            }

            M_DISPLAY_DRAW_TEXT => {
                // Phase 8 title bars: data[0]=x, data[1]=y, data[2]=rgb,
                // data[3]=len, data[4..8]=chars (up to 16).
                let (x, y, rgb, len) = (msg.data[0], msg.data[1], msg.data[2], msg.data[3]);
                let mut chars = [0u8; 16];
                for (i, ch) in chars.iter_mut().take(len.min(16) as usize).enumerate() {
                    *ch = (msg.data[4 + i / 4] >> (8 * (i % 4))) as u8;
                }
                let mut s = [0u8; 17];
                let n = len.min(16) as usize;
                s[..n].copy_from_slice(&chars[..n]);
                gpu.draw_text(
                    x,
                    y,
                    ((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8),
                    core::str::from_utf8(&s[..n]).unwrap_or(""),
                );
                reply(src, M_DISPLAY_DONE, &[1]);
            }

            _ => {
                // Unknown request — drop it without replying.
            }
        }
    }
}
