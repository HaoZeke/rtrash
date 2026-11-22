//! Paths that permanent-deletion commands must leave alone.
//!
//! The keep file sits beside `config.toml`, under the same platform
//! configuration directory. Each non-blank, non-comment line is an
//! original path or a path prefix. Prefixes are matched by path component,
//! so `/srv/data` protects `/srv/data/x` but not `/srv/database`.

use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct Keep {
    paths: Vec<PathBuf>,
}

impl Keep {
    /// Read the keep file. A missing file means no protected paths.
    pub fn load() -> io::Result<Keep> {
        let Some(path) = path() else {
            return Ok(Keep::default());
        };
        let body = match std::fs::read_to_string(path) {
            Ok(body) => body,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Keep::default()),
            Err(e) => return Err(e),
        };
        let home = crate::util::home_dir();
        let cwd = std::env::current_dir().ok();
        let mut paths = Vec::new();
        for raw in body.lines() {
            let line = raw.split_once('#').map_or(raw, |(line, _)| line).trim();
            if line.is_empty() {
                continue;
            }
            let value = parse_keep_line(line, home.as_deref(), cwd.as_deref())?;
            if !paths.contains(&value) {
                paths.push(value);
            }
        }
        Ok(Keep { paths })
    }

    /// Whether `path` is one of the configured paths or below one.
    pub fn protects(&self, path: &Path) -> bool {
        !path.as_os_str().is_empty() && self.paths.iter().any(|keep| keep_covers(keep, path))
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

/// One keep line: `~/...` from the home directory, an absolute path as
/// given, and a relative path from `cwd`. A relative path with no working
/// directory, or `~` with no home, is an error the caller refuses on.
fn parse_keep_line(line: &str, home: Option<&Path>, cwd: Option<&Path>) -> io::Result<PathBuf> {
    if line == "~" {
        let home = home.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "keep file uses '~' but HOME is unset",
            )
        })?;
        return normalize_keep(home);
    }
    if let Some(rest) = line.strip_prefix("~/").or_else(|| line.strip_prefix("~\\")) {
        let home = home.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "keep file uses '~' but HOME is unset",
            )
        })?;
        let joined = if rest.is_empty() {
            home.to_path_buf()
        } else {
            home.join(rest)
        };
        return normalize_keep(&joined);
    }
    let path = PathBuf::from(line);
    if path.is_absolute() {
        return normalize_keep(&path);
    }
    let cwd = cwd.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("keep file path '{line}' is relative and the working directory cannot be read"),
        )
    })?;
    normalize_keep(&cwd.join(path))
}

/// The path `put` would record: symlinks in the parents and, where the
/// path exists, the final component. A leftover `..` in a tail that does
/// not exist yet is applied only after that, so `..` across a symlink is
/// the physical parent. A line that is `/` or a drive root is refused: it
/// would protect every path and turn a permanent delete into a no-op.
fn normalize_keep(path: &Path) -> io::Result<PathBuf> {
    let resolved = resolve_keep(path, 0);
    let lexical = crate::rm_match::lexical_normalize(&resolved);
    let out = crate::util::plain_path(&crate::util::absolute_keep_tail(&lexical));
    if keep_is_volume_root(&out) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "keep file path '{}' is a whole volume; name a file or directory inside it",
                out.display()
            ),
        ));
    }
    Ok(out)
}

fn resolve_keep(path: &Path, depth: u8) -> PathBuf {
    if depth > 8 {
        return path.to_path_buf();
    }
    if let Ok(canon) = std::fs::canonicalize(path) {
        return crate::util::plain_path(&canon);
    }
    if let Ok(target) = std::fs::read_link(path) {
        let joined = if target.is_absolute() {
            target
        } else {
            match path.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent.join(target),
                _ => target,
            }
        };
        if joined != path {
            return resolve_keep(&joined, depth + 1);
        }
    }
    resolve_existing_keep(path)
}

/// Longest existing ancestor, with symlinks resolved, and the rest appended.
/// The same shape as the path `put` records for a parent that exists.
fn resolve_existing_keep(path: &Path) -> PathBuf {
    for ancestor in path.ancestors() {
        if ancestor.as_os_str().is_empty() {
            break;
        }
        if let Ok(real) = ancestor.canonicalize() {
            let real = crate::util::plain_path(&real);
            return match path.strip_prefix(ancestor) {
                Ok(rest) if !rest.as_os_str().is_empty() => real.join(rest),
                _ => real,
            };
        }
    }
    path.to_path_buf()
}

fn keep_is_volume_root(path: &Path) -> bool {
    let s = path.to_string_lossy();
    if s == "/" || s == "\\" || s.is_empty() {
        return true;
    }
    let b = s.as_bytes();
    let shaped =
        (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':') || s.starts_with(r"\\");
    if cfg!(windows) || shaped {
        let key = crate::win_recycle::path_key(&s, &[]);
        return crate::win_recycle::mass_canonical_key(&key);
    }
    false
}

/// Whether `keep` covers `path`, in either direction and on a component
/// boundary. A kept file protects the directory that holds it, and a kept
/// directory protects what is inside it. On Windows the comparison is the
/// case-insensitive key the Recycle Bin matcher uses.
fn keep_covers(keep: &Path, path: &Path) -> bool {
    if keep.as_os_str().is_empty() || path.as_os_str().is_empty() {
        return false;
    }
    #[cfg(windows)]
    {
        let path_key = crate::win_recycle::path_key(&path.to_string_lossy(), &[]);
        let keep_key = crate::win_recycle::path_key(&keep.to_string_lossy(), &[]);
        let under = |path: &str, prefix: &str| {
            crate::win_recycle::win_path_matches(
                crate::filters::Match::Prefix,
                path,
                std::slice::from_ref(&prefix.to_string()),
            )
        };
        under(&path_key, &keep_key) || under(&keep_key, &path_key)
    }
    #[cfg(not(windows))]
    {
        path.starts_with(keep) || keep.starts_with(path)
    }
}

/// The keep file beside the active config file.
pub fn path() -> Option<PathBuf> {
    crate::config::path().and_then(|p| p.parent().map(|d| d.join("keep")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_prefixes_end_at_components() {
        let keep = Keep {
            paths: vec![PathBuf::from("/srv/data")],
        };
        assert!(keep.protects(Path::new("/srv/data")));
        assert!(keep.protects(Path::new("/srv/data/x")));
        assert!(!keep.protects(Path::new("/srv/database")));
        assert!(!keep.protects(Path::new("/srv/data2")));
        let inside = Keep {
            paths: vec![PathBuf::from("/srv/data/secret")],
        };
        assert!(inside.protects(Path::new("/srv/data/secret")));
        assert!(inside.protects(Path::new("/srv/data")));
        assert!(inside.protects(Path::new("/srv")));
        assert!(!inside.protects(Path::new("/srv/other")));
        assert!(!inside.protects(Path::new("/tmp/unrelated")));
        let empty = Keep {
            paths: vec![PathBuf::new()],
        };
        assert!(!empty.protects(Path::new("/srv/data")));
    }

    #[test]
    fn a_relative_keep_line_is_read_from_the_working_directory() {
        let cwd = Path::new("/work");
        let got = parse_keep_line("secret.txt", None, Some(cwd)).unwrap();
        assert_eq!(got, Path::new("/work/secret.txt"));
        let nested = parse_keep_line("dir/secret", None, Some(cwd)).unwrap();
        assert_eq!(nested, Path::new("/work/dir/secret"));
        let parent = parse_keep_line("../secret", None, Some(Path::new("/work/proj"))).unwrap();
        assert_eq!(parent, Path::new("/work/secret"));
        let err = parse_keep_line("secret.txt", None, None).unwrap_err();
        assert!(err.to_string().contains("relative"), "{err}");
        let err = parse_keep_line("~", None, Some(cwd)).unwrap_err();
        assert!(err.to_string().contains("HOME"), "{err}");
        let home = Path::new("/home/u");
        assert_eq!(
            parse_keep_line("~/secret", Some(home), None).unwrap(),
            Path::new("/home/u/secret")
        );
    }

    /// Windows keep paths compare without case, and still stop at a
    /// component, so `Work` does not cover `WorkOut`.
    #[cfg(windows)]
    #[test]
    fn keep_paths_match_without_case() {
        let keep = Keep {
            paths: vec![PathBuf::from(r"C:\Users\Alice\Work")],
        };
        assert!(keep.protects(Path::new(r"c:\users\alice\work\file.txt")));
        assert!(keep.protects(Path::new(r"C:/Users/Alice/Work")));
        assert!(keep.protects(Path::new(r"C:\Users\Alice")));
        assert!(!keep.protects(Path::new(r"C:\Users\Alice\WorkOut\file.txt")));
        assert!(!keep.protects(Path::new(r"D:\Users\Alice\Work\file.txt")));
    }

    #[cfg(unix)]
    #[test]
    fn a_keep_line_that_is_the_root_is_rejected() {
        let err = parse_keep_line(".", None, Some(Path::new("/"))).unwrap_err();
        assert!(err.to_string().contains("whole volume"), "{err}");
        let err = parse_keep_line("/", None, None).unwrap_err();
        assert!(err.to_string().contains("whole volume"), "{err}");
    }

    /// The file is already gone, as it is when `rm` reads the keep file
    /// after `put`. The keep line still has to name the path put recorded.
    #[cfg(unix)]
    fn keep_of(line: &Path) -> Keep {
        Keep {
            paths: vec![parse_keep_line(line.to_str().unwrap(), None, None).unwrap()],
        }
    }

    #[cfg(unix)]
    fn keep_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "rtrash-keep-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::canonicalize(&root).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_parent_keep_protects_the_resolved_path() {
        let root = keep_root();
        let real = root.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let secret = real.join("secret");
        std::fs::write(&secret, b"s").unwrap();
        std::fs::remove_file(&secret).unwrap();
        let keep = keep_of(&link.join("secret"));
        assert!(keep.protects(&secret), "{:?}", keep.paths);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_keep_protects_the_resolved_path() {
        let root = keep_root();
        let real = root.join("dir");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.join("dlink");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        std::fs::remove_dir(&real).unwrap();
        let keep = keep_of(&link);
        assert!(keep.protects(&real), "{:?}", keep.paths);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_dotdot_across_a_symlink_keep_protects_the_resolved_path() {
        let root = keep_root();
        let real = root.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let hole = root.join("hole");
        std::fs::create_dir_all(&hole).unwrap();
        let link = hole.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let secret = root.join("secret");
        std::fs::write(&secret, b"s").unwrap();
        std::fs::remove_file(&secret).unwrap();
        let keep = keep_of(&link.join("..").join("secret"));
        assert!(keep.protects(&secret), "{:?}", keep.paths);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    #[test]
    fn a_keep_line_that_is_a_drive_root_is_rejected() {
        let err = parse_keep_line(r"C:\", None, None).unwrap_err();
        assert!(err.to_string().contains("whole volume"), "{err}");
        let err = parse_keep_line(r"\\server\share", None, None).unwrap_err();
        assert!(err.to_string().contains("whole volume"), "{err}");
    }
}
