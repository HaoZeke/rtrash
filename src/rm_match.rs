//! Matching safeguards shared by the permanent `rm` backends.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Whether an item id was selected explicitly.
pub fn id_hit(ids: &[String], id: &str) -> bool {
    ids.iter().any(|want| want == id)
}

/// Refuse an exact-path selection that names more than one trash item.
pub fn refuse_ambiguous<'a>(
    prog: &str,
    paths: impl IntoIterator<Item = &'a Path>,
    all_matches: bool,
) -> bool {
    if all_matches {
        return false;
    }
    let mut counts: BTreeMap<PathBuf, usize> = BTreeMap::new();
    for path in paths {
        *counts.entry(path.to_path_buf()).or_default() += 1;
    }
    let ambiguous: Vec<(PathBuf, usize)> = counts.into_iter().filter(|(_, n)| *n > 1).collect();
    if ambiguous.is_empty() {
        return false;
    }
    for (path, n) in ambiguous {
        eprintln!(
            "{prog}: refusing {n} trash items with original path '{}'",
            crate::util::err_path(&path)
        );
    }
    eprintln!("{prog}: select one with --id=NAME, or pass --all-matches");
    true
}

/// [`refuse_ambiguous`] by the key matching used, not by the path bytes.
/// One exact operand is one group of keys (the path as given and with its
/// parent resolved). Two items whose keys sit in that group are one place
/// spelled two ways: `C:\Temp\x.txt` and `c:\temp\X.TXT`, or `RUNNER~1`
/// and the long name the parent resolves to.
#[cfg(any(windows, test))]
pub fn refuse_ambiguous_keys(
    prog: &str,
    groups: &[Vec<String>],
    items: &[(String, PathBuf)],
    all_matches: bool,
) -> bool {
    if all_matches {
        return false;
    }
    let mut refused = false;
    for group in groups {
        let hits: Vec<&Path> = items
            .iter()
            .filter(|(key, _)| group.contains(key))
            .map(|(_, path)| path.as_path())
            .collect();
        if hits.len() < 2 {
            continue;
        }
        refused = true;
        eprintln!(
            "{prog}: refusing {} trash items with original path '{}'",
            hits.len(),
            crate::util::err_path(hits[0])
        );
    }
    if refused {
        eprintln!("{prog}: select one with --id=NAME, or pass --all-matches");
    }
    refused
}

pub fn no_matches(prog: &str) -> i32 {
    eprintln!("{prog}: no trash items match");
    1
}
