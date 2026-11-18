//! Operator keep lists for permanent trash removal.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
enum Rule {
    Prefix(PathBuf),
    Glob(String),
}

/// Original paths that `rm` and `empty` must never permanently remove.
#[derive(Clone, Debug, Default)]
pub struct KeepList {
    rules: Vec<Rule>,
}

impl KeepList {
    /// Read the files named by `RTRASH_KEEP_FROM` and `--keep-from`.
    ///
    /// A missing or unreadable file is a usage error: continuing without
    /// an operator's requested protection would be unsafe.
    pub fn load(prog: &str, files: &[PathBuf]) -> Result<Self, i32> {
        let mut sources = Vec::new();
        if let Some(path) = std::env::var_os("RTRASH_KEEP_FROM").filter(|p| !p.is_empty()) {
            sources.push(PathBuf::from(path));
        }
        sources.extend_from_slice(files);

        let mut rules = Vec::new();
        for source in sources {
            let body = match std::fs::read_to_string(&source) {
                Ok(body) => body,
                Err(e) => {
                    eprintln!(
                        "{prog}: cannot read keep list '{}': {e}",
                        crate::util::err_path(&source)
                    );
                    return Err(2);
                }
            };
            for line in body.lines() {
                let rule = line.trim();
                if rule.is_empty() || rule.starts_with('#') {
                    continue;
                }
                if rule.contains(['*', '?', '[']) {
                    rules.push(Rule::Glob(rule.to_string()));
                } else {
                    let prefix = crate::util::arg_path(rule);
                    let prefix = if prefix.is_absolute() {
                        prefix
                    } else {
                        std::path::absolute(&prefix).unwrap_or(prefix)
                    };
                    rules.push(Rule::Prefix(prefix));
                }
            }
        }
        Ok(Self { rules })
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Whether `original` equals or is below a prefix, or matches a glob.
    pub fn keeps(&self, original: &Path) -> bool {
        self.rules.iter().any(|rule| match rule {
            Rule::Prefix(prefix) => original.starts_with(prefix),
            Rule::Glob(pattern) => {
                let original = crate::util::str_from_os_ref(original.as_os_str());
                #[cfg(windows)]
                {
                    crate::util::fnmatch(&pattern.to_lowercase(), &original.to_lowercase())
                }
                #[cfg(not(windows))]
                {
                    crate::util::fnmatch(pattern, &original)
                }
            }
        })
    }
}

/// Take `--keep-from=FILE` or `--keep-from FILE` at `args[*i]`.
pub fn take_arg(
    args: &[String],
    i: &mut usize,
    files: &mut Vec<PathBuf>,
) -> Option<Result<(), String>> {
    let arg = args[*i].as_str();
    let value = match arg.strip_prefix("--keep-from=") {
        Some(value) => value,
        None if arg == "--keep-from" => {
            *i += 1;
            match args.get(*i) {
                Some(value) => value,
                None => return Some(Err("--keep-from requires FILE".into())),
            }
        }
        None => return None,
    };
    if value.is_empty() {
        return Some(Err("--keep-from requires a non-empty FILE".into()));
    }
    files.push(crate::util::arg_path(value));
    Some(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::{KeepList, Rule};
    use std::path::{Path, PathBuf};

    #[test]
    fn prefixes_stop_at_path_component_boundaries_and_globs_match_whole_paths() {
        let keep = KeepList {
            rules: vec![
                Rule::Prefix(PathBuf::from("/srv/protected")),
                Rule::Glob("/home/*/keep/**".into()),
            ],
        };
        assert!(keep.keeps(Path::new("/srv/protected")));
        assert!(keep.keeps(Path::new("/srv/protected/a")));
        assert!(!keep.keeps(Path::new("/srv/protected-old/a")));
        assert!(keep.keeps(Path::new("/home/u/keep/a")));
        assert!(!keep.keeps(Path::new("/home/u/cache/a")));
    }
}
