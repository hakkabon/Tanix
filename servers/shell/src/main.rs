//! Phase-9 terminal server (`shell`): a windowed shell with keyboard input.
//!
//! The shell is a pure client: it polls the display server for key events
//! (M_DISPLAY_GET_KEYS), renders a character grid into its window canvas,
//! and executes commands that talk to `ramfs` (ls/cat) and the kernel's
//! embedded app registry (`exec`).  It never parks on `receive(M_ANY)` —
//! every send is a synchronous RPC with exactly one reply.
//!
//! The window is 780×420 with a 76×28 character grid (10×14 px cells, the
//! shared 5×7 font at scale 2).  Input is buffered line by line; Enter
//! dispatches; backspace edits; the grid scrolls at the bottom.

#![no_std]
#![no_main]

use tanix_libsys::abi::{
    BootInfo, Message, M_ANY, M_DISPLAY_GET_KEYS, M_DISPLAY_KEYS_REPLY, M_RAMFS_READ,
    M_RAMFS_READDIR, M_RAMFS_READDIR_REPLY, M_RAMFS_READ_REPLY,
};
use tanix_libsys::sys;
use tanix_libtanix_ui::Window;

const W: u32 = 780;
const H: u32 = 420;
const COLS: usize = 76;
const ROWS: usize = 28;
const CELL_W: u32 = 10;
const CELL_H: u32 = 14;
const MX: i32 = 10;
const MY: i32 = 10;

const BG: (u8, u8, u8) = (0x10, 0x12, 0x18);
const FG: (u8, u8, u8) = (0x9f, 0xe8, 0xa0);
const DIM: (u8, u8, u8) = (0x4a, 0x5a, 0x4e);

// Linux input keycodes (linux/input-event-codes.h) → (unshifted, shifted).
// 0 = not mapped.
static KEYMAP: [(u8, u8); 128] = {
    let mut m = [(0u8, 0u8); 128];
    m[2] = (b'1', b'!'); m[3] = (b'2', b'@'); m[4] = (b'3', b'#');
    m[5] = (b'4', b'$'); m[6] = (b'5', b'%'); m[7] = (b'6', b'^');
    m[8] = (b'7', b'&'); m[9] = (b'8', b'*'); m[10] = (b'9', b'(');
    m[11] = (b'0', b')'); m[12] = (b'-', b'_'); m[13] = (b'=', b'+');
    m[16] = (b'q', b'Q'); m[17] = (b'w', b'W'); m[18] = (b'e', b'E');
    m[19] = (b'r', b'R'); m[20] = (b't', b'T'); m[21] = (b'y', b'Y');
    m[22] = (b'u', b'U'); m[23] = (b'i', b'I'); m[24] = (b'o', b'O');
    m[25] = (b'p', b'P'); m[26] = (b'[', b'{'); m[27] = (b']', b'}');
    m[30] = (b'a', b'A'); m[31] = (b's', b'S'); m[32] = (b'd', b'D');
    m[33] = (b'f', b'F'); m[34] = (b'g', b'G'); m[35] = (b'h', b'H');
    m[36] = (b'j', b'J'); m[37] = (b'k', b'K'); m[38] = (b'l', b'L');
    m[39] = (b';', b':'); m[40] = (b'\'', b'"'); m[41] = (b'`', b'~');
    m[43] = (b'\\', b'|'); m[44] = (b'z', b'Z'); m[45] = (b'x', b'X');
    m[46] = (b'c', b'C'); m[47] = (b'v', b'V'); m[48] = (b'b', b'B');
    m[49] = (b'n', b'N'); m[50] = (b'm', b'M'); m[51] = (b',', b'<');
    m[52] = (b'.', b'>'); m[53] = (b'/', b'?'); m[57] = (b' ', b' ');
    m
};

const KEY_ENTER: u16 = 28;
const KEY_BACKSPACE: u16 = 14;
const KEY_LSHIFT: u16 = 42;
const KEY_RSHIFT: u16 = 54;

static mut DISPLAY: i32 = -1;
static mut RAMFS: i32 = -1;

/// RPC helper: send + wait for the reply.  The receiver always replies.
fn rpc(dst: u32, mtype: u32, data: &[u32]) -> Message {
    let mut m = Message::new(mtype);
    for (i, v) in data.iter().take(8).enumerate() {
        m.data[i] = *v;
    }
    loop {
        let rc = sys::send(dst, &m);
        if rc == 0 {
            break;
        }
        sys::yield_cpu();
    }
    let (_src, rep) = sys::receive(M_ANY);
    rep
}

fn get_keys() -> Message {
    let d = unsafe { DISPLAY } as u32;
    rpc(d, M_DISPLAY_GET_KEYS, &[])
}

// ── Terminal ──────────────────────────────────────────────────────────────────

static mut GRID: [[u8; COLS]; ROWS] = [[0; COLS]; ROWS];
static mut ROW: usize = 0;
static mut COL: usize = 0;
static mut INPUT: [u8; 64] = [0; 64];
static mut INPUT_LEN: usize = 0;
static mut SHIFT: bool = false;
static mut DIRTY: bool = false;

fn scroll() {
    unsafe {
        for r in 0..ROWS - 1 {
            GRID[r] = GRID[r + 1];
        }
        GRID[ROWS - 1] = [0; COLS];
    }
}

fn newline() {
    unsafe {
        ROW += 1;
        COL = 0;
        if ROW >= ROWS {
            scroll();
            ROW = ROWS - 1;
        }
        DIRTY = true;
    }
}

fn putc(c: u8) {
    unsafe {
        if c == b'\n' {
            newline();
            return;
        }
        if COL >= COLS {
            newline();
        }
        GRID[ROW][COL] = c;
        COL += 1;
        DIRTY = true;
    }
}

fn puts(s: &str) {
    for &c in s.as_bytes() {
        putc(c);
    }
}

fn putln(s: &str) {
    puts(s);
    putc(b'\n');
}

fn clear() {
    unsafe {
        GRID = [[0; COLS]; ROWS];
        ROW = 0;
        COL = 0;
        INPUT_LEN = 0;
        DIRTY = true;
    }
}

/// Redraw the whole grid (all rows are local canvas ops; one flush per
/// change).
fn repaint(win: &mut Window) {
    win.clear(BG);
    // Snapshot the grid (raw copy avoids the shared-reference-to-static
    // lint), then draw each row as a string.
    let mut grid = [[0u8; COLS]; ROWS];
    unsafe {
        core::ptr::copy_nonoverlapping(
            core::ptr::addr_of!(GRID) as *const u8,
            grid.as_mut_ptr() as *mut u8,
            COLS * ROWS,
        );
    }
    for (r, row) in grid.iter().enumerate() {
        let mut end = COLS;
        while end > 0 && row[end - 1] == 0 {
            end -= 1;
        }
        if end == 0 {
            continue;
        }
        // ASCII-only content; draw each row as a string.
        let mut buf = [0u8; COLS];
        buf[..end].copy_from_slice(&row[..end]);
        let s = core::str::from_utf8(&buf[..end]).unwrap_or("");
        win.draw_text(MX, MY + (r as i32) * CELL_H as i32, 2, FG, s);
    }
    // Cursor: a dim underline block at the current cell.
    let (col, row) = unsafe { (COL, ROW) };
    win.fill_rect(
        MX + col as i32 * CELL_W as i32,
        MY + row as i32 * CELL_H as i32 + CELL_H as i32 - 2,
        CELL_W,
        2,
        DIM,
    );
    unsafe { DIRTY = false };
    win.flush();
}

// ── Commands ──────────────────────────────────────────────────────────────────

/// Pack a ≤ 24-char path into data[0..6] (NUL-terminated).
fn pack_path(data: &mut [u32; 8], path: &str) {
    let bytes = path.as_bytes();
    let n = bytes.len().min(23);
    for (i, b) in bytes.iter().take(n).enumerate() {
        data[i / 4] |= (*b as u32) << (8 * (i % 4));
    }
}

/// Strip leading slashes.
fn normalize(p: &str) -> &str {
    let mut p = p;
    while let Some(s) = p.strip_prefix('/') {
        p = s;
    }
    if p.is_empty() {
        ""
    } else {
        p
    }
}

fn cmd_ls(dir: &str) {
    let dir = normalize(dir);
    let ramfs = unsafe { RAMFS } as u32;
    let mut offset = 0u32;
    let mut count = 0u32;
    loop {
        let mut data = [0u32; 8];
        pack_path(&mut data, dir);
        data[6] = offset;
        let rep = rpc(ramfs, M_RAMFS_READDIR, &data);
        if rep.mtype != M_RAMFS_READDIR_REPLY || rep.data[0] == 0 {
            break;
        }
        // Name: 8 bytes in data[1], data[2].
        let mut name = [0u8; 8];
        for (i, b) in name.iter_mut().enumerate() {
            *b = (rep.data[1 + i / 4] >> (8 * (i % 4))) as u8;
        }
        let len = name.iter().position(|&b| b == 0).unwrap_or(8);
        let name = core::str::from_utf8(&name[..len]).unwrap_or("?");
        if rep.data[3] == 1 {
            let mut s = tanix_libsys::fmt::StrBuf::new();
            s.push_str(name);
            let mut pad = 16usize.saturating_sub(name.len());
            while pad > 0 {
                s.push_str(" ");
                pad -= 1;
            }
            s.push_str("<dir>");
            putln(s.as_str());
        } else {
            let mut s = tanix_libsys::fmt::StrBuf::new();
            s.push_str(name);
            let mut pad = 16usize.saturating_sub(name.len());
            while pad > 0 {
                s.push_str(" ");
                pad -= 1;
            }
            s.push_dec32(rep.data[4]);
            s.push_str(" bytes");
            putln(s.as_str());
        }
        count += 1;
        offset += 1;
    }
    if count == 0 {
        putln("(empty)");
    }
}

fn cmd_cat(path: &str) {
    let path = normalize(path);
    if path.is_empty() {
        putln("cat: usage: cat <file>");
        return;
    }
    let ramfs = unsafe { RAMFS } as u32;
    let mut offset = 0u32;
    loop {
        let mut data = [0u32; 8];
        pack_path(&mut data, path);
        data[6] = offset;
        let rep = rpc(ramfs, M_RAMFS_READ, &data);
        if rep.mtype != M_RAMFS_READ_REPLY || rep.data[0] == 0 {
            break;
        }
        let n = rep.data[0] as usize;
        for i in 0..n {
            let b = (rep.data[1 + i / 4] >> (8 * (i % 4))) as u8;
            putc(b);
        }
        offset += n as u32;
        if n < 28 {
            break; // short read = EOF
        }
    }
}

fn cmd_exec(app: &str) {
    if app.is_empty() {
        putln("exec: usage: exec <app>");
        return;
    }
    let pid = sys::exec(app);
    if pid >= 0 {
        let mut s = tanix_libsys::fmt::StrBuf::new();
        s.push_str(app);
        s.push_str(": started (task ");
        s.push_dec32(pid as u32);
        s.push_str(")");
        putln(s.as_str());
    } else {
        let mut s = tanix_libsys::fmt::StrBuf::new();
        s.push_str("exec ");
        s.push_str(app);
        s.push_str(": error ");
        s.push_dec32(-pid as u32);
        putln(s.as_str());
    }
}

fn cmd_help() {
    putln("tanix shell — phase 9");
    putln("  help            this text");
    putln("  ls [dir]        list /, /bin, /etc");
    putln("  cat <file>      print a file (e.g. /etc/motd)");
    putln("  exec <app>      start an embedded app (e.g. counter, clock, ui-demo, hog)");
    putln("  clear           clear the terminal");
}

fn dispatch(line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let (cmd, args) = match line.find(char::is_whitespace) {
        Some(i) => (&line[..i], line[i..].trim()),
        None => (line, ""),
    };
    match cmd {
        "help" => cmd_help(),
        "ls" => cmd_ls(args),
        "cat" => cmd_cat(args),
        "exec" => cmd_exec(args),
        "clear" => {
            clear();
            prompt();
        }
        _ => {
            putln("shell: no such command (try help)");
        }
    }
}

fn prompt() {
    puts("> ");
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn server_main(_info: *const BootInfo) -> ! {
    sys::log(0, "shell: up");

    unsafe {
        DISPLAY = sys::who("display");
        RAMFS = sys::who("ramfs");
    }
    if unsafe { DISPLAY < 0 || RAMFS < 0 } {
        sys::log(1, "shell: display or ramfs not found — idling");
        loop {
            let _ = sys::receive(M_ANY);
        }
    }

    let mut win = match Window::create("shell", W, H) {
        Some(w) => w,
        None => {
            sys::log(1, "shell: no window manager — idling");
            loop {
                let _ = sys::receive(M_ANY);
            }
        }
    };
    sys::log(0, "shell: window open");

    putln("tanix 0.1.0 — phase 9 shell");
    putln("type help for commands");
    prompt();
    repaint(&mut win);

    loop {
        // Pace the keyboard polling (human typing cadence).
        sys::sleep(15);

        let rep = get_keys();
        if rep.mtype != M_DISPLAY_KEYS_REPLY || rep.data[0] == 0 {
            continue;
        }
        let n = rep.data[0].min(7) as usize;
        for i in 0..n {
            let pair = rep.data[1 + i];
            let code = (pair >> 16) as u16;
            let value = (pair & 0xFFFF) as u16;
            match code {
                KEY_LSHIFT | KEY_RSHIFT => {
                    unsafe { SHIFT = value == 1 };
                    continue;
                }
                _ => {}
            }
            if value != 1 {
                continue; // releases (0) and autorepeats (2) do nothing
            }
            match code {
                KEY_ENTER => {
                    let mut line = [0u8; 64];
                    let n = unsafe { INPUT_LEN }.min(63);
                    line[..n].copy_from_slice(&unsafe { INPUT }[..n]);
                    putc(b'\n');
                    let s = core::str::from_utf8(&line[..n]).unwrap_or("");
                    unsafe { INPUT_LEN = 0 };
                    dispatch(s);
                    prompt();
                }
                KEY_BACKSPACE => {
                    unsafe {
                        if INPUT_LEN > 0 {
                            INPUT_LEN -= 1;
                            if COL > 0 {
                                COL -= 1;
                                GRID[ROW][COL] = 0;
                                DIRTY = true;
                            }
                        }
                    }
                }
                _ => {
                    let c = if (code as usize) < KEYMAP.len() {
                        if unsafe { SHIFT } {
                            KEYMAP[code as usize].1
                        } else {
                            KEYMAP[code as usize].0
                        }
                    } else {
                        0
                    };
                    if c != 0 {
                        unsafe {
                            if INPUT_LEN < 63 {
                                INPUT[INPUT_LEN] = c;
                                INPUT_LEN += 1;
                            }
                        }
                        putc(c);
                    }
                }
            }
        }
        if unsafe { DIRTY } {
            repaint(&mut win);
        }
    }
}
