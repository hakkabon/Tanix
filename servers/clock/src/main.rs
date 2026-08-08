//! Phase-8 demo app "clock": an animated analog clock window.
//!
//! The clock is the SYS_SLEEP demo: it blocks for 100 ms per frame (the
//! preemptive scheduler wakes it at the tick deadline while the other apps
//! and `hog` keep running), advances a second hand by 6°, and redraws its
//! canvas.  No pointer interaction — but the window is still draggable and
//! raisable like any other.

#![no_std]
#![no_main]

use tanix_libsys::{abi::BootInfo, sys};
use tanix_libtanix_ui::Window;

const W: u32 = 260;
const H: u32 = 260;
const CX: i32 = 130;
const CY: i32 = 130;
const R_SEC: i32 = 92;
const R_TICK: i32 = 82;
const R_HOUR: i32 = 50;

/// r·cos(a) for a in 0..=90°, 6° steps (r=96; exact values rounded).
const COS: [i32; 16] = [
    96, 95, 94, 91, 88, 83, 78, 71, 64, 56, 48, 39, 30, 20, 10, 0,
];

/// `r * cos(deg)` via quadrant mirroring (deg in degrees).
fn cos_r(deg: u32, r: i32) -> i32 {
    let d = deg % 360;
    let a = (d % 90).min(90);
    let v = if a == 90 {
        0
    } else {
        COS[(a / 6) as usize] * r / 96
    };
    match d / 90 {
        0 => v,
        1 => -v,
        2 => -v,
        _ => v,
    }
}

/// `r * sin(deg)` = `r * cos(deg - 90)`.
fn sin_r(deg: u32, r: i32) -> i32 {
    cos_r(deg.wrapping_sub(90), r)
}

/// Endpoint of a hand at `angle` degrees from 12 o'clock, length `r`.
fn hand(angle: u32, r: i32) -> (i32, i32) {
    (CX + cos_r(angle, r), CY - sin_r(angle, r))
}

#[no_mangle]
pub extern "C" fn server_main(_info: *const BootInfo) -> ! {
    sys::log(0, "clock: up");

    let mut win = match Window::create("clock", W, H) {
        Some(w) => w,
        None => {
            sys::log(1, "clock: no window manager — idling");
            loop {
                let _ = sys::receive(tanix_libsys::abi::M_ANY);
            }
        }
    };
    sys::log(0, "clock: window open");

    let mut frames: u32 = 0;
    let mut last_sec: i32 = -1;

    loop {
        sys::sleep(100);
        frames = frames.wrapping_add(1);
        let sec = (frames % 60) as i32;
        if sec == last_sec {
            continue;
        }
        last_sec = sec;

        // Redraw the face: tick marks + hour and second hands.
        win.clear((0x0f, 0x12, 0x1a));

        for k in 0..12 {
            let a = k * 30;
            let (ox, oy) = hand(a, R_TICK);
            let (ix, iy) = hand(a, R_TICK - 10);
            win.draw_line(ox, oy, ix, iy, (0x4a, 0x4e, 0x5a));
        }

        // Hour hand: one turn per 720 frames (2 minutes of wall time).
        let hour_a = (frames / 60) % 12 * 30;
        let (hx, hy) = hand(hour_a, R_HOUR);
        win.draw_line(CX, CY, hx, hy, (0xe8, 0xa0, 0x30));

        // Second hand.
        let sec_a = (frames % 60) * 6;
        let (sx, sy) = hand(sec_a, R_SEC);
        win.draw_line(CX, CY, sx, sy, (0xd8, 0xdc, 0xe8));

        // Centre cap.
        win.fill_rect(CX - 3, CY - 3, 7, 7, (0xa0, 0xa4, 0xb0));

        win.flush();
    }
}
