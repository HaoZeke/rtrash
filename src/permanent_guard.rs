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
//! `scripts/precheck.py` fails a call to a removal primitive that names
//! neither this module nor a `// scratch:` reason, so a new delete path
//! cannot skip the gate unnoticed.

use std::ffi::OsStr;
use std::io;
#[cfg(unix)]
use std::path::Path;

/// The variable that turns the guard on.
pub const NO_PERMANENT: &str = "RTRASH_NO_PERMANENT";

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
/// names [`NO_PERMANENT`] when the guard is on.
pub fn gate() -> io::Result<()> {
    if blocked() {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, REFUSED))
    } else {
        Ok(())
    }
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
