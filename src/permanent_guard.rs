//! The permanent-deletion guard for shared machines and agents.
//!
//! `RTRASH_NO_PERMANENT=1` makes rtrash refuse to delete anything for
//! good. Moving files to the trash, listing and restoring still work, and
//! so do dry runs, so an agent sandbox can keep trash as its only way to
//! remove files while the trash itself stays recoverable for everyone who
//! shares it.
//!
//! Two layers enforce it. [`refuse_command`] stops the commands whose job
//! is a permanent delete (rm, empty, reap `--permanent` and
//! `--purge-trash`, wsl rm and empty) before they start, with exit 2.
//! Under that, every removal of user data goes through [`gate`] (or
//! [`remove`], which calls it): emptying, rm, the size cap after a put,
//! reap's deletes, a restore that overwrites, and the Recycle Bin purges.
//!
//! `RTRASH_MAX_PERMANENT=N` (or `--max-count=N` on rm and empty) caps a
//! run at N permanent deletions. rm and empty count their matches first
//! and refuse the whole run, deleting nothing, when there are more
//! ([`check_count`]); the gate counts every removal it lets through and
//! refuses the rest once a run reaches the cap, which covers the paths
//! that cannot count first (the size cap after a put, reap).
//! `scripts/precheck.py` fails a call to a removal primitive that names
//! neither this module nor a `// scratch:` reason, so a new delete path
//! cannot skip the gate unnoticed.

use std::ffi::OsStr;
use std::io;
#[cfg(unix)]
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// The variable that turns the guard on.
pub const NO_PERMANENT: &str = "RTRASH_NO_PERMANENT";

/// The variable that caps a run's permanent deletions.
pub const MAX_PERMANENT: &str = "RTRASH_MAX_PERMANENT";

/// `--max-count=N`, or `u64::MAX` when not given.
static FLAG_CAP: AtomicU64 = AtomicU64::new(u64::MAX);

/// Permanent deletions the gate has let through in this run.
static USED: AtomicU64 = AtomicU64::new(0);

/// A cap as written: a whole number of items.
pub fn parse_cap(v: &str) -> Result<u64, String> {
    v.trim()
        .parse::<u64>()
        .map_err(|_| format!("'{v}' is not a whole number of items"))
}

/// The value of a `--max-count=N` argument; None when `a` is another one.
pub fn max_count_arg(a: &str) -> Option<Result<u64, String>> {
    a.strip_prefix("--max-count=")
        .map(|v| parse_cap(v).map_err(|e| format!("--max-count: {e}")))
}

/// Take `--max-count=N` for the rest of this run.
pub fn set_max_count(n: u64) {
    FLAG_CAP.store(n, Ordering::Relaxed);
}

/// The cap from `value` of [`MAX_PERMANENT`]: None when unset or empty,
/// and 0 (nothing may go) when it is not a number.
pub fn cap_from(value: Option<&OsStr>) -> Option<u64> {
    let v = value?.to_str().unwrap_or("x");
    if v.is_empty() {
        return None;
    }
    Some(parse_cap(v).unwrap_or(0))
}

/// The cap on this run's permanent deletions: `--max-count`, else
/// [`MAX_PERMANENT`].
pub fn cap() -> Option<u64> {
    match FLAG_CAP.load(Ordering::Relaxed) {
        u64::MAX => cap_from(std::env::var_os(MAX_PERMANENT).as_deref()),
        n => Some(n),
    }
}

/// The exit code when deleting `n` more items for good would pass the
/// cap, after saying so; None when they fit. Call it before the first
/// removal, so a refused run deletes nothing.
pub fn check_count(prog: &str, n: u64) -> Option<i32> {
    let cap = cap()?;
    if USED.load(Ordering::Relaxed).saturating_add(n) <= cap {
        return None;
    }
    eprintln!(
        "{prog}: {} would be deleted for good, more than the cap of {cap} \
         (--max-count or {MAX_PERMANENT}); nothing was deleted, and -n lists them",
        crate::util::plural(n, "item", "items")
    );
    Some(2)
}

/// What a refused removal says.
pub const REFUSED: &str = "RTRASH_NO_PERMANENT is set, so permanent deletion is refused";

/// Whether `value` of [`NO_PERMANENT`] turns the guard on: set, not
/// empty and not `0`.
pub fn guard_on(value: Option<&OsStr>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0")
}

/// Whether the guard is on in this process.
pub fn blocked() -> bool {
    guard_on(std::env::var_os(NO_PERMANENT).as_deref())
}

/// The gate every permanent removal of user data passes: an error that
/// names [`NO_PERMANENT`] when the guard is on, or [`MAX_PERMANENT`] when
/// the removal would pass the cap.
pub fn gate() -> io::Result<()> {
    gate_many(1)
}

/// [`gate`] for one removal that takes `n` items at once, such as a
/// Recycle Bin purge or a whole trash wiped: all of them fit, or none go.
pub fn gate_many(n: u64) -> io::Result<()> {
    if blocked() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, REFUSED));
    }
    let Some(cap) = cap() else {
        return Ok(());
    };
    let used = USED.fetch_add(n, Ordering::Relaxed);
    if used.saturating_add(n) > cap {
        USED.fetch_sub(n, Ordering::Relaxed);
        let msg =
            format!("{MAX_PERMANENT} or --max-count caps this run at {cap} permanent deletions");
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, msg));
    }
    Ok(())
}

/// Remove `path` (a file, link or whole tree) for good, through [`gate`].
#[cfg(unix)]
pub fn remove(path: &Path) -> io::Result<()> {
    gate()?;
    crate::trashdir::remove_any_path(path)
}

/// Whether `args` only look and delete nothing: a dry run, or help or
/// version.
pub fn only_looks(args: &[String]) -> bool {
    args.iter().any(|a| {
        matches!(
            a.as_str(),
            "-n" | "--dry-run" | "-h" | "--help" | "-V" | "--version"
        )
    })
}

/// Whether subcommand `cmd` with `args` deletes for good as its job.
pub fn deletes_for_good(cmd: &str, args: &[String]) -> bool {
    match cmd {
        "rm" | "empty" => true,
        "reap" => args
            .iter()
            .any(|a| a == "--permanent" || a == "--purge-trash"),
        "wsl" => matches!(args.first().map(String::as_str), Some("rm" | "empty")),
        _ => false,
    }
}

/// The exit code when the guard refuses `args` for `prog`, after saying
/// why; None when the command may run.
pub fn refuse_permanent(prog: &str, args: &[String]) -> Option<i32> {
    if !blocked() || only_looks(args) {
        return None;
    }
    eprintln!("{prog}: {REFUSED}; -n shows what would go, and put, list and restore still work");
    Some(2)
}

/// [`refuse_permanent`] for a whole command line: `argv0` is the program
/// name (`trash-rm`, `rtrash`) and `rest` the arguments after it.
pub fn refuse_command(argv0: &str, rest: &[String]) -> Option<i32> {
    let (prog, cmd, args) = match argv0 {
        "trash-rm" => (argv0.to_string(), "rm", rest),
        "trash-empty" => (argv0.to_string(), "empty", rest),
        "rm" | "trash" => return None,
        _ if argv0.starts_with("trash-") => return None,
        _ => {
            let cmd = rest.first()?;
            (format!("{argv0} {cmd}"), cmd.as_str(), &rest[1..])
        }
    };
    if !deletes_for_good(cmd, args) {
        return None;
    }
    if let Some(v) = std::env::var_os(MAX_PERMANENT).filter(|v| !v.is_empty()) {
        let v = v.to_string_lossy();
        if let Err(e) = parse_cap(&v) {
            eprintln!("{prog}: {MAX_PERMANENT}: {e}");
            return Some(2);
        }
    }
    refuse_permanent(&prog, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_guard_reads_set_non_zero_values() {
        assert!(!guard_on(None));
        assert!(!guard_on(Some(OsStr::new(""))));
        assert!(!guard_on(Some(OsStr::new("0"))));
        assert!(guard_on(Some(OsStr::new("1"))));
        assert!(guard_on(Some(OsStr::new("yes"))));
    }

    #[test]
    fn dry_runs_and_help_pass_the_guard() {
        assert!(only_looks(&args(&["-n", "*.log"])));
        assert!(only_looks(&args(&["--all", "--dry-run"])));
        assert!(only_looks(&args(&["--help"])));
        assert!(!only_looks(&args(&["-f", "*"])));
        assert!(!only_looks(&args(&["--all"])));
    }

    #[test]
    fn the_cap_reads_whole_numbers_and_fails_closed() {
        assert_eq!(cap_from(None), None);
        assert_eq!(cap_from(Some(OsStr::new(""))), None);
        assert_eq!(cap_from(Some(OsStr::new("3"))), Some(3));
        assert_eq!(cap_from(Some(OsStr::new(" 12 "))), Some(12));
        assert_eq!(cap_from(Some(OsStr::new("lots"))), Some(0));
        assert_eq!(cap_from(Some(OsStr::new("-1"))), Some(0));
        assert_eq!(max_count_arg("--max-count=5"), Some(Ok(5)));
        assert!(matches!(max_count_arg("--max-count=x"), Some(Err(_))));
        assert_eq!(max_count_arg("--max-size=5"), None);
    }

    #[test]
    fn the_commands_that_delete_for_good_are_named() {
        assert!(deletes_for_good("rm", &args(&["x"])));
        assert!(deletes_for_good("empty", &args(&["--all"])));
        assert!(deletes_for_good("reap", &args(&["--permanent", "/tmp"])));
        assert!(deletes_for_good(
            "reap",
            &args(&["--when-below=10%", "--purge-trash"])
        ));
        assert!(!deletes_for_good("reap", &args(&["/tmp"])));
        assert!(deletes_for_good("wsl", &args(&["empty", "--all"])));
        assert!(deletes_for_good("wsl", &args(&["rm", "x"])));
        assert!(!deletes_for_good("wsl", &args(&["list"])));
        assert!(!deletes_for_good("put", &args(&["x"])));
        assert!(!deletes_for_good("restore", &args(&["x"])));
    }
}
