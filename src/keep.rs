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
        return home.map(|h| h.to_path_buf()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "keep file uses '~' but HOME is unset",
            )
        });
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
        return Ok(normalize_keep(&joined));
    }
    let path = PathBuf::from(line);
    if path.is_absolute() {
        return Ok(normalize_keep(&path));
    }
    let cwd = cwd.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("keep file path '{line}' is relative and the working directory cannot be read"),
        )
    })?;
    Ok(normalize_keep(&cwd.join(path)))
}

fn normalize_keep(path: &Path) -> PathBuf {
    crate::util::absolute_keep_tail(&crate::rm_match::lexical_normalize(path))
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
}
