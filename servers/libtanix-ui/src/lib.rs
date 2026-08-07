//! Iced-shaped UI toolkit for Tanix (Phase 5).
//!
//! Applications are written exactly like `iced::Sandbox` apps (init/update/
//! view + a widget tree) and rendered by the Tanix display server through a
//! shared framebuffer.  Porting a Tanix UI app to desktop Iced is a matter
//! of switching the backend: the API surface mirrors Iced's.

#![no_std]

extern crate alloc;
