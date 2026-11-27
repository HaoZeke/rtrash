//! The skip note shared by integration tests and unit tests.
//!
//! `cargo test` and nextest both hide the stderr of a passing test, and CI
//! prints the file `RTRASH_SKIP_LOG` names after the suite. [`skip`] writes
//! that file. Integration tests call it through `tests/common::skip`.
//! A unit test that wants a private log sets [`override_log`] on its own
//! thread and leaves the process environment alone: other threads in the
//! same `cargo test` process are reading it. When the variable is unset,
//! [`log_when_unset`] names a stand-in for that thread only, and a value
//! in the environment still wins.

use std::cell::RefCell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

thread_local! {
    static LOG_OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    static LOG_WHEN_UNSET: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    static ARMED_HOLD: RefCell<Option<Arc<SkipHold>>> = const { RefCell::new(None) };
}

/// How long a [`SkipHold`] waits for the other parties before failing the test.
const HOLD_SECS: u64 = 60;

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

/// When this thread has no [`override_log`] and `RTRASH_SKIP_LOG` is unset,
/// skip notes go to `path`. A set variable still wins, including one another
/// thread installed for the whole process.
pub fn log_when_unset(path: &Path) -> UnsetLog {
    LOG_WHEN_UNSET.with(|slot| *slot.borrow_mut() = Some(path.to_path_buf()));
    UnsetLog
}

/// Clears a thread's [`log_when_unset`] when dropped.
#[must_use = "the stand-in ends when the guard is dropped"]
pub struct UnsetLog;

impl Drop for UnsetLog {
    fn drop(&mut self) {
        LOG_WHEN_UNSET.with(|slot| *slot.borrow_mut() = None);
    }
}

fn log_path() -> Option<PathBuf> {
    if let Some(path) = LOG_OVERRIDE.with(|slot| slot.borrow().clone()) {
        return Some(path);
    }
    if let Some(path) = std::env::var_os("RTRASH_SKIP_LOG") {
        return Some(PathBuf::from(path));
    }
    LOG_WHEN_UNSET.with(|slot| slot.borrow().clone())
}

/// Parties that must still be inside their log override while this thread's
/// [`skip`] reads the path and writes the line.
pub struct SkipHold {
    lock: Mutex<Meet>,
    cv: Condvar,
}

struct Meet {
    parties: usize,
    here: usize,
    wrote: bool,
}

impl SkipHold {
    /// `parties` includes the thread that skips and each thread that waits.
    pub fn new(parties: usize) -> Arc<Self> {
        Arc::new(Self {
            lock: Mutex::new(Meet {
                parties,
                here: 0,
                wrote: false,
            }),
            cv: Condvar::new(),
        })
    }

    /// Block while this thread's log override is held, until the armed
    /// skip has written its line.
    pub fn wait_until_written(&self) {
        let mut guard = self.lock.lock().unwrap();
        guard.here += 1;
        if guard.here == guard.parties {
            self.cv.notify_all();
        }
        let guard = self.wait_while(guard, |meet| !meet.wrote);
        drop(guard);
    }

    /// Wait until every party is in [`wait_until_written`], then run `write`
    /// before letting them leave.
    fn during_write(&self, write: impl FnOnce()) {
        let mut guard = self.lock.lock().unwrap();
        guard.here += 1;
        if guard.here == guard.parties {
            self.cv.notify_all();
        }
        let guard = self.wait_while(guard, |meet| meet.here < meet.parties);
        drop(guard);
        write();
        let mut guard = self.lock.lock().unwrap();
        guard.wrote = true;
        self.cv.notify_all();
    }

    fn wait_while<'a>(
        &self,
        guard: MutexGuard<'a, Meet>,
        keep: impl FnMut(&mut Meet) -> bool,
    ) -> MutexGuard<'a, Meet> {
        let (guard, result) = self
            .cv
            .wait_timeout_while(guard, Duration::from_secs(HOLD_SECS), keep)
            .unwrap();
        if result.timed_out() {
            panic!("the skip hold waited {HOLD_SECS}s for the other parties");
        }
        guard
    }
}

/// The next [`skip`] on this thread waits for `hold` around its read and write.
#[must_use = "the hold ends when the guard is dropped"]
pub fn arm_hold(hold: Arc<SkipHold>) -> ArmedHold {
    ARMED_HOLD.with(|slot| *slot.borrow_mut() = Some(hold));
    ArmedHold
}

/// Clears this thread's [`arm_hold`] when dropped.
pub struct ArmedHold;

impl Drop for ArmedHold {
    fn drop(&mut self) {
        ARMED_HOLD.with(|slot| *slot.borrow_mut() = None);
    }
}

/// Print `why` on stderr and append one line to this thread's log: the
/// test binary, the thread name (libtest names the thread after the test)
/// and `why`. The log is the thread's [`override_log`] path, or
/// `RTRASH_SKIP_LOG` when this thread set none, or its [`log_when_unset`]
/// path when the variable is unset. A failed write prints its error on
/// stderr; the test goes on.
pub fn skip(why: &str) {
    eprintln!("{why}");
    let hold = ARMED_HOLD.with(|slot| slot.borrow_mut().take());
    if let Some(hold) = hold {
        hold.during_write(|| record(why));
    } else {
        record(why);
    }
}

fn record(why: &str) {
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
