//! virtio-gpu driver (virtio 1.2 §5.7).
//!
//! The display server owns the GPU: a framebuffer resource is created with
//! `RESOURCE_CREATE_2D`, its pages attached with `ATTACH_BACKING`, bound to
//! scanout 0 with `SET_SCANOUT`, and each rendered frame is pushed with
//! `TRANSFER_TO_HOST_2D` (copies the attached guest pages into the host-side
//! resource image) followed by `RESOURCE_FLUSH`.

use core::mem::size_of;

use crate::virtio::{Device, VirtQueue};
use tanix_libsys::{fmt::StrBuf, sys};

pub const DEVICE_ID_GPU: u32 = 16;

// Commands.
const CMD_GET_DISPLAY_INFO: u32 = 0x100;
const CMD_RESOURCE_CREATE_2D: u32 = 0x101;
const CMD_RESOURCE_SET_SCANOUT: u32 = 0x103;
const CMD_RESOURCE_FLUSH: u32 = 0x104;
const CMD_TRANSFER_TO_HOST_2D: u32 = 0x105;
const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x106;

// Responses (virtio-gpu §5.7.6.1).
const RESP_OK_NODATA: u32 = 0x1100;
const RESP_OK_DISPLAY_INFO: u32 = 0x1101;

/// BGRA8888 — QEMU's native display format.
pub const FORMAT_BGRA8888: u32 = 2;

/// Maximum backing entries (1280×800 BGRA8888 = 1000 frames).
const MAX_ENTRIES: usize = 1024;

/// Display mode reported by the device (per scanout).
#[derive(Clone, Copy)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
}

/// 24-byte header shared by every GPU command/response (QEMU 11 header:
/// type, flags, fence_id, ctx_id, ring_idx, padding — see virtio_gpu.h).
#[repr(C)]
struct Header {
    ctrl_type: u32,
    flags: u32,
    fence_id: u64,
    ctx_id: u32,
    ring_idx: u8,
    padding: [u8; 3],
}

/// Response to GET_DISPLAY_INFO: mode descriptors for all scanouts.
#[repr(C)]
struct DisplayInfoResp {
    header: Header,
    modes: [ModeDesc; 16],
}

#[repr(C)]
struct ModeDesc {
    rect: Rect,
    enabled: u32,
    flags: u32,
}

#[repr(C)]
struct Rect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
struct ResourceCreate2d {
    header: Header,
    resource_id: u32,
    format: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
struct AttachBacking {
    header: Header,
    resource_id: u32,
    nr_entries: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct BackingEntry {
    addr: u64,
    length: u32,
    padding: u32,
}

#[repr(C)]
struct SetScanout {
    header: Header,
    rect: Rect,
    scanout_id: u32,
    resource_id: u32,
}

#[repr(C)]
struct Flush {
    header: Header,
    rect: Rect,
    resource_id: u32,
    padding: u32,
}

#[repr(C)]
struct Transfer2d {
    header: Header,
    rect: Rect,
    offset: u64,
    resource_id: u32,
    padding: u32,
}

/// Storage for one in-flight request (any command + response fit here).
#[repr(C, align(16))]
struct Command {
    header: Header,
    payload: [u8; 512],
}

impl Command {
    const fn empty() -> Self {
        Self {
            header: Header {
                ctrl_type: 0,
                flags: 0,
                fence_id: 0,
                ctx_id: 0,
                ring_idx: 0,
                padding: [0; 3],
            },
            payload: [0; 512],
        }
    }
    fn new(ctrl_type: u32) -> Self {
        let mut c = Self::empty();
        c.header.ctrl_type = ctrl_type;
        c
    }
}

// One dedicated, never-reused buffer per operation.  QEMU 11 dispatches
// GPU commands asynchronously (bottom half) and reads the command struct
// live from guest RAM at dispatch time; stack locals that the compiler
// overlaps across scopes get clobbered before the device reads them.
static mut CMD_DISP_INFO: Command = Command::empty();
static mut CMD_CREATE: Command = Command::empty();
static mut CMD_ATTACH: Command = Command::empty();
static mut ENTRIES: [BackingEntry; MAX_ENTRIES] =
    [BackingEntry { addr: 0, length: 0, padding: 0 }; MAX_ENTRIES];
static mut CMD_SET_SCANOUT: Command = Command::empty();
static mut CMD_TRANSFER: Command = Command::empty();
static mut CMD_FLUSH: Command = Command::empty();
static mut RESP: [u8; 1024] = [0; 1024];

/// A GPU device with its control queue.
pub struct Gpu {
    dev: Device,
    queue: VirtQueue,
    /// Physical address of the framebuffer pages.
    pub fb_base: u64,
    /// Framebuffer stride in bytes (= width × 4 for BGRA8888).
    pub stride: usize,
    pub width: u32,
    pub height: u32,
}

/// Round-trip one command: chain `reqs` (READ) then `resp` (WRITE).
fn roundtrip(
    dev: &Device,
    queue: &mut VirtQueue,
    reqs: &[&[u8]],
    resp: &mut [u8],
) -> Option<u32> {
    let n = dev.submit(queue, reqs, resp);
    if n < size_of::<Header>() as u32 {
        return None;
    }
    let rt = unsafe { &*(resp.as_ptr() as *const Header) }.ctrl_type;
    Some(rt)
}

impl Gpu {
    /// Probe, reset, negotiate and initialise the GPU's control queue.
    pub fn open() -> Option<Self> {
        let dev = crate::virtio::find(DEVICE_ID_GPU)?;
        sys::log(0, "gpu: device found");
        dev.reset();
        sys::log(0, "gpu: reset");

        // Legacy transport: accept everything the host offers.
        dev.negotiate(dev.host_features());
        dev.driver_ok();
        sys::log(0, "gpu: features + driver_ok");

        let mut queue = VirtQueue::new()?;
        sys::log(0, "gpu: queue frames allocated");
        let size = dev.setup_queue(0, &mut queue)?;
        {
            let mut buf = StrBuf::new();
            buf.push_str("gpu: queue ready (size=");
            buf.push_dec32(size);
            buf.push_str(")");
            sys::log(0, buf.as_str());
        }

        Some(Gpu {
            dev,
            queue,
            fb_base: 0,
            stride: 0,
            width: 0,
            height: 0,
        })
    }

    /// Query the display modes; returns the first enabled scanout size.
    pub fn display_info(&mut self) -> Option<DisplayMode> {
        let cmd = unsafe { &mut CMD_DISP_INFO };
        *cmd = Command::new(CMD_GET_DISPLAY_INFO);
        let resp = unsafe { &mut RESP[..1024] };
        let n = self.dev.submit(&mut self.queue, &[bytes(&*cmd)], resp);
        if n < size_of::<Header>() as u32 {
            return None;
        }
        let info = unsafe { &*(resp.as_ptr() as *const DisplayInfoResp) };
        if info.header.ctrl_type != RESP_OK_DISPLAY_INFO {
            return None;
        }
        for m in info.modes.iter() {
            if m.enabled != 0 && m.rect.width != 0 {
                return Some(DisplayMode {
                    width: m.rect.width,
                    height: m.rect.height,
                });
            }
        }
        None
    }

    /// Allocate the framebuffer (BGRA8888) from kernel frames and create
    /// the GPU resource, then bind it to scanout 0.
    pub fn setup_framebuffer(&mut self, mode: &DisplayMode) -> Option<()> {
        let width = mode.width;
        let height = mode.height;
        let stride = width as usize * 4;
        let fb_bytes = stride * height as usize;
        let pages = fb_bytes.div_ceil(4096);
        if pages > MAX_ENTRIES {
            sys::log(1, "gpu: framebuffer too large");
            return None;
        }

        let base = sys::alloc_frames(pages as u32);
        if base == 0 {
            sys::log(1, "gpu: framebuffer allocation failed");
            return None;
        }
        self.fb_base = base;
        self.stride = stride;
        self.width = width;
        self.height = height;

        // RESOURCE_CREATE_2D.
        {
            let cmd = unsafe { &mut CMD_CREATE };
            *cmd = Command::new(CMD_RESOURCE_CREATE_2D);
            let create = unsafe { &mut *(bytes(&*cmd).as_ptr() as *mut ResourceCreate2d) };
            create.resource_id = 1;
            create.format = FORMAT_BGRA8888;
            create.width = width;
            create.height = height;
            let resp = unsafe { &mut RESP[..64] };
            let rc = roundtrip(&self.dev, &mut self.queue, &[bytes(&*cmd)], resp);
            if rc != Some(RESP_OK_NODATA) {
                sys::log(1, "gpu: resource create failed");
                return None;
            }
        }

        // RESOURCE_ATTACH_BACKING: header + one entry per 4 KiB frame,
        // sent as separate chained descriptors (the response follows).
        {
            let cmd = unsafe { &mut CMD_ATTACH };
            *cmd = Command::new(CMD_RESOURCE_ATTACH_BACKING);
            let attach = unsafe { &mut *(bytes(&*cmd).as_ptr() as *mut AttachBacking) };
            attach.resource_id = 1;
            attach.nr_entries = pages as u32;
            let entries = unsafe { &mut ENTRIES[..pages] };
            for (i, e) in entries.iter_mut().enumerate() {
                *e = BackingEntry {
                    addr: base + (i as u64 * 4096),
                    length: if i == pages - 1 { (fb_bytes - i * 4096) as u32 } else { 4096 },
                    padding: 0,
                };
            }
            let resp = unsafe { &mut RESP[..64] };
            let rc = roundtrip(
                &self.dev,
                &mut self.queue,
                &[&bytes(&*cmd)[..size_of::<AttachBacking>()], bytes(entries)],
                resp,
            );
            if rc != Some(RESP_OK_NODATA) {
                sys::log(1, "gpu: attach backing failed");
                return None;
            }
        }

        // RESOURCE_SET_SCANOUT.
        {
            let cmd = unsafe { &mut CMD_SET_SCANOUT };
            *cmd = Command::new(CMD_RESOURCE_SET_SCANOUT);
            let set = unsafe { &mut *(bytes(&*cmd).as_ptr() as *mut SetScanout) };
            set.resource_id = 1;
            set.scanout_id = 0;
            set.rect = Rect { x: 0, y: 0, width, height };
            let resp = unsafe { &mut RESP[..64] };
            let rc = roundtrip(&self.dev, &mut self.queue, &[bytes(&*cmd)], resp);
            if rc != Some(RESP_OK_NODATA) {
                sys::log(1, "gpu: set scanout failed");
                return None;
            }
        }

        Some(())
    }

    /// Push the whole framebuffer to the display: copy the attached guest
    /// pages into the host-side resource image, then flush it to the
    /// scanout.
    pub fn flush(&mut self) -> Option<()> {
        // TRANSFER_TO_HOST_2D: the scanout displays the *host* resource
        // image; the attached guest pages only reach it via this copy.
        {
            let cmd = unsafe { &mut CMD_TRANSFER };
            *cmd = Command::new(CMD_TRANSFER_TO_HOST_2D);
            let t = unsafe { &mut *(bytes(&*cmd).as_ptr() as *mut Transfer2d) };
            t.rect = Rect {
                x: 0,
                y: 0,
                width: self.width,
                height: self.height,
            };
            t.offset = 0;
            t.resource_id = 1;
            t.padding = 0;
            let resp = unsafe { &mut RESP[..64] };
            let rc = roundtrip(&self.dev, &mut self.queue, &[bytes(&*cmd)], resp);
            if rc != Some(RESP_OK_NODATA) {
                return None;
            }
        }

        let cmd = unsafe { &mut CMD_FLUSH };
        *cmd = Command::new(CMD_RESOURCE_FLUSH);
        let flush = unsafe { &mut *(bytes(&*cmd).as_ptr() as *mut Flush) };
        flush.resource_id = 1;
        flush.padding = 0;
        flush.rect = Rect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        };
        let resp = unsafe { &mut RESP[..64] };
        let rc = roundtrip(&self.dev, &mut self.queue, &[bytes(&*cmd)], resp);
        if rc == Some(RESP_OK_NODATA) {
            Some(())
        } else {
            None
        }
    }

    /// Mutable view over the whole framebuffer (BGRA8888).
    pub fn fb_mut(&mut self) -> &mut [u8] {
        unsafe {
            core::slice::from_raw_parts_mut(
                self.fb_base as *mut u8,
                self.stride * self.height as usize,
            )
        }
    }

    /// Fill a rectangle with an RGB colour (BGRA8888 pixels).
    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, rgb: (u8, u8, u8)) {
        let width = self.width;
        let height = self.height;
        let stride = self.stride;
        let fb = self.fb_mut();
        let (r, g, b) = rgb;        for row in y..(y + h).min(height) {
            for col in x..(x + w).min(width) {
                let off = (row as usize * stride) + (col as usize * 4);
                fb[off] = b;
                fb[off + 1] = g;
                fb[off + 2] = r;
                fb[off + 3] = 0xFF;
            }
        }
    }
}

fn bytes<T: ?Sized>(v: &T) -> &[u8] {
    unsafe {
        core::slice::from_raw_parts(v as *const T as *const u8, core::mem::size_of_val(v))
    }
}
