//! Ctrl-C during a put, restore, undo, rm or `status --fix`, and around
//! every trash record.
//!
//! A batch command calls [`stop_then_quit`]: the first Ctrl-C asks the run
//! to stop once the item in hand is done, and the next one quits at once,
//! as gdu 5.38 does for a scan that is slow to wind down. The run checks
//! [`requested`] between items. A long copy across file systems inside
//! [`abortable`] stops at the first Ctrl-C, and its usual error path removes
//! the part copied.
//!
//! A [`Critical`] section (taking a trash name, writing its record and
//! moving the payload) never ends halfway: a Ctrl-C inside it waits until
//! the record and the payload agree, then acts. That holds for every
//! command, even one that never called [`stop_then_quit`]: without it, a
//! Ctrl-C inside a section quits as soon as the section ends. A process
//! that ignores SIGINT (`nohup`) keeps ignoring it.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};
use std::sync::Mutex;

/// Ctrl-C presses seen by the handler.
static PRESSES: AtomicU32 = AtomicU32::new(0);
/// Open [`Critical`] sections.
static CRITICAL: AtomicU32 = AtomicU32::new(0);
/// Open [`abortable`] scopes.
static ABORTABLE: AtomicU32 = AtomicU32::new(0);
/// A press came inside a section and waits for its end.
static DEFERRED: AtomicBool = AtomicBool::new(false);
/// [`stop_then_quit`] was called: a first press outside a section stops.
static STOP_FIRST: AtomicBool = AtomicBool::new(false);

/// The SIGINT disposition before ours, while ours is in place, and
/// whether a section put ours in place (and takes it out again).
struct Installed {
    saved: Option<libc::sigaction>,
    by_section: bool,
}

static STATE: Mutex<Installed> = Mutex::new(Installed {
    saved: None,
    by_section: false,
});

/// What one press does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Press {
    /// Ask the run to stop after the item in hand.
    Stop,
    /// Wait for the open section to end.
    Defer,
    /// End the process now, as Ctrl-C does by default.
    Quit,
}

/// What press number `n` does, with a section open or not.
fn on_press(n: u32, in_section: bool, stop_first: bool) -> Press {
    if in_section {
        Press::Defer
    } else if stop_first && n == 1 {
        Press::Stop
    } else {
        Press::Quit
    }
}

/// Whether a press deferred to the end of a section quits there: a
/// second press always does, and a first one unless the run stops on it.
fn quits_at_section_end(presses: u32, stop_first: bool) -> bool {
    !stop_first || presses >= 2
}

extern "C" fn handler(_: libc::c_int) {
    let n = PRESSES.fetch_add(1, SeqCst).saturating_add(1);
    match on_press(n, CRITICAL.load(SeqCst) > 0, STOP_FIRST.load(SeqCst)) {
        Press::Stop => {}
        Press::Defer => DEFERRED.store(true, SeqCst),
        Press::Quit => quit(),
    }
}

/// End the process as an unhandled Ctrl-C would, so the shell sees it.
fn quit() {
    // SAFETY: signal(2) and raise(3) are async-signal-safe. Inside the
    // handler SIGINT is blocked, so the raised one lands on return.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::raise(libc::SIGINT);
    }
}

/// Put the handler in place and return the disposition it replaced, or
/// `None` when SIGINT is ignored and stays so.
fn install() -> Option<libc::sigaction> {
    // SAFETY: plain sigaction(2) calls on zeroed structs; the handler
    // only touches atomics and calls async-signal-safe functions.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(libc::SIGINT, std::ptr::null(), &mut old) != 0
            || old.sa_sigaction == libc::SIG_IGN
        {
            return None;
        }
        let mut new: libc::sigaction = std::mem::zeroed();
        new.sa_sigaction = handler as extern "C" fn(libc::c_int) as libc::sighandler_t;
        new.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut new.sa_mask);
        if libc::sigaction(libc::SIGINT, &new, &mut old) != 0 {
            return None;
        }
        Some(old)
    }
}

fn state() -> std::sync::MutexGuard<'static, Installed> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// For a batch run (put, restore, undo, rm, status --fix): the first
/// Ctrl-C stops it after the item in hand, and a second one quits.
/// Stays for the process.
pub fn stop_then_quit() {
    let mut st = state();
    STOP_FIRST.store(true, SeqCst);
    if st.saved.is_none() {
        st.saved = install();
    }
    st.by_section = false;
}

/// Whether Ctrl-C was pressed: a batch run stops before its next item.
pub fn requested() -> bool {
    PRESSES.load(SeqCst) > 0
}

/// An error once Ctrl-C was pressed inside [`abortable`], for a long copy
/// to check between files and chunks.
pub fn check() -> io::Result<()> {
    if ABORTABLE.load(SeqCst) > 0 && requested() {
        Err(io::Error::other("stopped by Ctrl-C"))
    } else {
        Ok(())
    }
}

/// Run `f`, a copy whose error path removes what it copied, so that
/// [`check`] can end it at the first Ctrl-C. A copy that puts a payload
/// back after a failure runs outside, so it always finishes.
pub fn abortable<T>(f: impl FnOnce() -> T) -> T {
    struct Scope;
    impl Drop for Scope {
        fn drop(&mut self) {
            ABORTABLE.fetch_sub(1, SeqCst);
        }
    }
    ABORTABLE.fetch_add(1, SeqCst);
    let _scope = Scope;
    f()
}

/// A section that must not end halfway; see the module notes.
#[must_use = "the section ends when this is dropped"]
pub struct Critical(());

/// Open a [`Critical`] section until the value is dropped.
pub fn critical() -> Critical {
    let mut st = state();
    if CRITICAL.fetch_add(1, SeqCst) == 0 && st.saved.is_none() {
        st.saved = install();
        st.by_section = st.saved.is_some();
    }
    Critical(())
}

impl Drop for Critical {
    fn drop(&mut self) {
        let mut st = state();
        if CRITICAL.fetch_sub(1, SeqCst) != 1 {
            return;
        }
        let stop_first = STOP_FIRST.load(SeqCst);
        if DEFERRED.swap(false, SeqCst) && quits_at_section_end(PRESSES.load(SeqCst), stop_first) {
            quit();
        }
        if st.by_section {
            if let Some(old) = st.saved.take() {
                // SAFETY: puts back the disposition `install` replaced.
                unsafe {
                    libc::sigaction(libc::SIGINT, &old, std::ptr::null_mut());
                }
            }
            st.by_section = false;
        }
    }
}

/// Test seam, debug builds only: `RTRASH_TEST_SIGINT=PLACE:COUNT` raises
/// SIGINT `COUNT` times the first time the run reaches `place`
/// (`after-item` in a put or restore loop, `in-record` once a put has
/// taken its trash name and before it writes the record).
pub fn test_seam(place: &str) {
    static FIRED: AtomicBool = AtomicBool::new(false);
    if !cfg!(debug_assertions) {
        return;
    }
    let Ok(v) = std::env::var("RTRASH_TEST_SIGINT") else {
        return;
    };
    let Some((at, count)) = v.split_once(':') else {
        return;
    };
    if at != place || FIRED.swap(true, SeqCst) {
        return;
    }
    for _ in 0..count.parse::<u32>().unwrap_or(0) {
        // SAFETY: raise(3) runs the installed handler on this thread.
        unsafe {
            libc::raise(libc::SIGINT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_first_press_stops_a_batch_and_a_second_quits() {
        assert_eq!(on_press(1, false, true), Press::Stop);
        assert_eq!(on_press(2, false, true), Press::Quit);
        assert_eq!(on_press(3, false, true), Press::Quit);
        // Without a batch run, Ctrl-C keeps its meaning.
        assert_eq!(on_press(1, false, false), Press::Quit);
    }

    #[test]
    fn a_press_inside_a_record_waits_for_its_end() {
        for (n, stop_first) in [(1, true), (2, true), (1, false), (5, false)] {
            assert_eq!(
                on_press(n, true, stop_first),
                Press::Defer,
                "{n} {stop_first}"
            );
        }
        // At the end: a batch's first press only stops it; any other quits.
        assert!(!quits_at_section_end(1, true));
        assert!(quits_at_section_end(2, true));
        assert!(quits_at_section_end(1, false));
    }
}
