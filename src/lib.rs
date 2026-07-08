//! FreeDesktop trash library used by the `rtrash` CLI and optional Python bindings.
//!
//! On Linux this crate implements the freedesktop.org Trash specification with an
//! rm-compatible put path. On **macOS** (experimental) the same FreeDesktop *home*
//! trash layout is used — not Finder Trash. On **Windows**, put/list/restore/empty
//! use the system Recycle Bin (see [`win_recycle`]), not a FreeDesktop fiction.
//! The binary is a thin multi-call dispatcher over these backends; PyO3 wraps the
//! FreeDesktop path when the `python` feature is enabled (Unix).

pub mod platform;
pub mod util;
pub mod win_recycle;

#[cfg(unix)]
pub mod empty;
#[cfg(all(unix, feature = "tui"))]
pub mod empty_tui;
#[cfg(unix)]
pub mod fastdelete;
#[cfg(unix)]
pub mod info;
#[cfg(unix)]
pub mod list;
#[cfg(unix)]
pub mod put;
#[cfg(all(unix, feature = "tui"))]
pub mod put_tui;
#[cfg(unix)]
pub mod restore;
#[cfg(all(unix, feature = "tui"))]
pub mod restore_tui;
#[cfg(unix)]
pub mod rm;
#[cfg(unix)]
pub mod setup;
#[cfg(unix)]
pub mod status;
#[cfg(unix)]
pub mod trashdir;
#[cfg(all(unix, feature = "tui"))]
pub mod tui_fuzzy;
#[cfg(all(unix, feature = "tui"))]
pub mod tui_keys;
#[cfg(all(unix, feature = "tui"))]
pub mod tui_list;
#[cfg(all(unix, feature = "tui"))]
pub mod tui_select;
#[cfg(all(unix, feature = "tui"))]
pub mod tui_term;

// Windows CLI surface: system Recycle Bin (not FreeDesktop modules).
#[cfg(windows)]
pub mod empty {
    pub fn run(prog: &str, args: &[String]) -> i32 {
        crate::win_recycle::empty_run(prog, args)
    }
}
#[cfg(windows)]
pub mod list {
    pub fn run(prog: &str, args: &[String]) -> i32 {
        crate::win_recycle::list_run(prog, args)
    }
}
#[cfg(windows)]
pub mod put {
    pub fn run(prog: &str, args: &[String]) -> i32 {
        crate::win_recycle::put_run(prog, args)
    }
}
#[cfg(windows)]
pub mod restore {
    pub fn run(prog: &str, args: &[String]) -> i32 {
        crate::win_recycle::restore_run(prog, args)
    }
}
#[cfg(windows)]
pub mod rm {
    pub fn run(prog: &str, args: &[String]) -> i32 {
        crate::win_recycle::rm_run(prog, args)
    }
}
#[cfg(windows)]
pub mod setup {
    pub fn run(prog: &str, args: &[String]) -> i32 {
        crate::win_recycle::setup_run(prog, args)
    }
}
#[cfg(windows)]
pub mod status {
    pub fn run(prog: &str, args: &[String]) -> i32 {
        crate::win_recycle::status_run(prog, args)
    }
}

#[cfg(all(unix, feature = "python"))]
mod python;

/// Library / package version string.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
