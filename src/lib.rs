//! FreeDesktop trash library used by the `rtrash` CLI and optional Python bindings.
//!
//! On Linux this crate implements the freedesktop.org Trash specification with an
//! rm-compatible put path. On **macOS** (experimental) plain put, list, restore
//! and empty use the Finder Trash (see [`finder`]); the FreeDesktop *home*
//! trash stays one setting away. On **Windows**, put/list/restore/empty
//! use the system Recycle Bin (see [`win_recycle`]), not a FreeDesktop fiction.
//! The binary is a thin multi-call dispatcher over these backends; PyO3 wraps the
//! FreeDesktop path when the `python` feature is enabled (Unix).

pub mod allow_roots;
pub mod answer;
pub mod listfmt;
pub mod platform;
pub mod util;
pub mod win_recycle;
#[cfg(unix)]
pub mod wsl;
#[cfg(unix)]
pub mod wsl_bin;

pub mod budget;
pub mod compat;
pub mod config;
pub mod ds_store;
pub mod du;
pub mod du_dev;
pub mod du_dupes;
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
pub mod file_replace;
pub mod filters;
#[cfg(unix)]
pub mod finder;
#[cfg(unix)]
pub mod finder_shared;
#[cfg(unix)]
pub mod finder_total;
pub mod holders;
#[cfg(unix)]
pub mod info;
#[cfg(unix)]
pub mod interrupt;
#[cfg(unix)]
pub mod list;
#[cfg(unix)]
pub mod mount_probe;
pub mod permanent_guard;
#[cfg(unix)]
pub mod put;
#[cfg(all(unix, feature = "tui"))]
pub mod put_tui;
pub mod reap;
pub mod reap_profiles;
pub mod reap_timer;
#[cfg(unix)]
pub mod restore;
#[cfg(feature = "tui")]
pub mod restore_tui;
#[cfg(unix)]
pub mod retention;
#[cfg(unix)]
pub mod rm;
pub mod rm_ask;
#[cfg(unix)]
pub mod safety;
#[cfg(unix)]
pub mod setup;
pub mod shell_assets;
pub mod space;
#[cfg(unix)]
pub mod status;
pub mod status_json;
#[cfg(unix)]
pub mod trash_check;
pub mod trash_history;
#[cfg(unix)]
pub mod trash_inodes;
pub mod trash_stats;
#[cfg(unix)]
pub mod trash_total;
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
pub mod who;

// Windows CLI surface: system Recycle Bin (not FreeDesktop modules).
#[cfg(windows)]
pub mod empty {
    pub fn run(prog: &str, args: &[String]) -> i32 {
        let bins = crate::trash_history::bin_snapshot();
        let code = crate::win_recycle::empty_run(prog, args);
        crate::trash_history::log_bins("empty", &bins);
        code
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
        let bins = crate::trash_history::bin_snapshot();
        let code = crate::win_recycle::put_run(prog, args);
        crate::trash_history::log_bins("put", &bins);
        code
    }
}
#[cfg(windows)]
pub mod restore {
    pub fn run(prog: &str, args: &[String]) -> i32 {
        let bins = crate::trash_history::bin_snapshot();
        let code = crate::win_recycle::restore_run(prog, args);
        crate::trash_history::log_bins("restore", &bins);
        code
    }
}
#[cfg(windows)]
pub mod rm {
    pub fn run(prog: &str, args: &[String]) -> i32 {
        let bins = crate::trash_history::bin_snapshot();
        let code = crate::win_recycle::rm_run(prog, args);
        crate::trash_history::log_bins("rm", &bins);
        code
    }
}
#[cfg(windows)]
pub mod setup {
    const HELP: &str = "\
Usage: rtrash completions {bash|zsh|fish|nu|powershell|elvish}
       rtrash man

completions: print the embedded completion script for that shell
  (powershell, also pwsh, is the one for Windows).
man: print the embedded man(1) page.
";

    pub fn run(prog: &str, args: &[String]) -> i32 {
        match prog {
            "completions" => crate::shell_assets::run_completions(args, HELP),
            "man" => crate::shell_assets::run_man(args, HELP),
            _ => crate::win_recycle::setup_run(prog, args),
        }
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

#[cfg(all(unix, feature = "python"))]
mod python;

/// Library / package version string.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
