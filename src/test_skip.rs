//! The skip note shared by integration tests and unit tests.
//!
//! `cargo test` and nextest both hide the stderr of a passing test, and CI
//! prints the file `RTRASH_SKIP_LOG` names after the suite. [`skip`] writes
//! that file. Integration tests call it through `tests/common::skip`.
//! A unit test that wants a private log sets [`override_log`] on its own
//! thread and leaves the process environment alone: other threads in the
//! same `cargo test` process are reading it.

use std::cell::RefCell;
use std::io::Write;
use std::path::{Path, PathBuf};

thread_local! {
    static LOG_OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Skip notes on this thread go to `path` instead of `RTRASH_SKIP_LOG`.
/// Dropping the guard restores the process log for this thread only.
pub fn override_log(path: &Path) -> LogOverride {
    LOG_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(path.to_path_buf()));
    LogOverride
}

/// Clears a thread's [`override_log`] when dropped.
#[must_use = "the override ends when the guard is dropped"]
pub struct LogOverride;

impl Drop for LogOverride {
    fn drop(&mut self) {
        LOG_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
    }
}

fn log_path() -> Option<PathBuf> {
    LOG_OVERRIDE
        .with(|slot| slot.borrow().clone())
        .or_else(|| std::env::var_os("RTRASH_SKIP_LOG").map(PathBuf::from))
}

/// Print `why` on stderr and append one line to this thread's log: the
/// test binary, the thread name (libtest names the thread after the test)
/// and `why`. The log is the thread's [`override_log`] path, or
/// `RTRASH_SKIP_LOG` when this thread set none. A failed write prints its
/// error on stderr; the test goes on.
pub fn skip(why: &str) {
    eprintln!("{why}");
    let Some(log) = log_path() else {
        return;
    };
    let exe = std::env::current_exe().unwrap_or_default();
    let stem = exe.file_stem().unwrap_or_default().to_string_lossy();
    // Cargo names a test binary NAME-HASH.
    let bin = stem.rsplit_once('-').map_or(&*stem, |(name, _)| name);
    let current = std::thread::current();
    let test = current.name().unwrap_or("?");
    let line = format!("{bin} {test}: {why}\n");
    let wrote = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .and_then(|mut file| file.write_all(line.as_bytes()));
    if let Err(e) = wrote {
        let shown = log.display().to_string();
        eprintln!("RTRASH_SKIP_LOG {shown}: {e}");
    }
}
