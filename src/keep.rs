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
        let mut paths = Vec::new();
        for raw in body.lines() {
            let line = raw.split_once('#').map_or(raw, |(line, _)| line).trim();
            if line.is_empty() {
                continue;
            }
            let value = match (line.strip_prefix("~/"), &home) {
                (Some(rest), Some(home)) => home.join(rest),
                _ => PathBuf::from(line),
            };
            if !paths.contains(&value) {
                paths.push(value);
            }
        }
        Ok(Keep { paths })
    }

    /// Whether `path` is one of the configured paths or below one.
    pub fn protects(&self, path: &Path) -> bool {
        !path.as_os_str().is_empty() && self.paths.iter().any(|keep| path.starts_with(keep))
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
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
    }
}
