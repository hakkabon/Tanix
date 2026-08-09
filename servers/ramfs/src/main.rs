//! RAM filesystem server (`ramfs`) — Phase 9.
//!
//! A minimal read-only filesystem served over IPC.  The whole tree lives in
//! the server's image (embedded `.rodata`), so reads are just memcpys from
//! static tables.  Two directories:
//!
//!   /bin   — the app registry: everything the kernel's embedded-image
//!            registry can `exec` (sizes = image bytes, from kernel build)
//!   /etc   — text files (motd, version)
//!
//! Protocol (namespace 0x0700):
//!   M_RAMFS_READDIR(dir, offset) → M_RAMFS_READDIR_REPLY
//!       data[0..6] = dir path (≤ 24 bytes + NUL), data[6] = entry offset
//!       reply: data[0] = 1 (entry) | 0 (end); data[1,2] = name (8 bytes);
//!              data[3] = is_dir; data[4] = size
//!   M_RAMFS_READ(path, offset) → M_RAMFS_READ_REPLY
//!       data[0..6] = path (≤ 24 bytes + NUL), data[6] = byte offset
//!       reply: data[0] = bytes read (0 = EOF / not found),
//!              data[1..8] = up to 28 payload bytes (little-endian words)

#![no_std]
#![no_main]

use tanix_libsys::abi::{
    BootInfo, Message, M_ANY, M_RAMFS_READ, M_RAMFS_READDIR, M_RAMFS_READDIR_REPLY,
    M_RAMFS_READ_REPLY,
};
use tanix_libsys::sys;

const DIR: u8 = 1;
const FILE: u8 = 2;

/// Embedded image sizes (debug build; from `stat` on the server ELFs —
/// rounded to the byte).
const SZ_COUNTER: u32 = 964_000;
const SZ_CLOCK: u32 = 960_000;
const SZ_UI_DEMO: u32 = 1_000_000;
const SZ_HOG: u32 = 940_000;

/// One filesystem entry.  `path` is the full path without a leading slash
/// ("bin", "etc/motd", ...); `kind` is DIR or FILE; `data` holds file bytes
/// (empty for directories and app images, which live in the kernel).
struct Entry {
    path: &'static str,
    kind: u8,
    data: &'static [u8],
}

const MOTD: &[u8] = b"Welcome to Tanix 0.1.0 (phase 9).\nA tiny microkernel: servers talk over IPC,\nthe window manager composites this desktop, and\nyour filesystem is RAM. Everything here is real.\n\nType `help` for available commands.\n";
const VERSION: &[u8] = b"tanix 0.1.0 - phase 9 (ramfs, shell, exec)\n";

static FILES: [Entry; 8] = [
    Entry { path: "bin", kind: DIR, data: &[] },
    Entry { path: "etc", kind: DIR, data: &[] },
    Entry { path: "bin/counter", kind: FILE, data: &[], },
    Entry { path: "bin/clock", kind: FILE, data: &[] },
    Entry { path: "bin/ui-demo", kind: FILE, data: &[] },
    Entry { path: "bin/hog", kind: FILE, data: &[] },
    Entry { path: "etc/motd", kind: FILE, data: MOTD },
    Entry { path: "etc/version", kind: FILE, data: VERSION },
];

/// Parse a NUL-terminated ≤ 24-byte path out of `data`, strip any leading
/// slash, and copy it into `out` (NUL-terminated).  Returns the byte
/// length, or `None` when malformed (not ASCII text).
fn read_path(data: &[u32], out: &mut [u8; 24]) -> Option<usize> {
    let mut bytes = [0u8; 24];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = (data[i / 4] >> (8 * (i % 4))) as u8;
    }
    let n = bytes.iter().position(|&b| b == 0).unwrap_or(24);
    if !bytes[..n].is_ascii() {
        return None;
    }
    let mut start = 0;
    while start < n && bytes[start] == b'/' {
        start += 1;
    }
    let len = n - start;
    out[..len].copy_from_slice(&bytes[start..n]);
    out[len] = 0;
    Some(len)
}

/// Does `path` (len bytes, NUL-terminated in `out`) equal `entry`?
fn path_eq(entry: &str, path: &[u8; 24], len: usize) -> bool {
    entry.as_bytes() == &path[..len]
}

/// The display name of an entry: the path component after the last slash.
fn leaf(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

/// Does `entry` appear when listing directory `dir`?  Top-level (dir = "")
/// shows entries without a slash; deeper dirs show their direct children.
fn listed_by(entry: &Entry, dir: &[u8; 24], dir_len: usize) -> bool {
    let p = entry.path.as_bytes();
    if dir_len == 0 {
        return !p.contains(&b'/');
    }
    p.starts_with(&dir[..dir_len]) && p.get(dir_len) == Some(&b'/') && !p[dir_len + 1..].contains(&b'/')
}

/// The displayed size of an entry.
fn entry_size(e: &Entry) -> u32 {
    if e.kind == DIR {
        0
    } else if !e.data.is_empty() {
        e.data.len() as u32
    } else {
        // /bin app images: the kernel's embedded image size.
        match e.path {
            "bin/counter" => SZ_COUNTER,
            "bin/clock" => SZ_CLOCK,
            "bin/ui-demo" => SZ_UI_DEMO,
            "bin/hog" => SZ_HOG,
            _ => 0,
        }
    }
}

fn reply(dst: u32, mtype: u32, data: &[u32]) {
    let mut m = Message::new(mtype);
    for (i, v) in data.iter().take(8).enumerate() {
        m.data[i] = *v;
    }
    sys::send(dst, &m);
}

fn serve_readdir(dst: u32, dir: &[u8; 24], dir_len: usize, offset: usize) {
    let mut skipped = 0usize;
    for e in FILES.iter() {
        if !listed_by(e, dir, dir_len) {
            continue;
        }
        if skipped < offset {
            skipped += 1;
            continue;
        }
        let name = leaf(e.path);
        let mut name_bytes = [0u8; 8];
        for (i, b) in name_bytes.iter_mut().enumerate() {
            *b = *name.as_bytes().get(i).unwrap_or(&0);
        }
        let mut data = [0u32; 8];
        data[0] = 1;
        data[1] = u32::from_le_bytes([name_bytes[0], name_bytes[1], name_bytes[2], name_bytes[3]]);
        data[2] = u32::from_le_bytes([name_bytes[4], name_bytes[5], name_bytes[6], name_bytes[7]]);
        data[3] = (e.kind == DIR) as u32;
        data[4] = entry_size(e);
        reply(dst, M_RAMFS_READDIR_REPLY, &data);
        return;
    }
    reply(dst, M_RAMFS_READDIR_REPLY, &[0]);
}

fn serve_read(dst: u32, path: &[u8; 24], path_len: usize, offset: u32) {
    for e in FILES.iter() {
        if e.kind != FILE || !path_eq(e.path, path, path_len) {
            continue;
        }
        let off = offset as usize;
        if off >= e.data.len() {
            reply(dst, M_RAMFS_READ_REPLY, &[0]);
            return;
        }
        let n = (e.data.len() - off).min(28);
        let mut data = [0u32; 8];
        data[0] = n as u32;
        for (i, b) in e.data[off..off + n].iter().enumerate() {
            data[1 + i / 4] |= (*b as u32) << (8 * (i % 4));
        }
        reply(dst, M_RAMFS_READ_REPLY, &data);
        return;
    }
    reply(dst, M_RAMFS_READ_REPLY, &[0]);
}

#[no_mangle]
pub extern "C" fn server_main(_info: *const BootInfo) -> ! {
    sys::log(0, "ramfs: up");
    sys::log(0, "ramfs: /bin (4 apps) /etc (2 files)");

    loop {
        let (src, msg) = sys::receive(M_ANY);
        match msg.mtype {
            M_RAMFS_READDIR => {
                let mut path = [0u8; 24];
                let len = read_path(&msg.data, &mut path).unwrap_or(0);
                serve_readdir(src, &path, len, msg.data[6] as usize);
            }
            M_RAMFS_READ => {
                let mut path = [0u8; 24];
                let len = read_path(&msg.data, &mut path).unwrap_or(0);
                serve_read(src, &path, len, msg.data[6]);
            }
            _ => {}
        }
    }
}
