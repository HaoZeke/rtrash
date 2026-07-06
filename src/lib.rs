//! FreeDesktop trash library used by the `rtrash` CLI and optional Python bindings.
//!
//! This crate implements the freedesktop.org Trash specification with an
//! rm-compatible put path. The binary is a thin multi-call dispatcher over
//! the same modules PyO3 wraps when the `python` feature is enabled.

pub mod empty;
pub mod fastdelete;
pub mod info;
pub mod list;
pub mod put;
pub mod restore;
pub mod rm;
pub mod status;
pub mod trashdir;
pub mod util;

#[cfg(feature = "python")]
mod python;

/// Library / package version string.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
