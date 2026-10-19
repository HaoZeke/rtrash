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
//! real bin that another test's du summary read. On Windows it also pins
//! `LOCALAPPDATA`, where the last-put record and the bin history live:
//! round 267 went red when a parallel test's put rewrote the shared
//! last-put record between another test's put and its `empty`.
//!
//! Every test that compares two `du --json` outputs compares them through
//! `du_scan`, which cuts off the live trash summary: it counts the trash
//! as it is at that moment, not the scan.
//!
//! Every test that waits for a run to ask on stderr runs it through
//! `Asking`. A question that never comes then fails the test after
//! `PROMPT_WAIT` and names what the run wrote, where a blocking read
//! would hang the whole binary until CI kills it.
//!
//! Every test that skips itself, or a case, says why through `skip`.
//! Both `cargo test` and `cargo nextest` hide the stderr of a passing
//! test, so an `eprintln!` alone never reaches a CI log. Round 276 could
//! not tell from the Windows and macOS logs whether the rsync half ran.
//! `skip` also adds the note to the file `RTRASH_SKIP_LOG` names, which
//! CI prints after the tests.
#![allow(dead_code)]

use std::fmt::Debug;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

/// Print why a test, or a case of one, is skipped, on stderr. When
/// `RTRASH_SKIP_LOG` names a file, also append one line to it: the test
/// binary, the thread name (libtest names the thread after the test)
/// and `why`. A failed write prints its error on stderr; the test goes
/// on.
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
    let wrote = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .and_then(|mut f| f.write_all(line.as_bytes()));
    if let Err(e) = wrote {
        eprintln!("RTRASH_SKIP_LOG {}: {e}", Path::new(&log).display());
    }
}

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
/// du trash summary use that folder, not the system Recycle Bin),
/// `LOCALAPPDATA` is [`local_app_data`]`(home)` on Windows, and the
/// guard and config variables are cleared. Set anything else on the
/// result; a later `.env` wins.
pub fn rtrash(home: &Path) -> Command {
    let mut c = Command::new(bin());
    if cfg!(windows) {
        c.env("LOCALAPPDATA", local_app_data(home));
    }
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

/// Make the junction `link` to `target` with `mklink /J`. cmd reads a `/`
/// anywhere in an argument as the start of a switch ("Invalid switch"),
/// so both paths must be native; a `/` in either fails here first. Every
/// mklink call in the tests goes through here (precheck
/// check_mklink_calls).
#[cfg(windows)]
pub fn mklink_junction(link: &Path, target: &Path) {
    for p in [link, target] {
        let plain = !p.as_os_str().to_string_lossy().contains('/');
        assert!(plain, "a mklink path with '/': {}", p.display());
    }
    let out = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "mklink /J {}: {err}", link.display());
}

/// The `LOCALAPPDATA` that [`rtrash`] gives a Windows run under `home`.
/// The last-put record that `empty` and `restore --last` read lives
/// there, so a parallel test's put cannot replace it.
pub fn local_app_data(home: &Path) -> PathBuf {
    home.join("localappdata")
}

/// The scratch Recycle Bin [`rtrash`] gives a Windows run under `home`.
/// It is named like the real one, so the du duplicate walk skips it as
/// it skips a real bin.
pub fn test_bin(home: &Path) -> PathBuf {
    home.join("$Recycle.Bin")
}

/// A `du --json` output with the live trash summary (the last field,
/// `"trash"`) cut off, for comparing the scans of two runs: the summary
/// counts the trash as it is at that moment, which a parallel test can
/// change. Output with no summary, such as `--print0`, comes back whole.
pub fn du_scan(stdout: impl AsRef<[u8]>) -> String {
    let s = String::from_utf8_lossy(stdout.as_ref());
    match s.rfind(",\"trash\":") {
        Some(at) => s[..at].to_string(),
        None => s.into_owned(),
    }
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

/// How long [`Asking`] waits for a question, or for the run to end after
/// the answer. Far above any healthy run on a loaded runner; it only
/// turns a hang into a failure.
pub const PROMPT_WAIT: Duration = Duration::from_secs(60);

/// A run that asks on stderr. The test waits for the question, acts while
/// the run waits on it, then answers. A thread reads stderr, so a wait
/// can give up after [`PROMPT_WAIT`]; the run is then killed and the test
/// fails with what the run wrote.
pub struct Asking {
    child: Child,
    chunks: Receiver<Vec<u8>>,
    err: Vec<u8>,
}

impl Asking {
    /// Spawn `cmd` with stdin and stderr piped and stdout discarded.
    pub fn spawn(cmd: &mut Command) -> Asking {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn rtrash");
        let mut err = child.stderr.take().expect("stderr");
        let (tx, chunks) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match err.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        });
        Asking {
            child,
            chunks,
            err: Vec::new(),
        }
    }

    /// Wait until stderr ends with `question`. The test fails when the run
    /// ends first or [`PROMPT_WAIT`] passes.
    pub fn wait_for(&mut self, question: &str) {
        let deadline = Instant::now() + PROMPT_WAIT;
        while !self.err.ends_with(question.as_bytes()) {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.chunks.recv_timeout(left) {
                Ok(chunk) => self.err.extend_from_slice(&chunk),
                Err(RecvTimeoutError::Timeout) => self.fail(&format!("no {question:?} in time")),
                Err(RecvTimeoutError::Disconnected) => {
                    self.fail(&format!("the run ended before {question:?}"))
                }
            }
        }
    }

    /// Write `line` to stdin, close it and wait for the run to end: its
    /// exit status and all it wrote to stderr. The test fails when the
    /// run is still going after [`PROMPT_WAIT`].
    pub fn answer(mut self, line: &str) -> (ExitStatus, String) {
        if let Some(mut stdin) = self.child.stdin.take() {
            let _ = stdin.write_all(line.as_bytes());
        }
        let deadline = Instant::now() + PROMPT_WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.chunks.recv_timeout(left) {
                Ok(chunk) => self.err.extend_from_slice(&chunk),
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => self.fail("the run did not end after the answer"),
            }
        }
        let status = self.child.wait().expect("wait for rtrash");
        (status, String::from_utf8_lossy(&self.err).into_owned())
    }

    fn fail(&mut self, why: &str) -> ! {
        let _ = self.child.kill();
        let _ = self.child.wait();
        panic!(
            "{why}; stderr so far: {}",
            String::from_utf8_lossy(&self.err)
        );
    }
}

impl Drop for Asking {
    /// A test that fails between the question and the answer leaves no
    /// run behind.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
