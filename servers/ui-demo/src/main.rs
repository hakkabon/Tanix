//! Phase-8 demo app "paint": a pointer-reactive canvas in its own window.
//!
//! The app owns a window canvas (off-screen buffer), draws into it with
//! fill operations, and presents through the window manager (`wm`), which
//! routes tablet events into window-local coordinates.  A button in the
//! top-right toggles the background colour; pressing or dragging elsewhere
//! paints amber dots.

#![no_std]
#![no_main]

use tanix_libsys::{abi::BootInfo, fmt::StrBuf, sys};
use tanix_libtanix_ui::Window;

/// Painted-dot history ring.
const DOTS: usize = 128;

#[derive(Clone, Copy, Default)]
struct Point {
    x: i32,
    y: i32,
}

#[no_mangle]
pub extern "C" fn server_main(_info: *const BootInfo) -> ! {
    sys::log(0, "ui-demo: up");

    let mut win = match Window::create("paint", 360, 260) {
        Some(w) => w,
        None => {
            sys::log(1, "ui-demo: no window manager — idling");
            loop {
                let _ = sys::receive(tanix_libsys::abi::M_ANY);
            }
        }
    };
    {
        let mut s = StrBuf::new();
        s.push_str("ui-demo: window #");
        s.push_dec32(win.winid);
        s.push_str(" open (");
        s.push_dec32(win.w);
        s.push_str("x");
        s.push_dec32(win.h);
        s.push_str(")");
        sys::log(0, s.as_str());
    }

    // Button area (top-right corner of the window).
    let (btn_x, btn_y, btn_w, btn_h) = (win.w as i32 - 110, 8, 100, 32);
    let btn_fill = (0x40, 0x70, 0xc0);

    let mut bg: (u8, u8, u8) = (0x20, 0x22, 0x2a);
    let mut dots = [Point::default(); DOTS];
    let mut dot_next = 0usize;
    let mut dot_count = 0usize;

    // Redraw whenever the pointer moved or the button state changed.
    let mut last = (i32::MAX, i32::MAX, u32::MAX);

    loop {
        let Some(ev) = win.tick() else {
            continue; // cursor not over this window — nothing to do
        };
        let (px, py, pb) = (ev.x as i32, ev.y as i32, ev.buttons);
        let old_pos = (last.0, last.1);
        let pressed_edge = pb & 1 == 1 && last.2 & 1 == 0;
        let changed = (px, py) != old_pos || pressed_edge;
        last = (px, py, pb);

        if pb & 1 == 1 {
            let moved_while_pressed = (px, py) != old_pos;
            if pressed_edge
                && px >= btn_x && px < btn_x + btn_w
                && py >= btn_y && py < btn_y + btn_h
            {
                // Button toggle: flip the background colour.
                bg = if bg == (0x20, 0x22, 0x2a) {
                    (0x12, 0x10, 0x1c)
                } else {
                    (0x20, 0x22, 0x2a)
                };
            } else if pressed_edge || moved_while_pressed {
                // Paint a dot at the pointer (press, or drag = finger
                // painting).
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

        // Full redraw into the canvas, then present.
        win.clear(bg);
        win.fill_rect(btn_x, btn_y, btn_w as u32, btn_h as u32, btn_fill);
        for d in dots[..dot_count].iter() {
            win.fill_rect(d.x - 3, d.y - 3, 7, 7, (0xe8, 0xa0, 0x30));
        }
        win.flush();
    }
}
