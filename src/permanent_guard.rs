//! The permanent-deletion guard for shared machines and agents.
//!
//! `RTRASH_NO_PERMANENT=1` makes `rm` and `empty` (and `trash-rm`,
//! `trash-empty`) refuse to delete anything for good. Moving files to the
//! trash, listing and restoring still work, and so do dry runs, so an
//! agent sandbox can keep trash as its only way to remove files while the
//! trash itself stays recoverable for everyone who shares it.

use std::ffi::OsStr;

/// The variable that turns the guard on.
pub const NO_PERMANENT: &str = "RTRASH_NO_PERMANENT";

/// Whether `value` of [`NO_PERMANENT`] turns the guard on: set, not
/// empty and not `0`.
pub fn guard_on(value: Option<&OsStr>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0")
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

/// The exit code when the guard refuses `args` for `prog`, after saying
/// why; None when the command may run.
pub fn refuse_permanent(prog: &str, args: &[String]) -> Option<i32> {
    let on = guard_on(std::env::var_os(NO_PERMANENT).as_deref());
    if !on || only_looks(args) {
        return None;
    }
    eprintln!(
        "{prog}: {NO_PERMANENT} is set, so permanent deletion is refused; \
         -n shows what would go, and put, list and restore still work"
    );
    Some(2)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(only_looks(&args(&["-n", "*.log"])));
        assert!(only_looks(&args(&["--all", "--dry-run"])));
        assert!(only_looks(&args(&["--help"])));
        assert!(!only_looks(&args(&["-f", "*"])));
        assert!(!only_looks(&args(&["--all"])));
    }
}
