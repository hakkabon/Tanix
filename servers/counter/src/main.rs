//! Phase-8 demo app "counter": a +/− counter window.
//!
//! Two big square buttons ("+" and "−") increment and decrement a
//! two-digit number rendered with the shared 5×7 font at scale 6.  The
//! window manager routes clicks into window-local coordinates, so the app
//! never deals with screen positions.

#![no_std]
#![no_main]

use tanix_libsys::{abi::BootInfo, fmt::StrBuf, sys};
use tanix_libtanix_ui::Window;

const W: u32 = 240;
const H: u32 = 220;
const BTN_SIZE: u32 = 84;
const BTN_Y: i32 = 112;
const BTN_PLUS_X: i32 = 20;
const BTN_MINUS_X: i32 = 136;
const BTN_C: (u8, u8, u8) = (0x3a, 0x6a, 0xc8);
const BTN_C_DOWN: (u8, u8, u8) = (0x2a, 0x4e, 0x96);
const NUM_C: (u8, u8, u8) = (0xe8, 0xec, 0xf4);

fn button_hit(px: i32, py: i32, bx: i32) -> bool {
    px >= bx && px < bx + BTN_SIZE as i32 && py >= BTN_Y && py < BTN_Y + BTN_SIZE as i32
}

#[no_mangle]
pub extern "C" fn server_main(_info: *const BootInfo) -> ! {
    sys::log(0, "counter: up");

    let mut win = match Window::create("counter", W, H) {
        Some(w) => w,
        None => {
            sys::log(1, "counter: no window manager — idling");
            loop {
                let _ = sys::receive(tanix_libsys::abi::M_ANY);
            }
        }
    };
    sys::log(0, "counter: window open");

    let mut value: u32 = 42;
    let mut last = (i32::MAX, i32::MAX, u32::MAX);
    let mut btn_down = 0u32; // which button is visually pressed

    loop {
        let Some(ev) = win.tick() else {
            continue;
        };
        let (px, py, pb) = (ev.x as i32, ev.y as i32, ev.buttons);
        let pressed_edge = pb & 1 == 1 && last.2 & 1 == 0;
        let released = pb & 1 == 0 && last.2 & 1 == 1;
        last = (px, py, pb);

        if pressed_edge {
            if button_hit(px, py, BTN_PLUS_X) {
                btn_down = 1;
                value = (value + 1) % 100;
            } else if button_hit(px, py, BTN_MINUS_X) {
                btn_down = 2;
                value = (value + 99) % 100;
            }
        }
        if released {
            btn_down = 0;
        }
        if !pressed_edge && !released && pb & 1 == 0 {
            continue;
        }

        // Redraw.
        win.clear((0x14, 0x16, 0x1e));

        // The value, two big digits.
        let mut num = StrBuf::new();
        num.push_dec32(value);
        win.draw_text(56, 40, 6, NUM_C, num.as_str());

        // Buttons with their glyphs.
        for (bx, glyph) in [(BTN_PLUS_X, '+'), (BTN_MINUS_X, '-')] {
            let c = if (btn_down == 1 && glyph == '+') || (btn_down == 2 && glyph == '-') {
                BTN_C_DOWN
            } else {
                BTN_C
            };
            win.fill_rect(bx, BTN_Y, BTN_SIZE, BTN_SIZE, c);
            // 5×7 glyph at scale 6 → 30×42 px, centered in the 84 px button.
            let mut s = StrBuf::new();
            s.push_str(if glyph == '+' { "+" } else { "-" });
            win.draw_text(bx + (BTN_SIZE as i32 - 30) / 2, BTN_Y + (BTN_SIZE as i32 - 42) / 2, 6, (0xf0, 0xf2, 0xf8), s.as_str());
        }

        win.flush();
    }
}
