//! Architecture-specific code.
//!
//! Only aarch64 is supported.  Future phases might add a thin HAL trait here
//! so the rest of the kernel stays architecture-agnostic.

#[cfg(target_arch = "aarch64")]
pub mod aarch64;
