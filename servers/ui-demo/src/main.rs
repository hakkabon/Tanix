//! Phase-5 demo app: a small pointer-reactive UI drawn through the display
//! server.
//!
//! Shows the full stack end-to-end: the app draws with `M_DISPLAY_FILL_RECT`
//! requests, presents with `M_DISPLAY_FLUSH`, and samples the virtio-tablet
//! pointer with `M_DISPLAY_TICK`.  A button in the top-right toggles the
//! background colour; pressing elsewhere paints an amber dot (drag to
//! paint); the pointer itself is a white ring that follows the tablet.

#![no_std]
#![no_main]

use tanix_libsys::abi::{
    BootInfo, Message, M_ANY, M_DISPLAY_DONE, M_DISPLAY_FILL_RECT, M_DISPLAY_FLUSH,
    M_DISPLAY_GET_MODE, M_DISPLAY_MODE_REPLY, M_DISPLAY_TICK, M_DISPLAY_TICK_REPLY,
};
use tanix_libsys::sys;

/// Painted-dot history ring.
const DOTS: usize = 128;

#[derive(Clone, Copy, Default)]
struct Point {
    x: u32,
    y: u32,
}

/// Send `mtype` to the display server and return its reply.
fn rpc(display: u32, mtype: u32, data: &[u32]) -> Message {
    let mut m = Message::new(mtype);
    for (i, v) in data.iter().take(8).enumerate() {
        m.data[i] = *v;
    }
    sys::send(display, &m);
    let (_, reply) = sys::receive(M_ANY);
    reply
}

/// Fill a rectangle through the display server.
fn fill(display: u32, x: u32, y: u32, w: u32, h: u32, rgb: (u8, u8, u8)) {
    let _ = rpc(
        display,
        M_DISPLAY_FILL_RECT,
        &[x, y, w, h, rgb.0 as u32, rgb.1 as u32, rgb.2 as u32],
    );
}

#[no_mangle]
pub extern "C" fn server_main(_info: *const BootInfo) -> ! {
    sys::log(0, "ui-demo: up");

    let display = sys::who("display");
    if display < 0 {
        sys::log(1, "ui-demo: display server not found — idling");
        loop {
            let _ = sys::receive(M_ANY);
        }
    }
    let display = display as u32;

    let mode = rpc(display, M_DISPLAY_GET_MODE, &[]);
    let w = mode.data[0];
    let h = mode.data[1];

    // Button area (top-right corner).
    let btn = (w - 120, 8, 110, 36);
    let btn_fill = (0x40, 0x70, 0xc0);

    let mut bg: (u8, u8, u8) = (0x20, 0x22, 0x2a);
    let mut dots = [Point::default(); DOTS];
    let mut dot_next = 0usize;
    let mut dot_count = 0usize;

    // Redraw whenever the pointer moved or the button state changed.
    let mut last = (u32::MAX, u32::MAX, u32::MAX);

    loop {
        let m = rpc(display, M_DISPLAY_TICK, &[]);
        let (px, py, pb) = (m.data[0], m.data[1], m.data[2]);
        let old_pos = (last.0, last.1);
        let pressed_edge = pb & 1 == 1 && last.2 & 1 == 0;
        let changed = (px, py) != old_pos || pressed_edge;
        last = (px, py, pb);

        if pb & 1 == 1 {
            let moved_while_pressed = (px, py) != old_pos;
            if pressed_edge && px >= btn.0 && px < btn.0 + btn.2 && py >= btn.1 && py < btn.1 + btn.3
            {
                // Button toggle: flip the background colour.
                bg = if bg == (0x20, 0x22, 0x2a) {
                    (0x12, 0x10, 0x1c)
                } else {
                    (0x20, 0x22, 0x2a)
                };
            } else if pressed_edge || moved_while_pressed {
                // Paint a dot at the pointer (press, or drag = finger painting).
                dots[dot_next] = Point { x: px, y: py };
                dot_next = (dot_next + 1) % DOTS;
                if dot_count < DOTS {
                    dot_count += 1;
                }
            }
        }

        if !changed && pb & 1 == 0 {
            continue;
        }

        // Full redraw.
        fill(display, 0, 0, w, h, bg);
        fill(display, btn.0, btn.1, btn.2, btn.3, btn_fill);
        for d in dots[..dot_count].iter() {
            fill(display, d.x - 3, d.y - 3, 7, 7, (0xe8, 0xa0, 0x30));
        }
        // Pointer cursor: white ring on a black hole.
        let cx = px.max(4).min(w.saturating_sub(5));
        let cy = py.max(4).min(h.saturating_sub(5));
        fill(display, cx - 4, cy - 4, 9, 9, (0xff, 0xff, 0xff));
        fill(display, cx - 3, cy - 3, 7, 7, (0x00, 0x00, 0x00));

        let _ = rpc(display, M_DISPLAY_FLUSH, &[]);
    }
}
