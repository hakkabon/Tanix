//! Shared UI helpers for Tanix apps (Phase 8).
//!
//! Applications run as Minix-style servers and draw through the window
//! manager (`wm`): each app owns an off-screen canvas (`Window`), renders
//! into it with the raster helpers here, and the compositor blits windows
//! onto the display server's framebuffer.  `font` is the shared 5×7 bitmap
//! font — the display server uses it for title bars too.

#![no_std]

pub mod font;
pub mod window;

pub use window::{PointerEvent, Window};
