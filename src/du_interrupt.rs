//! Ctrl-C during `rtrash du`: the first one stops the scan so the listing
//! shows what was counted; a second one ends the process as usual.

use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

use crate::du::ScanControl;

/// The scan the handler stops. Set once per process, before the handler
/// is installed, so the handler only reads it.
static TARGET: OnceLock<Arc<ScanControl>> = OnceLock::new();

/// Route Ctrl-C to `control`. Returns false when another scan holds it.
pub fn install(control: Arc<ScanControl>) -> bool {
    if TARGET.set(control).is_err() {
        return false;
    }
    os::install();
    true
}

/// Give Ctrl-C back its default meaning.
pub fn uninstall() {
    if TARGET.get().is_some() {
        os::uninstall();
    }
}

/// Whether Ctrl-C has been pressed since [`install`].
pub fn pressed() -> bool {
    TARGET.get().is_some_and(|c| c.stopped())
}

fn on_interrupt() {
    if let Some(c) = TARGET.get() {
        c.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(unix)]
mod os {
    extern "C" fn handler(_: libc::c_int) {
        super::on_interrupt();
        // The next Ctrl-C ends the process.
        // SAFETY: signal(2) is async-signal-safe.
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
        }
    }

    pub fn install() {
        // SAFETY: the handler only stores to an atomic and calls signal(2).
        unsafe {
            libc::signal(
                libc::SIGINT,
                handler as extern "C" fn(libc::c_int) as libc::sighandler_t,
            );
        }
    }

    pub fn uninstall() {
        // SAFETY: restoring the default disposition.
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
        }
    }
}

#[cfg(windows)]
mod os {
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows_sys::Win32::Foundation::{BOOL, FALSE, TRUE};
    use windows_sys::Win32::System::Console::{SetConsoleCtrlHandler, CTRL_C_EVENT};

    static FIRED: AtomicBool = AtomicBool::new(false);

    unsafe extern "system" fn handler(kind: u32) -> BOOL {
        // The first Ctrl-C stops the scan; the next one, or Ctrl-Break,
        // takes the default action and ends the process.
        if kind == CTRL_C_EVENT && !FIRED.swap(true, Ordering::Relaxed) {
            super::on_interrupt();
            return TRUE;
        }
        FALSE
    }

    pub fn install() {
        // SAFETY: registers a handler that only touches atomics.
        unsafe {
            SetConsoleCtrlHandler(Some(handler), TRUE);
        }
    }

    pub fn uninstall() {
        // SAFETY: removes the handler registered above.
        unsafe {
            SetConsoleCtrlHandler(Some(handler), FALSE);
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod os {
    pub fn install() {}
    pub fn uninstall() {}
}
