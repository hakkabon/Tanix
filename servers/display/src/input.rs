//! virtio-input (virtio 1.2 §5.10) tablet and keyboard drivers.
//!
//! QEMU's `virtio-tablet-device` is an absolute pointing device: it emits
//! `EV_ABS` (`ABS_X`/`ABS_Y`) plus `EV_KEY` (`BTN_TOUCH`/`BTN_LEFT`) events
//! into the event queue, batched and terminated by `EV_SYN`/`SYN_REPORT`.
//! Absolute coordinates range 0..0x7FFF (QEMU `INPUT_EVENT_ABS_MAX`); the
//! caller scales them to the screen.
//!
//! QEMU's `virtio-keyboard-device` (Phase 9) emits `EV_KEY` events with
//! Linux input keycodes (code 1..=127, value 0 = release, 1 = press, 2 =
//! autorepeat) into its own event queue, also SYN-terminated.  The keyboard
//! is distinguished from the tablet via the device config (EV_BITS bitmap):
//! the tablet advertises `EV_ABS`, the keyboard does not.
//!
//! Each driver hands the device a ring of empty 8-byte buffers; the device
//! fills one buffer per event and returns it in the used ring.  The status
//! queue (queue 1) is unused — the devices need no driver configuration.

use core::mem::size_of;

use crate::virtio::{Device, VirtQueue, QUEUE_SIZE};
use tanix_libsys::sys;

pub const DEVICE_ID_INPUT: u32 = 18;

// Event types (linux/input.h).
// (EV_SYN = 0x00 marks the end of an event batch; the parsers below only
// consume the payloads they care about and ignore the rest.)
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;

// EV_ABS codes.
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;

// EV_KEY codes.
const BTN_TOUCH: u16 = 0x14a;
const BTN_MOUSE: u16 = 0x110; // = BTN_LEFT

/// QEMU's absolute-coordinate range (INPUT_EVENT_ABS_MAX).
pub const ABS_MAX: u32 = 0x7FFF;

/// A single input event as written by the device (`virtio_input_event`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct InputEvent {
    pub kind: u16,
    pub code: u16,
    pub value: u32,
}

/// Latest pointer state in raw tablet coordinates.
#[derive(Clone, Copy, Default)]
pub struct Pointer {
    pub x: u32,
    pub y: u32,
    pub buttons: u32,
}

/// One empty event buffer per ring slot.
static mut EVBUF: [InputEvent; QUEUE_SIZE] =
    [InputEvent { kind: 0, code: 0, value: 0 }; QUEUE_SIZE];

/// virtio-input event-queue driver for the QEMU tablet.
pub struct Tablet {
    dev: Device,
    queue: VirtQueue,
    state: Pointer,
}

impl Tablet {
    /// Probe for the tablet, bring it to DRIVER_OK and hand it the full
    /// ring of empty event buffers.
    pub fn open() -> Option<Self> {
        let dev = match crate::virtio::find(DEVICE_ID_INPUT) {
            Some(d) => {
                sys::log(0, "tablet: device found");
                d
            }
            None => {
                sys::log(1, "tablet: device not found");
                return None;
            }
        };
        let mut queue = match VirtQueue::new() {
            Some(q) => q,
            None => {
                sys::log(1, "tablet: vring allocation failed");
                return None;
            }
        };
        dev.reset();
        if dev.setup_queue(0, &mut queue).is_none() {
            sys::log(1, "tablet: queue setup failed");
            return None;
        }
        dev.negotiate(0);
        dev.driver_ok();

        let base = core::ptr::addr_of_mut!(EVBUF) as *mut u8;
        let added =
            dev.add_empty_buffers(&mut queue, base, size_of::<InputEvent>() as u32, QUEUE_SIZE);
        if added == 0 {
            sys::log(1, "tablet: no event buffers added");
            return None;
        }
        sys::log(0, "tablet: online");
        Some(Self {
            dev,
            queue,
            state: Pointer { x: 0, y: 0, buttons: 0 },
        })
    }

    /// Drain all completed events and return the latest pointer state.
    pub fn poll(&mut self) -> Pointer {
        let mut done = [(0u16, 0u32); 8];
        let n = self.dev.drain_used(&mut self.queue, &mut done);
        if n == 0 {
            return self.state;
        }
        for &(id, _len) in &done[..n] {
            let ev = unsafe { &(*core::ptr::addr_of_mut!(EVBUF))[id as usize % QUEUE_SIZE] };
            match (ev.kind, ev.code) {
                (EV_ABS, ABS_X) => self.state.x = ev.value,
                (EV_ABS, ABS_Y) => self.state.y = ev.value,
                (EV_KEY, BTN_TOUCH) | (EV_KEY, BTN_MOUSE) => {
                    self.state.buttons = ev.value & 1;
                }
                _ => {}
            }
        }
        // Hand the consumed buffers back to the device.
        let base = core::ptr::addr_of_mut!(EVBUF) as *mut u8;
        self.dev
            .add_empty_buffers(&mut self.queue, base, size_of::<InputEvent>() as u32, n);
        self.state
    }
}

/// One empty event buffer per ring slot (keyboard — separate ring from the
/// tablet so the two devices never trample each other's buffers).
static mut KBD_EVBUF: [InputEvent; QUEUE_SIZE] =
    [InputEvent { kind: 0, code: 0, value: 0 }; QUEUE_SIZE];

/// virtio-input event-queue driver for the QEMU keyboard.
pub struct Keyboard {
    dev: Device,
    queue: VirtQueue,
}

impl Keyboard {
    /// Probe for the keyboard, bring it to DRIVER_OK and hand it the full
    /// ring of empty event buffers.  A keyboard is an input device that
    /// advertises `EV_KEY` but no `EV_ABS` (the tablet advertises both).
    pub fn open() -> Option<Self> {
        let mut found: Option<Device> = None;
        crate::virtio::for_each(DEVICE_ID_INPUT, |dev| {
            if dev.supports_event(EV_KEY) && !dev.supports_event(EV_ABS) {
                found = Some(dev);
            }
        });
        let dev = match found {
            Some(d) => {
                sys::log(0, "kbd: device found");
                d
            }
            None => {
                sys::log(1, "kbd: no keyboard (add -device virtio-keyboard-device)");
                return None;
            }
        };
        let mut queue = match VirtQueue::new() {
            Some(q) => q,
            None => {
                sys::log(1, "kbd: vring allocation failed");
                return None;
            }
        };
        dev.reset();
        if dev.setup_queue(0, &mut queue).is_none() {
            sys::log(1, "kbd: queue setup failed");
            return None;
        }
        dev.negotiate(0);
        dev.driver_ok();

        let base = core::ptr::addr_of_mut!(KBD_EVBUF) as *mut u8;
        let added = dev.add_empty_buffers(&mut queue, base, size_of::<InputEvent>() as u32, QUEUE_SIZE);
        if added == 0 {
            sys::log(1, "kbd: no event buffers added");
            return None;
        }
        sys::log(0, "kbd: online");
        Some(Self { dev, queue })
    }

    /// Drain all completed key events into `out` as (keycode, value) pairs.
    /// Returns how many were written.  Only EV_KEY payloads are returned;
    /// SYN/other event types are consumed and dropped.
    pub fn poll(&mut self, out: &mut [(u16, u16)]) -> usize {
        let mut done = [(0u16, 0u32); 8];
        let n = self.dev.drain_used(&mut self.queue, &mut done);
        let mut written = 0;
        for &(id, _len) in &done[..n] {
            let ev = unsafe { &(*core::ptr::addr_of_mut!(KBD_EVBUF))[id as usize % QUEUE_SIZE] };
            if ev.kind == EV_KEY && written < out.len() {
                out[written] = (ev.code, ev.value as u16);
                written += 1;
            }
        }
        // Hand the consumed buffers back to the device.
        if n > 0 {
            let base = core::ptr::addr_of_mut!(KBD_EVBUF) as *mut u8;
            self.dev
                .add_empty_buffers(&mut self.queue, base, size_of::<InputEvent>() as u32, n);
        }
        written
    }
}
