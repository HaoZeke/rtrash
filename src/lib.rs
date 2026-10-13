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

pub mod config;
pub mod ds_store;
pub mod du;
pub mod du_dev;
pub mod du_extent;
pub mod du_filter;
pub mod du_fs;
pub mod du_git;
pub mod du_interrupt;
pub mod du_ncdu;
pub mod du_ncdu_bin;
pub mod du_snap;
#[cfg(feature = "tui")]
pub mod du_tui;
#[cfg(unix)]
pub mod empty;
#[cfg(all(unix, feature = "tui"))]
pub mod empty_tui;
#[cfg(unix)]
pub mod fastdelete;
pub mod filters;
#[cfg(unix)]
pub mod finder;
#[cfg(unix)]
pub mod holders;
#[cfg(unix)]
pub mod info;
#[cfg(unix)]
pub mod list;
#[cfg(unix)]
pub mod put;
#[cfg(all(unix, feature = "tui"))]
pub mod put_tui;
#[cfg(unix)]
pub mod reap;
#[cfg(unix)]
pub mod reap_profiles;
#[cfg(unix)]
pub mod restore;
#[cfg(feature = "tui")]
pub mod restore_tui;
#[cfg(unix)]
pub mod retention;
#[cfg(unix)]
pub mod rm;
#[cfg(unix)]
pub mod safety;
#[cfg(unix)]
pub mod setup;
pub mod space;
#[cfg(unix)]
pub mod status;
#[cfg(unix)]
pub mod trashdir;
#[cfg(feature = "tui")]
pub mod tui_binds;
#[cfg(feature = "tui")]
pub mod tui_fuzzy;
#[cfg(feature = "tui")]
pub mod tui_keys;
#[cfg(feature = "tui")]
pub mod tui_list;
#[cfg(feature = "tui")]
pub mod tui_select;
#[cfg(feature = "tui")]
pub mod tui_term;
#[cfg(unix)]
pub mod undo;
#[cfg(unix)]
pub mod who;

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
pub mod undo {
    pub fn run(prog: &str, _args: &[String]) -> i32 {
        eprintln!("{prog}: undo needs the FreeDesktop trash; on Windows, use restore");
        1
    }
}
#[cfg(windows)]
pub mod finder {
    pub fn run(prog: &str, _args: &[String]) -> i32 {
        eprintln!("{prog}: the Finder Trash exists on macOS only");
        1
    }
}
#[cfg(windows)]
pub mod status {
    pub fn run(prog: &str, args: &[String]) -> i32 {
        crate::win_recycle::status_run(prog, args)
    }
}

#[cfg(windows)]
pub mod who {
    pub fn run(prog: &str, _args: &[String]) -> i32 {
        eprintln!("{prog}: cannot list the processes holding a path on Windows");
        1
    }
}

#[cfg(windows)]
pub mod reap {
    pub fn run(prog: &str, _args: &[String]) -> i32 {
        eprintln!("{prog}: reap needs to see which processes hold files, which rtrash cannot do on Windows");
        1
    }
}

#[cfg(all(unix, feature = "python"))]
mod python;

/// Library / package version string.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
