//! `RTRASH_ALLOW_ROOTS`: a scope for agents and sandboxes.
//!
//! The variable holds a path list (`:` between entries on Unix, `;` on
//! Windows, as `PATH` does). While it is set, put only trashes paths under
//! one of those roots, restore and undo only write under them, and rm only
//! deletes trashed items whose original path was under them. empty, reap
//! and wsl rm and empty, which cannot be scoped, refuse outright. Each
//! command checks every path it would touch first and refuses the whole
//! run with exit 2 before it changes anything; the primitives check again,
//! so a path that skips the first check still stops.
//!
//! Both sides are compared on disk: each root is canonicalized, and so is
//! the nearest existing ancestor of each path, which resolves symlinked
//! directories (a link inside a root that points outside does not widen
//! it) and, on Windows, 8.3 short names such as `RUNNER~1`. Windows paths
//! compare without case. The last part of a path stays as named, since put
//! and restore act on a link itself rather than on its target. A root that
//! does not resolve is dropped, so a scope whose roots are all gone allows
//! nothing, and a path that climbs with `..` through a part that does not
//! exist yet is refused.

use std::io;
use std::path::{Path, PathBuf};

/// The variable that sets the scope.
pub const ALLOW_ROOTS: &str = "RTRASH_ALLOW_ROOTS";

/// The roots, canonical; None when the variable is unset or empty.
pub fn roots() -> Option<Vec<PathBuf>> {
    let v = std::env::var_os(ALLOW_ROOTS)?;
    if v.is_empty() {
        return None;
    }
    Some(
        std::env::split_paths(&v)
            .filter(|p| !p.as_os_str().is_empty())
            .filter_map(|p| std::fs::canonicalize(p).ok())
            .map(|p| crate::util::plain_path(&p))
            .collect(),
    )
}

/// Where `p` sits on disk: its nearest existing ancestor canonicalized,
/// with the parts below it appended as named, and the last part never
/// resolved. None when a part that does not exist yet is `..`, or when
/// nothing of the path resolves.
pub fn resolve(p: &Path) -> Option<PathBuf> {
    let abs = std::path::absolute(p).ok()?;
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut base = abs.as_path();
    // The entry itself stays as named: a link names its own place.
    if let (Some(name), Some(parent)) = (base.file_name(), base.parent()) {
        tail.push(name.to_os_string());
        base = parent;
    }
    while base.symlink_metadata().is_err() {
        let (Some(name), Some(parent)) = (base.file_name(), base.parent()) else {
            return None;
        };
        tail.push(name.to_os_string());
        base = parent;
    }
    let mut out = crate::util::plain_path(&std::fs::canonicalize(base).ok()?);
    for part in tail.iter().rev() {
        out.push(part);
    }
    Some(out)
}

/// Whether resolved `p` is `root` or below it. Windows compares without
/// case and with either slash.
pub fn under(p: &Path, root: &Path) -> bool {
    if cfg!(windows) {
        let key = |x: &Path| {
            x.to_string_lossy()
                .replace('/', "\\")
                .trim_end_matches('\\')
                .to_lowercase()
        };
        let (p, r) = (key(p), key(root));
        p == r || p.starts_with(&format!("{r}\\"))
    } else {
        p.starts_with(root)
    }
}

/// Whether the scope lets a command touch `p`: always when no scope is
/// set.
pub fn allowed(p: &Path) -> bool {
    match roots() {
        None => true,
        Some(rs) => allowed_in(&rs, p),
    }
}

/// Whether `p` sits under one of `roots` (canonical, as [`roots`] gives
/// them). An empty list allows nothing.
pub fn allowed_in(roots: &[PathBuf], p: &Path) -> bool {
    resolve(p).is_some_and(|q| roots.iter().any(|r| under(&q, r)))
}

/// The one way rtrash hands a path to the OS trash (the `trash` crate's
/// `delete`): it checks the scope first, so no caller can skip it. The
/// precheck refuses any other call of `trash::delete`. A debug build
/// under the tests' bin seam moves the path into that folder instead, so
/// a test never fills the user's Recycle Bin.
#[cfg(windows)]
pub fn trash_delete(p: &Path) -> io::Result<()> {
    check(p)?;
    if let Some(dir) = crate::win_recycle::test_bin_dir() {
        return crate::win_recycle::test_bin_put(&dir, p);
    }
    trash::delete(p).map_err(|e| io::Error::other(e.to_string()))
}

/// The error a primitive returns for a path outside the scope.
pub fn check(p: &Path) -> io::Result<()> {
    if allowed(p) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("'{}' is outside {ALLOW_ROOTS}", p.display()),
        ))
    }
}

/// The exit code when any of `paths` lies outside the scope, after naming
/// each one; None when all are inside. Call it before the first change.
pub fn refuse_outside(prog: &str, paths: &[PathBuf]) -> Option<i32> {
    roots()?;
    let outside: Vec<&PathBuf> = paths.iter().filter(|p| !allowed(p)).collect();
    if outside.is_empty() {
        return None;
    }
    for p in &outside {
        eprintln!(
            "{prog}: '{}' is outside {ALLOW_ROOTS}",
            crate::util::err_path(p)
        );
    }
    eprintln!("{prog}: nothing was changed");
    Some(2)
}

/// [`refuse_outside`] for items whose origin may be unknown: each pair is
/// the path to name and the recorded origin. An item with no origin
/// fails closed, as outside, whatever folder holds it.
pub fn refuse_outside_or_unplaced(prog: &str, items: &[(PathBuf, Option<PathBuf>)]) -> Option<i32> {
    roots()?;
    let mut refused = false;
    for (shown, origin) in items {
        match origin {
            Some(o) if allowed(o) => {}
            Some(o) => {
                refused = true;
                eprintln!(
                    "{prog}: '{}' is outside {ALLOW_ROOTS}",
                    crate::util::err_path(o)
                );
            }
            None => {
                refused = true;
                eprintln!(
                    "{prog}: '{}' has no recorded origin, so it is outside {ALLOW_ROOTS}",
                    crate::util::err_path(shown)
                );
            }
        }
    }
    if !refused {
        return None;
    }
    eprintln!("{prog}: nothing was changed");
    Some(2)
}

/// The exit code for a command that cannot be scoped while the scope is
/// set: `empty`, `reap`, and wsl `rm` and `empty`. Dry runs and help pass,
/// and so does `empty --dangling`, which checks each record's origin
/// against the roots itself, as `status --check --fix` does.
pub fn refuse_unscoped(prog: &str, cmd: &str, args: &[String]) -> Option<i32> {
    let unscoped = match cmd {
        "empty" => !args.iter().any(|a| a == "--dangling"),
        "reap" => true,
        "wsl" => matches!(args.first().map(String::as_str), Some("rm" | "empty")),
        _ => false,
    };
    if !unscoped || roots().is_none() || crate::permanent_guard::only_looks(args) {
        return None;
    }
    eprintln!(
        "{prog}: {ALLOW_ROOTS} is set, and this command cannot keep to it; \
         rm takes a pattern and keeps to the roots"
    );
    Some(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_compare_below_their_roots_only() {
        let (root, out) = if cfg!(windows) {
            (Path::new(r"C:\Work"), Path::new(r"C:\Workshop\x"))
        } else {
            (Path::new("/work"), Path::new("/workshop/x"))
        };
        assert!(under(root, root));
        assert!(under(&root.join("a").join("b"), root));
        assert!(!under(out, root));
        if cfg!(windows) {
            assert!(under(Path::new(r"c:/work/A.TXT"), root));
            assert!(under(Path::new(r"C:\x"), Path::new(r"C:\")));
        }
    }

    #[test]
    fn verbatim_prefixes_drop_on_windows() {
        let plain = crate::util::plain_path;
        let p = plain(Path::new(r"\\?\C:\Users\me"));
        if cfg!(windows) {
            assert_eq!(p, PathBuf::from(r"C:\Users\me"));
            assert_eq!(
                plain(Path::new(r"\\?\UNC\srv\share")),
                PathBuf::from(r"\\srv\share")
            );
            // A volume GUID path has no plain form and stays whole.
            assert_eq!(
                plain(Path::new(r"\\?\Volume{1}\x")),
                PathBuf::from(r"\\?\Volume{1}\x")
            );
        } else {
            assert_eq!(p, PathBuf::from(r"\\?\C:\Users\me"));
        }
    }

    #[test]
    fn a_missing_part_that_climbs_is_refused() {
        let base = std::env::temp_dir();
        let gone = base.join("rtrash-allow-roots-no-such-dir");
        // Windows reads `..` before it looks at the disk, as the shell does.
        if cfg!(unix) {
            assert_eq!(resolve(&gone.join("..").join("x")), None);
        }
        let kept = resolve(&gone.join("x")).unwrap();
        assert!(kept.ends_with(Path::new("rtrash-allow-roots-no-such-dir").join("x")));
    }
}
