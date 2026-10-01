//! Window manager / compositor (`wm`) — Phase 8.
//!
//! Sits between the apps and the display server:
//!
//! ```text
//! paint / counter / clock ── M_WM_* ──▶ wm ── M_DISPLAY_* ──▶ display
//! ```
//!
//! The wm owns the *window table* — placement, z-order, focus, dragging —
//! and composites the desktop: it paints the background, blits every
//! window's canvas into the framebuffer in Z-order, draws the chrome
//! (border + title bar with the app's title), renders the pointer cursor,
//! and presents through the display server.  Apps never see screen
//! coordinates: the wm routes tablet events in window-local pixels to the
//! topmost window under the cursor.
//!
//! All pixel work is done by the display server (the wm never touches the
//! framebuffer — it only sends `M_DISPLAY_*` requests).  The wm is fully
//! receive-driven, exactly like the display server.

#![no_std]
#![no_main]

use tanix_libsys::abi::{
    BootInfo, Message, M_ANY, M_DISPLAY_BLIT, M_DISPLAY_DRAW_TEXT, M_DISPLAY_FILL_RECT,
    M_DISPLAY_FLUSH, M_DISPLAY_GET_MODE, M_DISPLAY_MODE_REPLY, M_DISPLAY_TICK, M_WM_CLOSE,
    M_WM_CREATE, M_WM_CREATE_REPLY, M_WM_DONE, M_WM_FLUSH, M_WM_TICK, M_WM_TICK_REPLY,
};
use tanix_libsys::{sys, Message as M};

// ── Chrome geometry ───────────────────────────────────────────────────────────

/// Title-bar height in pixels.
const TITLE_H: u32 = 16;
/// Border thickness in pixels.
const BORDER: u32 = 1;
/// Maximum number of windows.
const MAX_WINDOWS: usize = 8;
/// Max canvas size: 1024×640×4 = 2.5 MiB ≈ 640 pages (Phase 9: the shell
/// terminal is 780×420 ≈ 320 pages).
const MAX_PAGES: u32 = 640;
/// Window content size limits.
const MIN_DIM: u32 = 32;
const MAX_W: u32 = 1024;
const MAX_H: u32 = 640;

// ── Palette ───────────────────────────────────────────────────────────────────

const BG: (u8, u8, u8) = (0x1e, 0x20, 0x28);
const BORDER_C: (u8, u8, u8) = (0x0c, 0x0e, 0x12);
const TITLE_ACTIVE: (u8, u8, u8) = (0x3a, 0x6a, 0xc8);
const TITLE_INACTIVE: (u8, u8, u8) = (0x2c, 0x30, 0x3a);
const TITLE_TEXT: (u8, u8, u8) = (0xd8, 0xdc, 0xe8);
const CURSOR_A: (u8, u8, u8) = (0xff, 0xff, 0xff);
const CURSOR_B: (u8, u8, u8) = (0x00, 0x00, 0x00);

// ── WM state (static; single-threaded cooperative execution) ─────────────────

/// One window.  Slots are kept in Z-order: later slots are on top.
#[derive(Clone, Copy)]
struct Window {
    id: u32,
    app: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    canvas: u64,
    title: [u8; 12],
    title_len: u32,
    /// True when this window's canvas has changed since the last composite.
    /// Set by `serve_flush`; cleared after the window is blitted.
    dirty: bool,
}

impl Window {
    const fn empty() -> Self {
        Self { id: 0, app: 0, x: 0, y: 0, w: 0, h: 0, canvas: 0,
               title: [0; 12], title_len: 0, dirty: false }
    }
}

static mut WINDOWS: [Window; MAX_WINDOWS] = [Window::empty(); MAX_WINDOWS];
static mut DISPLAY: u32 = 0;
static mut SCREEN: (u32, u32) = (0, 0); // width, height
static mut PTR: (u32, u32, u32) = (0, 0, 0); // x, y, buttons (last sampled)
static mut DRAG: Option<(usize, u32, u32)> = None; // (slot, grab dx, grab dy)
static mut NEXT_ID: u32 = 1;
static mut CREATED: u32 = 0;

/// Mutable view over the window table (single-threaded — same idiom as the
/// kernel's static scheduler).
fn windows() -> &'static mut [Window; MAX_WINDOWS] {
    unsafe { &mut *core::ptr::addr_of_mut!(WINDOWS) }
}

/// Number of live windows.
fn live_count() -> usize {
    windows().iter().filter(|w| w.id != 0).count()
}

/// Slot index of the window with the given id.
fn find_slot(id: u32) -> Option<usize> {
    windows().iter().position(|w| w.id == id)
}

/// Bounding box of a window including its chrome.
fn bbox(w: &Window) -> (i32, i32, i32, i32) {
    (
        w.x as i32 - BORDER as i32,
        w.y as i32 - TITLE_H as i32 - BORDER as i32,
        (w.w + 2 * BORDER) as i32,
        (w.h + TITLE_H + 2 * BORDER) as i32,
    )
}

/// Topmost window under the screen point (None = desktop).
fn hit_window(px: u32, py: u32) -> Option<usize> {
    windows()
        .iter()
        .enumerate()
        .rev()
        .find(|(_, w)| {
            if w.id == 0 {
                return false;
            }
            let (bx, by, bw, bh) = bbox(w);
            px >= bx as u32 && px < (bx + bw) as u32 && py >= by as u32 && py < (by + bh) as u32
        })
        .map(|(i, _)| i)
}

/// Is the point on the window's title bar?
fn on_title(w: &Window, px: u32, py: u32) -> bool {
    px >= w.x && px < w.x + w.w && py >= w.y.saturating_sub(TITLE_H) && py < w.y
}

/// Move a window's content area, clamping it onto the desktop.
fn clamp_move(slot: usize, nx: u32, ny: u32) {
    let (sw, sh) = unsafe { SCREEN };
    let w = &mut windows()[slot];
    w.x = nx.min(sw.saturating_sub(w.w + 2 * BORDER));
    w.y = ny.min(sh.saturating_sub(w.h + TITLE_H + 2 * BORDER));
}

/// Mark all live windows dirty — needed after structural changes (window
/// created, closed, raised) where every window must be recomposited.
fn mark_all_dirty() {
    for w in windows().iter_mut() {
        if w.id != 0 {
            w.dirty = true;
        }
    }
}

/// Raise a window to the top of the Z-order.
fn raise(slot: usize) {
    let n = live_count();
    if n <= 1 || slot == n - 1 {
        return;
    }
    let ws = windows();
    let w = ws[slot];
    // Shift everyone above down one slot.
    for i in slot..n - 1 {
        ws[i] = ws[i + 1];
    }
    ws[n - 1] = w;
}

// ── Display-server RPC ────────────────────────────────────────────────────────

/// Request/response helper against the display server.
fn dcall(mtype: u32, data: &[u32]) -> Message {
    let mut m = M::new(mtype);
    for (i, v) in data.iter().take(8).enumerate() {
        m.data[i] = *v;
    }
    sys::send(unsafe { DISPLAY }, &m);
    // Receive ONLY from the display server: the reply must not be
    // starved by queued requests from other servers (M_WM_CREATE etc.).
    let (_src, rep) = sys::receive(unsafe { DISPLAY } as i32);
    rep
}

fn dfill(x: u32, y: u32, w: u32, h: u32, rgb: (u8, u8, u8)) {
    let _ = dcall(
        M_DISPLAY_FILL_RECT,
        &[x, y, w, h, rgb.0 as u32, rgb.1 as u32, rgb.2 as u32],
    );
}

fn dblit(canvas: u64, sx: u32, sy: u32, w: u32, h: u32, dx: u32, dy: u32) {
    let _ = dcall(
        M_DISPLAY_BLIT,
        &[canvas as u32, (canvas >> 32) as u32, sx, sy, w, h, dx, dy],
    );
}

fn dtext(x: u32, y: u32, rgb: (u8, u8, u8), s: &str) {
    let mut m = M::new(M_DISPLAY_DRAW_TEXT);
    m.data[0] = x;
    m.data[1] = y;
    m.data[2] = ((rgb.0 as u32) << 16) | ((rgb.1 as u32) << 8) | rgb.2 as u32;
    m.data[3] = s.len().min(16) as u32;
    for (i, &b) in s.as_bytes().iter().take(16).enumerate() {
        m.data[4 + i / 4] |= (b as u32) << (8 * (i % 4));
    }
    sys::send(unsafe { DISPLAY }, &m);
    let (_src, _rep) = sys::receive(unsafe { DISPLAY } as i32);
}

// ── Compositing ───────────────────────────────────────────────────────────────

/// Redraw the desktop using damage-region tracking:
///   1. Only blits windows whose `dirty` flag is set; clears the flag.
///   2. Tracks the bounding union of all dirty window rectangles (including
///      chrome) to determine which part of the framebuffer actually changed.
///   3. Always redraws the background behind dirty windows and the cursor
///      (cursor movement is cheap — two small fills).
///   4. Calls `M_DISPLAY_FLUSH` once to present; the GPU transfer covers
///      only the dirty bounding rect (passed via `M_DISPLAY_FLUSH`'s data
///      words — the display server's full-screen flush is called when the
///      dirty rect equals the whole screen, but partial flushes use
///      `M_DISPLAY_FLUSH_RECT` when that protocol is available, otherwise
///      fall back to full-screen flush).
///
/// A full-screen repaint is still issued when any structural change occurred
/// (create, close, raise) — callers set all windows dirty via `mark_all_dirty`
/// before calling `composite` in those cases.
fn composite() {
    use tanix_libsys::abi::M_DISPLAY_FLUSH;

    let (sw, sh) = unsafe { SCREEN };

    // Compute the dirty bounding rect across all dirty windows (including
    // chrome) and the cursor region.
    let mut any_dirty = false;
    let mut dx0 = sw;
    let mut dy0 = sh;
    let mut dx1 = 0u32;
    let mut dy1 = 0u32;

    for w in windows().iter() {
        if w.id == 0 || !w.dirty {
            continue;
        }
        any_dirty = true;
        let (bx, by, bw, bh) = bbox(w);
        let x0 = (bx.max(0)) as u32;
        let y0 = (by.max(0)) as u32;
        let x1 = ((bx + bw).max(0) as u32).min(sw);
        let y1 = ((by + bh).max(0) as u32).min(sh);
        if x0 < dx0 { dx0 = x0; }
        if y0 < dy0 { dy0 = y0; }
        if x1 > dx1 { dx1 = x1; }
        if y1 > dy1 { dy1 = y1; }
    }

    // Always include the cursor region in the dirty rect (it moves on every
    // M_WM_TICK so its previous and new positions must both be repainted).
    let (px, py, _) = unsafe { PTR };
    if px != u32::MAX {
        let cx0 = px.saturating_sub(5);
        let cy0 = py.saturating_sub(5);
        let cx1 = (px + 5).min(sw);
        let cy1 = (py + 5).min(sh);
        any_dirty = true;
        if cx0 < dx0 { dx0 = cx0; }
        if cy0 < dy0 { dy0 = cy0; }
        if cx1 > dx1 { dx1 = cx1; }
        if cy1 > dy1 { dy1 = cy1; }
    }

    // Nothing changed — skip the entire composite+flush round.
    if !any_dirty || dx0 >= dx1 || dy0 >= dy1 {
        return;
    }

    // Fill the background only within the dirty rect to minimise GPU work.
    dfill(dx0, dy0, dx1 - dx0, dy1 - dy0, BG);

    let n = live_count();
    for (slot, w) in windows().iter_mut().enumerate() {
        if w.id == 0 {
            continue;
        }
        // Check if this window overlaps the dirty rect — if so, it must be
        // redrawn even if it wasn't the one that changed (windows behind a
        // dirty one need to be recomposited because we filled BG over them).
        let (bx, by, bw, bh) = bbox(w);
        let wx0 = (bx.max(0)) as u32;
        let wy0 = (by.max(0)) as u32;
        let wx1 = ((bx + bw).max(0) as u32).min(sw);
        let wy1 = ((by + bh).max(0) as u32).min(sh);
        let overlaps_dirty = wx0 < dx1 && wx1 > dx0 && wy0 < dy1 && wy1 > dy0;

        if !overlaps_dirty {
            // This window doesn't intersect the dirty region at all — skip.
            continue;
        }

        // Border.
        dfill(
            w.x.saturating_sub(BORDER),
            w.y.saturating_sub(TITLE_H + BORDER),
            w.w + 2 * BORDER,
            w.h + TITLE_H + 2 * BORDER,
            BORDER_C,
        );
        // Canvas blit.
        dblit(w.canvas, 0, 0, w.w, w.h, w.x, w.y);
        // Title bar.
        let active = slot == n - 1;
        let title_c = if active { TITLE_ACTIVE } else { TITLE_INACTIVE };
        dfill(w.x, w.y.saturating_sub(TITLE_H), w.w, TITLE_H, title_c);
        // Title text.
        let mut s = [0u8; 16];
        let len = (w.title_len as usize).min(16);
        for (i, &b) in w.title[..len].iter().enumerate() {
            s[i] = if b.is_ascii_lowercase() { b - 32 } else { b };
        }
        let text = core::str::from_utf8(&s[..len]).unwrap_or("");
        dtext(w.x + 4, w.y.saturating_sub(TITLE_H) + (TITLE_H - 8) / 2, TITLE_TEXT, text);

        // Mark clean — this window's canvas has now been composited.
        w.dirty = false;
    }

    // Cursor: white ring on black hole (always drawn last, always in front).
    if px != u32::MAX {
        dfill(px.saturating_sub(4), py.saturating_sub(4), 9, 9, CURSOR_A);
        dfill(px.saturating_sub(3), py.saturating_sub(3), 7, 7, CURSOR_B);
    }

    // Flush: send the dirty rect coordinates so the display server can issue
    // a partial TRANSFER+FLUSH to the GPU when the protocol supports it.
    // For now the display server still does a full-screen flush, but the
    // dirty rect is passed in data[0..3] for future use.
    let mut m = tanix_libsys::Message::new(M_DISPLAY_FLUSH);
    m.data[0] = dx0;
    m.data[1] = dy0;
    m.data[2] = dx1 - dx0; // dirty rect width
    m.data[3] = dy1 - dy0; // dirty rect height
    sys::send(unsafe { DISPLAY }, &m);
    let (_src, _rep) = sys::receive(unsafe { DISPLAY } as i32);
}

// ── Service loop ──────────────────────────────────────────────────────────────

fn reply(dst: u32, mtype: u32, data: &[u32]) {
    let mut m = M::new(mtype);
    for (i, v) in data.iter().take(8).enumerate() {
        m.data[i] = *v;
    }
    sys::send(dst, &m);
}

fn serve_create(src: u32, msg: &Message) {
    let (w, h) = (msg.data[0], msg.data[1]);
    let canvas = ((msg.data[3] as u64) << 32) | msg.data[2] as u64;
    let pages = msg.data[4];
    let bytes = (w as u64) * (h as u64) * 4;

    let valid = (MIN_DIM..=MAX_W).contains(&w)
        && (MIN_DIM..=MAX_H).contains(&h)
        && pages <= MAX_PAGES
        && bytes <= (pages as u64) * 4096
        && canvas != 0;
    let slot = if valid { windows().iter().position(|wd| wd.id == 0) } else { None };

    match slot {
        None => reply(src, M_WM_CREATE_REPLY, &[0, 0, 0, 0, 0, 0]),
        Some(slot) => {
            let (sw, _sh) = unsafe { SCREEN };
            let col = unsafe { CREATED } % 3;
            let row = unsafe { CREATED } / 3;
            let x = (40 + col * (w + 24)).min(sw.saturating_sub(w + 2 * BORDER));
            let y = 56 + row * (h + 48);

            let mut wd = Window {
                id: unsafe { NEXT_ID },
                app: src,
                x,
                y,
                w,
                h,
                canvas,
                title: [0; 12],
                title_len: 0,
                dirty: true, // new window needs its first composite
            };
            for i in 0..12 {
                wd.title[i] = (msg.data[5 + i / 4] >> (8 * (i % 4))) as u8;
                if wd.title[i] == 0 {
                    wd.title_len = i as u32;
                    break;
                }
                wd.title_len = i as u32 + 1;
            }
            windows()[slot] = wd;
            unsafe {
                NEXT_ID += 1;
                CREATED += 1;
            }
            let id = windows()[slot].id;
            reply(src, M_WM_CREATE_REPLY, &[id, x, y, w, h, 1]);
            mark_all_dirty(); // new window requires full desktop repaint
            composite();
        }
    }
}

fn serve_flush(src: u32, msg: &Message) {
    let ok = match find_slot(msg.data[0]) {
        Some(slot) => windows()[slot].app == src,
        None => false,
    };
    if ok {
        // Mark only the flushing window dirty — other windows are unchanged.
        let slot = find_slot(msg.data[0]).unwrap();
        windows()[slot].dirty = true;
        composite();
    }
    reply(src, M_WM_DONE, &[ok as u32]);
}

fn serve_tick(src: u32, msg: &Message) {
    let winid = msg.data[0];
    let Some(slot) = find_slot(winid) else {
        return; // no such window — never reply to TICK
    };
    if windows()[slot].app != src {
        return;
    }

    // Sample the tablet through the display server.
    let rep = dcall(M_DISPLAY_TICK, &[]);
    let (px, py, pb) = (rep.data[0], rep.data[1], rep.data[2]);
    let prev_pb = unsafe { PTR.2 };
    unsafe { PTR = (px, py, pb) }

    let mut recomposite = false;

    // Drag / raise state machine.
    if let Some((dslot, off_x, off_y)) = unsafe { DRAG } {
        if pb & 1 == 1 {
            // Continue dragging: mark the dragged window (and all windows
            // it may have uncovered/covered) dirty so the compositor
            // repaints the affected region.
            clamp_move(dslot, px.saturating_sub(off_x), py.saturating_sub(off_y));
            mark_all_dirty(); // position changed — neighboring windows affected
            recomposite = true;
        } else {
            unsafe { DRAG = None }
        }
    } else if pb & 1 == 1 && prev_pb & 1 == 0 {
        // Press edge: interact with the topmost window under the cursor.
        if let Some(hit) = hit_window(px, py) {
            let on_tb = on_title(&windows()[hit], px, py);
            let n = live_count();
            if on_tb && hit != n - 1 {
                // Grab the title bar: raise + begin dragging.
                raise(hit);
                mark_all_dirty(); // z-order changed
                let off = (windows()[hit].x, windows()[hit].y);
                unsafe { DRAG = Some((hit, px.saturating_sub(off.0), py.saturating_sub(off.1))) }
                recomposite = true;
            } else if on_tb {
                let off = (windows()[hit].x, windows()[hit].y);
                unsafe { DRAG = Some((hit, px.saturating_sub(off.0), py.saturating_sub(off.1))) }
            } else if hit != n - 1 {
                // Click anywhere raises the window to the top.
                raise(hit);
                mark_all_dirty(); // z-order changed
                recomposite = true;
            }
        }
    }

    // Route the event: the requester's window gets the pointer only when it
    // is the topmost window under the cursor.
    let routed = hit_window(px, py) == Some(slot);
    if routed {
        let w = &windows()[slot];
        reply(
            src,
            M_WM_TICK_REPLY,
            &[
                px.saturating_sub(w.x).min(w.w),
                py.saturating_sub(w.y).min(w.h),
                pb,
                if slot == live_count() - 1 { 1 } else { 0 },
            ],
        );
    } else {
        reply(src, M_WM_TICK_REPLY, &[u32::MAX, u32::MAX, 0, 0]);
    }

    if recomposite {
        composite();
    }
}

fn serve_close(src: u32, msg: &Message) {
    let mut ok = false;
    if let Some(slot) = find_slot(msg.data[0]) {
        if windows()[slot].app == src {
            let n = live_count();
            windows()[slot] = Window::empty();
            // Compact the Z-order.
            for i in slot..n.saturating_sub(1) {
                windows()[i] = windows()[i + 1];
            }
            windows()[n.saturating_sub(1)] = Window::empty();
            if let Some((ds, _, _)) = unsafe { DRAG } {
                // Clear drag only when the window being dragged is the one
                // being closed — not any window with a higher slot index.
                if ds == slot {
                    unsafe { DRAG = None }
                }
            }
            ok = true;
        }
    }
    if ok {
        mark_all_dirty(); // closed window requires full desktop repaint
        composite();
    }
    reply(src, M_WM_DONE, &[ok as u32]);
}

#[no_mangle]
pub extern "C" fn server_main(_info: *const BootInfo) -> ! {
    sys::log(0, "wm: up");

    let display = sys::who("display");
    if display < 0 {
        sys::log(1, "wm: display server not found — idling");
        loop {
            let _ = sys::receive(M_ANY);
        }
    }
    unsafe { DISPLAY = display as u32 }

    let mode = dcall(M_DISPLAY_GET_MODE, &[]);
    if mode.mtype != M_DISPLAY_MODE_REPLY {
        sys::log(1, "wm: no display mode — idling");
        loop {
            let _ = sys::receive(M_ANY);
        }
    }
    unsafe {
        SCREEN = (mode.data[0], mode.data[1]);
        PTR = (u32::MAX, u32::MAX, 0);
    }
    {
        let (sw, sh) = unsafe { SCREEN };
        let mut s = tanix_libsys::fmt::StrBuf::new();
        s.push_str("wm: desktop ");
        s.push_dec32(sw);
        s.push_str("x");
        s.push_dec32(sh);
        s.push_str(" online, awaiting apps");
        sys::log(0, s.as_str());
    }

    composite(); // initial empty desktop — paint the background
    // Force a full background fill on startup even though there are no dirty
    // windows yet (the damage system would skip it otherwise).
    {
        let (sw, sh) = unsafe { SCREEN };
        dfill(0, 0, sw, sh, BG);
        let _ = dcall(M_DISPLAY_FLUSH, &[]);
    }

    loop {
        let (src, msg) = sys::receive(M_ANY);
        match msg.mtype {
            M_WM_CREATE => serve_create(src, &msg),
            M_WM_FLUSH => serve_flush(src, &msg),
            M_WM_TICK => serve_tick(src, &msg),
            M_WM_CLOSE => serve_close(src, &msg),
            _ => {
                // Unknown request — drop it without replying.
            }
        }
    }
}
