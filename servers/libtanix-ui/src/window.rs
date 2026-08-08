//! Window handle + canvas raster helpers for Phase-8 apps.
//!
//! An app window is an off-screen BGRA8888 canvas the app owns (frames
//! allocated with `SYS_ALLOC_FRAMES`, shared with the display server so the
//! compositor can blit it).  The window manager (`wm`) keeps the window
//! table: screen placement, z-order, dragging, and pointer routing.  Apps
//! only ever see window-local coordinates.
//!
//! Lifecycle: `Window::create` (alloc + share + register) → draw into
//! `Window::canvas` → `flush` (composite + present) → `tick` (routed
//! pointer event, or None) → `close` (unshare + free + unregister).

use tanix_libsys::abi::{
    M_ANY, M_WM_CLOSE, M_WM_CREATE, M_WM_CREATE_REPLY, M_WM_FLUSH, M_WM_TICK, M_WM_TICK_REPLY,
};
use tanix_libsys::{sys, Message};
use crate::font;

/// A routed pointer event, in window-local pixels.
#[derive(Clone, Copy, Debug)]
pub struct PointerEvent {
    pub x: u32,
    pub y: u32,
    pub buttons: u32,
    /// True when this window is the topmost one under the cursor.
    pub focused: bool,
}

/// RPC helper: send and block for the reply, retrying when the receiver's
/// send queue is momentarily full (a compositor busy compositing another
/// app's frame).
pub fn rpc(dst: u32, msg: &Message) -> Message {
    loop {
        let rc = sys::send(dst, msg);
        if rc == 0 {
            break;
        }
        for _ in 0..4096 {
            core::hint::spin_loop();
        }
        sys::yield_cpu();
    }
    let (_src, rep) = sys::receive(M_ANY);
    rep
}

/// An open window.  The canvas pointer is valid while the window lives.
pub struct Window {
    /// Window id, assigned by the window manager.
    pub winid: u32,
    /// Physical base of the BGRA8888 canvas (mapped in this app's table).
    pub canvas: u64,
    /// Canvas size in pixels.
    pub w: u32,
    pub h: u32,
    pages: u32,
    wm: u32,
    display: u32,
}

impl Window {
    /// Create a window: allocate the canvas, share it with the display
    /// server, and register it with the window manager.  Returns the
    /// window (with its on-screen placement) or `None` if the display
    /// stack is not up.
    pub fn create(title: &str, w: u32, h: u32) -> Option<Window> {
        let wm = sys::who("wm");
        let display = sys::who("display");
        if wm < 0 || display < 0 {
            return None;
        }
        let wm = wm as u32;
        let display = display as u32;

        let bytes = (w as usize) * (h as usize) * 4;
        let pages = bytes.div_ceil(4096);
        let canvas = sys::alloc_frames(pages as u32);
        if canvas == 0 {
            return None;
        }
        if sys::share_frames(canvas, pages as u32, display) != 0 {
            sys::free_frames(canvas, pages as u32);
            return None;
        }
        let mut m = Message::new(M_WM_CREATE);
        m.data[0] = w;
        m.data[1] = h;
        m.data[2] = canvas as u32;
        m.data[3] = (canvas >> 32) as u32;
        m.data[4] = pages as u32;
        let t = title.as_bytes();
        for (i, &b) in t.iter().take(12).enumerate() {
            m.data[5 + i / 4] |= (b as u32) << (8 * (i % 4));
        }
        let rep = rpc(wm, &m);
        if rep.mtype != M_WM_CREATE_REPLY || rep.data[5] != 1 {
            sys::unshare_frames(canvas, pages as u32, display);
            sys::free_frames(canvas, pages as u32);
            return None;
        }

        Some(Window {
            winid: rep.data[0],
            canvas,
            w,
            h,
            pages: pages as u32,
            wm,
            display,
        })
    }

    /// Mutable view over the whole canvas (BGRA8888).
    pub fn canvas_mut(&mut self) -> &mut [u8] {
        let len = (self.w as usize) * (self.h as usize) * 4;
        unsafe { core::slice::from_raw_parts_mut(self.canvas as *mut u8, len) }
    }

    /// Fill a rectangle with an RGB colour.
    pub fn fill_rect(&mut self, x: i32, y: i32, w: u32, h: u32, rgb: (u8, u8, u8)) {
        let stride = self.w as usize * 4;
        let (r, g, b) = rgb;
        let (wmax, hmax) = (self.w, self.h);
        let fb = self.canvas_mut();
        let (x0, y0) = (x.max(0) as usize, y.max(0) as usize);
        let x1 = (x + w as i32).min(wmax as i32).max(0) as usize;
        let y1 = (y + h as i32).min(hmax as i32).max(0) as usize;
        for row in y0..y1 {
            for col in x0..x1 {
                let off = row * stride + col * 4;
                fb[off] = b;
                fb[off + 1] = g;
                fb[off + 2] = r;
                fb[off + 3] = 0xFF;
            }
        }
    }

    /// Clear the whole canvas.
    pub fn clear(&mut self, rgb: (u8, u8, u8)) {
        self.fill_rect(0, 0, self.w, self.h, rgb);
    }

    /// Draw text into the canvas with the shared 5×7 font.
    pub fn draw_text(&mut self, x: i32, y: i32, scale: u32, rgb: (u8, u8, u8), s: &str) {
        let stride = self.w as usize * 4;
        let fb = self.canvas_mut();
        font::draw_str(fb, stride, x, y, scale, rgb, s);
    }

    /// Draw a thick line (2 px) with Bresenham — clock hands etc.
    pub fn draw_line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, rgb: (u8, u8, u8)) {
        let (mut x, mut y) = (x0, y0);
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        loop {
            self.fill_rect(x - 1, y - 1, 3, 3, rgb);
            if x == x1 && y == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
    }

    /// Sample the pointer.  Returns a routed event when the cursor is over
    /// this window (the wm's topmost hit), otherwise `None`.
    pub fn tick(&mut self) -> Option<PointerEvent> {
        let mut m = Message::new(M_WM_TICK);
        m.data[0] = self.winid;
        let rep = rpc(self.wm, &m);
        if rep.mtype != M_WM_TICK_REPLY || rep.data[0] == u32::MAX {
            return None;
        }
        Some(PointerEvent {
            x: rep.data[0],
            y: rep.data[1],
            buttons: rep.data[2],
            focused: rep.data[3] == 1,
        })
    }

    /// Composite this window onto the scanout and present.
    pub fn flush(&mut self) {
        let mut m = Message::new(M_WM_FLUSH);
        m.data[0] = self.winid;
        let _ = rpc(self.wm, &m);
    }

    /// Destroy the window, unshare the canvas from the display server and
    /// return its frames.
    pub fn close(&mut self) {
        let mut m = Message::new(M_WM_CLOSE);
        m.data[0] = self.winid;
        let _ = rpc(self.wm, &m);
        sys::unshare_frames(self.canvas, self.pages, self.display);
        sys::free_frames(self.canvas, self.pages);
    }
}
