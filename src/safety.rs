//! Safety rails for put: safe-rm protected paths, the home directory and
//! mount roots, and a warning before trashing a repository with work
//! that exists nowhere else.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// safe-rm's built-in list: system directories nobody means to remove.
pub const SAFE_RM_DEFAULTS: &[&str] = &[
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/home",
    "/initrd",
    "/lib",
    "/lib32",
    "/lib64",
    "/proc",
    "/root",
    "/sbin",
    "/sys",
    "/usr",
    "/usr/bin",
    "/usr/include",
    "/usr/lib",
    "/usr/local",
    "/usr/local/bin",
    "/usr/local/include",
    "/usr/local/sbin",
    "/usr/local/share",
    "/usr/sbin",
    "/usr/share",
    "/usr/src",
    "/var",
];

/// Config files safe-rm reads, in its order: system-wide, then per user.
pub fn config_files() -> Vec<PathBuf> {
    let mut v = vec![
        PathBuf::from("/etc/safe-rm.conf"),
        PathBuf::from("/usr/local/etc/safe-rm.conf"),
    ];
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.as_ref().map(|h| h.join(".config")));
    if let Some(c) = config {
        v.push(c.join("safe-rm"));
    }
    if let Some(h) = home {
        v.push(h.join(".safe-rm"));
    }
    v
}

/// Patterns from one safe-rm config body: one path or shell pattern per
/// line; blank lines and `#` comments skipped.
pub fn parse_config(body: &str) -> Vec<String> {
    body.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.trim_end_matches('/').to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// The protected list: safe-rm's defaults plus every config file present.
pub fn protected_patterns() -> Vec<String> {
    let mut v: Vec<String> = SAFE_RM_DEFAULTS.iter().map(|s| s.to_string()).collect();
    for f in config_files() {
        if let Ok(body) = fs::read_to_string(&f) {
            v.extend(parse_config(&body));
        }
    }
    v
}

/// The pattern that protects `abs`, if any. `abs` should have its parent
/// resolved (as put records originals), so `/usr/../usr` cannot slip by.
pub fn protected_by<'a>(abs: &Path, patterns: &'a [String]) -> Option<&'a str> {
    let text = abs.to_string_lossy();
    let text = text.trim_end_matches('/');
    patterns
        .iter()
        .find(|p| {
            if p.contains(['*', '?', '[']) {
                crate::util::fnmatch(p, text)
            } else {
                p.as_str() == text
            }
        })
        .map(String::as_str)
}

/// Whether directory `abs`, with its parent resolved, names the user's
/// home directory.
pub fn is_home(abs: &Path) -> bool {
    static HOME: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    let home = HOME.get_or_init(|| {
        let h = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
        Some(fs::canonicalize(&h).unwrap_or_else(|_| PathBuf::from(h)))
    });
    let Some(home) = home else {
        return false;
    };
    // Put hands over a path with its parent resolved and a directory
    // (never a symlink) at its end, so it compares as is.
    home == abs
}

/// Whether directory `abs` (with metadata `meta`) sits on another device
/// than its parent: the root of a mount. Renaming one fails, and a copy
/// into the trash would drag a whole file system along.
pub fn is_mount_root(abs: &Path, meta: &fs::Metadata) -> bool {
    if !meta.is_dir() {
        return false;
    }
    match abs.parent() {
        Some(parent) => fs::metadata(parent).is_ok_and(|p| p.dev() != meta.dev()),
        None => true,
    }
}

/// For a directory that holds a git repository at its top, the risky
/// state of that repository (dirty, unpushed or unknown), if any.
pub fn risky_repo_state(abs: &Path) -> Option<crate::du_git::GitState> {
    let is_repo = abs.join(".git").symlink_metadata().is_ok() || crate::du::is_bare_repo(abs);
    if !is_repo {
        return None;
    }
    let repos = [abs.to_path_buf()];
    let rows = [crate::du_git::RowQuery {
        path: abs,
        is_dir: true,
        repos: &repos,
    }];
    let states = crate::du_git::classify_all(&rows)?;
    let (state, _) = states.into_iter().next()??;
    state.is_risky().then_some(state)
}

/// The last component of `path` as typed, past any trailing slashes, and
/// whether a slash trailed it. `Path::file_name` reads `d/.` as `d` and
/// gives no name for `./` or `d/..`, so the operand rules read the bytes.
pub fn typed_last(path: &Path) -> (&[u8], bool) {
    let b = path.as_os_str().as_bytes();
    let end = b.iter().rposition(|&c| c != b'/').map_or(0, |i| i + 1);
    let start = b[..end]
        .iter()
        .rposition(|&c| c == b'/')
        .map_or(0, |i| i + 1);
    (&b[start..end], end < b.len())
}

/// An operand whose last component is `.` or `..`, however it is spelled
/// (`.`, `./`, `d/.`, `d/..//`). put refuses each one, as GNU rm does.
pub fn is_dot_operand(path: &Path) -> bool {
    matches!(typed_last(path).0, b"." | b"..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_operands_are_spotted_however_they_are_spelled() {
        for p in [
            ".", "./", ".//", "..", "../", "d/.", "d/./", "d/..", "d/..//", "/a/.",
        ] {
            assert!(is_dot_operand(Path::new(p)), "{p}");
        }
        for p in ["d", "d/", "d//", ".d", "..d", "d.", "a/.d/", "/", ""] {
            assert!(!is_dot_operand(Path::new(p)), "{p}");
        }
        assert_eq!(typed_last(Path::new("a//d//")), (&b"d"[..], true));
        assert_eq!(typed_last(Path::new("a/b")), (&b"b"[..], false));
        assert_eq!(typed_last(Path::new("/")), (&b""[..], true));
    }

    #[test]
    fn config_lines_and_patterns_protect_paths() {
        let mut pats: Vec<String> = SAFE_RM_DEFAULTS.iter().map(|s| s.to_string()).collect();
        pats.extend(parse_config(
            "# mine\n\n/srv/data/\n/home/*/precious\n  /opt/keep  \n",
        ));
        assert_eq!(protected_by(Path::new("/usr"), &pats), Some("/usr"));
        assert_eq!(protected_by(Path::new("/usr/lib/x"), &pats), None);
        assert_eq!(
            protected_by(Path::new("/srv/data"), &pats),
            Some("/srv/data")
        );
        assert_eq!(
            protected_by(Path::new("/home/ann/precious"), &pats),
            Some("/home/*/precious")
        );
        assert_eq!(
            protected_by(Path::new("/opt/keep"), &pats),
            Some("/opt/keep")
        );
        assert_eq!(protected_by(Path::new("/opt/keeper"), &pats), None);
    }

    #[test]
    fn root_is_a_mount_root_and_a_plain_dir_is_not() {
        let root = fs::metadata("/").unwrap();
        assert!(is_mount_root(Path::new("/"), &root));
        let tmp = std::env::temp_dir().join(format!("rtrash-safety-{}", std::process::id()));
        fs::create_dir_all(tmp.join("sub")).unwrap();
        let sub = tmp.join("sub");
        assert!(!is_mount_root(&sub, &fs::metadata(&sub).unwrap()));
        let _ = fs::remove_dir_all(&tmp);
    }
}
