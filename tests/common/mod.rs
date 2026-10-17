//! Scaffolding shared by the integration test binaries.
//!
//! Every test that needs a scratch directory takes it from `scratch_dir`.
//! Tests in one binary run in parallel with the same pid, so a path built
//! from the pid and a hand-picked tag lets two tests share, and delete,
//! one tree (round 149 reused the du tag "types"). The counter here makes
//! each call unique whatever tag it gets.
//!
//! Every test that needs two trash puts to get different deletion times
//! calls `next_trash_second` between them, in place of a fixed sleep or a
//! loop of its own on the system clock; it waits on the file system's
//! clock too, which trails the system clock.
//!
//! Every test that expects a command to refuse and change nothing calls
//! `assert_refused_unchanged`. It takes the snapshot itself, just before
//! the command runs, so a stale baseline cannot hide a change (round
//! 181c compared against a listing taken several calls earlier).
//!
//! Every test that compares a du table row, against a literal or a row
//! from another run, takes it from `du_row`, which writes the AGE cell
//! as `AGE`. A second can tick between two runs (round 218 failed when
//! `0s` became `1s`).
//!
//! Every test that runs rtrash builds the command with `rtrash(home)`,
//! which pins the home, data and config folders to `home`, keeps macOS
//! off the Finder Trash, points Windows at a scratch Recycle Bin under
//! `home`, and clears the guard variables a developer may have set. A
//! test that must reach the real trash calls `rtrash_real` and names why.
//! Round 231 went red on Windows when a parallel test's put filled the
//! real bin that another test's du summary read.
#![allow(dead_code)]

use std::fmt::Debug;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A fresh, empty directory under the system temp dir, named
/// `rtrash-{prefix}-{pid}-{n}-{tag}`, unique to this call.
pub fn scratch_dir(prefix: &str, tag: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let name = format!("rtrash-{prefix}-{}-{n}-{tag}", std::process::id());
    let root = std::env::temp_dir().join(name);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// Whole seconds since the Unix epoch on the system clock.
pub fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Whole seconds on the clock the file system stamps a change with: the
/// modification time of a probe file written just now.
///
/// That clock trails the system clock. Linux stamps files from its coarse
/// clock, one tick (a few ms) behind, so a file written just after the
/// system clock reaches a second can still carry the second before; on
/// the box every one of 40 such writes did, by up to 5 ms. Windows stamps
/// files from its coarse system time as well.
pub fn fs_clock_secs() -> u64 {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let probe = std::env::temp_dir().join(format!("rtrash-clock-{}-{n}", std::process::id()));
    fs::write(&probe, b"x").unwrap();
    let modified = fs::metadata(&probe).and_then(|m| m.modified());
    let _ = fs::remove_file(&probe);
    modified
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Wait until both the system clock and the file system's clock reach a
/// later whole second, so a trash put after this call gets a later
/// deletion time than any put before it.
///
/// The Recycle Bin, the Finder Trash and a trashinfo `DeletionDate` all
/// keep whole seconds. Two puts in one second tie, and the order then
/// falls back to the name, which a test that sorts by date does not
/// expect (commit 9708b71). A trashinfo date comes from the system
/// clock, but a Finder item's date is its change time in the trash,
/// stamped by the file system's clock (see [`fs_clock_secs`]). Waiting on
/// the system clock alone let a put just after the tick land in the
/// second before, tied with the previous put
/// (`list_last_keeps_the_newest_finder_items`, round 208). The file
/// system's clock never runs ahead of the system clock, so once it has
/// passed `start`, both have. It returns the file system's later second.
pub fn next_trash_second() -> u64 {
    wait_for_later_second(epoch_secs(), fs_clock_secs, || {
        std::thread::sleep(Duration::from_millis(20))
    })
}

/// Read `clock` until it shows a second after `start` and return that
/// reading, calling `nap` between readings and not after the last one.
/// It counts readings rather than wall time, so a loaded runner cannot
/// make it fail early. It panics after `SECOND_POLLS` readings instead
/// of spinning forever on a clock that never moves.
pub fn wait_for_later_second(
    start: u64,
    mut clock: impl FnMut() -> u64,
    mut nap: impl FnMut(),
) -> u64 {
    for _ in 0..SECOND_POLLS {
        let now = clock();
        if now > start {
            return now;
        }
        nap();
    }
    panic!("the clock stayed at second {start} for {SECOND_POLLS} readings");
}

/// Readings [`wait_for_later_second`] takes before it gives up: at 20 ms
/// a nap, 10 s or more of waiting on a tick that comes within a second.
pub const SECOND_POLLS: usize = 500;

/// The end of each stderr line naming a path outside `RTRASH_ALLOW_ROOTS`.
pub const OUTSIDE: &str = "is outside RTRASH_ALLOW_ROOTS";

/// The stderr of `out`, lossy.
pub fn stderr_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// How many stderr lines of `out` end with `suffix`.
pub fn lines_ending(out: &Output, suffix: &str) -> usize {
    stderr_text(out)
        .lines()
        .filter(|l| l.ends_with(suffix))
        .count()
}

/// How many stderr lines of `out` name a path outside the scope.
pub fn outside_lines(out: &Output) -> usize {
    lines_ending(out, OUTSIDE)
}

/// One path in a `state` snapshot.
#[derive(Debug, PartialEq)]
pub enum Node {
    Missing,
    Dir,
    File(Vec<u8>),
    Link(PathBuf),
    Other,
    Unreadable(String),
}

/// Every path under each of `watch` (a folder or a file), with its kind
/// and, for files, its bytes; a symlink keeps its target and is not
/// followed. Sorted, so two snapshots compare equal when nothing changed.
pub fn state(watch: &[&Path]) -> Vec<(PathBuf, Node)> {
    fn walk(p: &Path, out: &mut Vec<(PathBuf, Node)>) {
        let node = match fs::symlink_metadata(p) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Node::Missing,
            Err(e) => Node::Unreadable(e.kind().to_string()),
            Ok(m) if m.file_type().is_symlink() => Node::Link(fs::read_link(p).unwrap_or_default()),
            Ok(m) if m.is_file() => match fs::read(p) {
                Ok(b) => Node::File(b),
                Err(e) => Node::Unreadable(e.kind().to_string()),
            },
            Ok(m) if m.is_dir() => match fs::read_dir(p) {
                Ok(rd) => {
                    out.push((p.to_path_buf(), Node::Dir));
                    let mut kids: Vec<PathBuf> =
                        rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
                    kids.sort();
                    for k in kids {
                        walk(&k, out);
                    }
                    return;
                }
                Err(e) => Node::Unreadable(e.kind().to_string()),
            },
            Ok(_) => Node::Other,
        };
        out.push((p.to_path_buf(), node));
    }
    let mut out = Vec::new();
    for p in watch {
        walk(p, &mut out);
    }
    out
}

/// Snapshots `watch`, runs `run` right after, and asserts it exited with
/// `code`, that for each `(suffix, n)` in `lines` exactly `n` stderr lines
/// end with `suffix`, and that `watch` is the same as just before the
/// call. Returns the output for any further checks. `label` names the
/// call in failure messages.
pub fn assert_refused_unchanged(
    label: &dyn Debug,
    watch: &[&Path],
    code: i32,
    lines: &[(&str, usize)],
    run: impl FnOnce() -> Output,
) -> Output {
    let before = state(watch);
    let out = run();
    let err = stderr_text(&out);
    assert_eq!(out.status.code(), Some(code), "{label:?}: {err}");
    for (suffix, n) in lines {
        assert_eq!(
            lines_ending(&out, suffix),
            *n,
            "{label:?} {suffix:?}: {err}"
        );
    }
    assert_eq!(state(watch), before, "{label:?} changed {watch:?}: {err}");
    out
}

/// The rtrash binary under test.
pub fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rtrash")
}

/// The du table row of `text` that ends with `name`, its cells joined
/// by one space and the AGE cell (`45s`, `3m`, `2h`, `5d`, `1y`) written
/// as `AGE`, so that rows from two runs compare equal whatever second
/// each one ran in. Panics when no row ends with `name`.
pub fn du_row(text: &str, name: &str) -> String {
    let row = text
        .lines()
        .find(|l| l.ends_with(name))
        .unwrap_or_else(|| panic!("no row ends with {name:?} in:\n{text}"));
    let mut cells = String::new();
    for cell in row.split_whitespace() {
        let age = cell.len() > 1
            && cell.ends_with(|c: char| "smhdy".contains(c))
            && cell[..cell.len() - 1].bytes().all(|b| b.is_ascii_digit());
        if !cells.is_empty() {
            cells.push(' ');
        }
        cells.push_str(if age { "AGE" } else { cell });
    }
    cells
}

/// rtrash with every trash and config lookup pinned under `home`:
/// `HOME` and `USERPROFILE` are `home`, `XDG_DATA_HOME` is `home/xdg`
/// (so the home trash is `home/xdg/Trash`), `XDG_CONFIG_HOME` is
/// `home/.config`, `RTRASH_MACOS_TRASH` is `freedesktop` (plain commands
/// on macOS would otherwise go to the Finder Trash),
/// `RTRASH_TEST_BIN_DIR` is [`test_bin`]`(home)` (Windows puts and the
/// du trash summary use that folder, not the system Recycle Bin), and the
/// guard and config variables are cleared. Set anything else on the
/// result; a later `.env` wins.
pub fn rtrash(home: &Path) -> Command {
    let mut c = Command::new(bin());
    c.env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_DATA_HOME", home.join("xdg"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("RTRASH_MACOS_TRASH", "freedesktop")
        .env("RTRASH_TEST_BIN_DIR", test_bin(home))
        .env_remove("RTRASH_NO_PERMANENT")
        .env_remove("RTRASH_MAX_PERMANENT")
        .env_remove("RTRASH_ALLOW_ROOTS")
        .env_remove("RTRASH_CONFIG");
    // Sizes print in rtrash's own units unless a test sets one of these.
    for var in [
        "RTRASH_BLOCK_SIZE",
        "DU_BLOCK_SIZE",
        "BLOCK_SIZE",
        "BLOCKSIZE",
    ] {
        c.env_remove(var);
    }
    c
}

/// The scratch Recycle Bin [`rtrash`] gives a Windows run under `home`.
/// It is named like the real one, so the du duplicate walk skips it as
/// it skips a real bin.
pub fn test_bin(home: &Path) -> PathBuf {
    home.join("$Recycle.Bin")
}

/// Why a test runs rtrash without the `rtrash` pin.
#[derive(Debug, Clone, Copy)]
pub enum Real {
    /// The Finder Trash tests, which pick it with `RTRASH_MACOS_TRASH`.
    Finder,
    /// The Windows tests that need the system Recycle Bin.
    RecycleBin,
    /// Output that reads no trash and no config (help, man, completions).
    NoTrash,
}

/// rtrash with the caller's environment, for a test that must reach the
/// real trash or must see the environment as is; `why` names the reason.
pub fn rtrash_real(why: Real) -> Command {
    let _ = why;
    Command::new(bin())
}
