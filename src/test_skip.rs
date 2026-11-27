//! The skip note shared by integration tests and unit tests.
//!
//! `cargo test` and nextest both hide the stderr of a passing test, and CI
//! prints the file `RTRASH_SKIP_LOG` names after the suite. [`skip`] writes
//! that file. Integration tests call it through `tests/common::skip`.

use std::io::Write;

/// Print `why` on stderr and, when `RTRASH_SKIP_LOG` names a file, append
/// one line: the test binary, the thread name (libtest names the thread
/// after the test) and `why`. A failed write prints its error on stderr;
/// the test goes on.
pub fn skip(why: &str) {
    eprintln!("{why}");
    let Some(log) = std::env::var_os("RTRASH_SKIP_LOG") else {
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
        let shown = std::path::Path::new(&log).display().to_string();
        eprintln!("RTRASH_SKIP_LOG {shown}: {e}");
    }
}
