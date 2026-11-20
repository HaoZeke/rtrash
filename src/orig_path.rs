//! The original path `put` records, and the near misses of an exact one.
//!
//! `rm` and `empty` take a bare path as that original path, not as a glob.
//! A relative path is resolved from the current directory, and a trailing
//! slash is not part of the path. When nothing matches, the longer paths
//! that start the same way are named and left alone.

use std::path::{Component, Path, PathBuf};

/// Every spelling of `typed` that can equal the path a backend recorded:
/// the path made absolute from the current directory, and the same path
/// with a link in a parent resolved, which is the path `put` writes down.
pub fn forms(typed: &str) -> Vec<String> {
    let lexical = lexical_abs(Path::new(typed));
    let mut out = vec![norm(&lexical)];
    if let Some(put) = put_form(&lexical) {
        let recorded = norm(&put);
        if !out.contains(&recorded) {
            out.push(recorded);
        }
    }
    out
}

/// `forms` of each typed path, in order, without repeats.
pub fn expand(typed: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for one in typed {
        for form in forms(one) {
            if !out.contains(&form) {
                out.push(form);
            }
        }
    }
    out
}

/// The path in the form two originals are compared in: NFC, no `\\?\`
/// prefix, slashes folded on Windows, and no trailing separator.
pub fn norm(path: &Path) -> String {
    let plain = crate::util::plain_path(path);
    let text = crate::util::str_from_os_ref(plain.as_os_str());
    trim_sep(&compose(&text))
}

/// `original` is one of the wanted forms.
pub fn selects(wants: &[String], original: &Path) -> bool {
    let got = norm(original);
    wants.iter().any(|want| want == &got)
}

/// `original` is a longer path that starts with `want`: a sibling such as
/// `~/ab` or a child such as `~/a/b`, never `want` itself.
pub fn continues(want: &Path, original: &Path) -> bool {
    let want = norm(want);
    let original = norm(original);
    original != want && original.starts_with(&want)
}

/// When a wanted form matches nothing, name the originals that only start
/// with it. An original that equals some other wanted form is a hit of
/// that form, so it is not named here.
pub fn hint(prog: &str, wants: &[String], have: &[PathBuf]) {
    let got: Vec<String> = have.iter().map(|path| norm(path)).collect();
    let mut named = std::collections::BTreeSet::new();
    for want in wants {
        if got.iter().any(|one| one == want) {
            continue;
        }
        let mut near: Vec<&Path> = Vec::new();
        for (path, one) in have.iter().zip(got.iter()) {
            let longer = one.starts_with(want.as_str()) && one != want;
            let exact_elsewhere = wants.iter().any(|w| w == one);
            if longer && !exact_elsewhere && named.insert(one.clone()) {
                near.push(path);
            }
        }
        if near.is_empty() {
            continue;
        }
        near.sort_by_key(|path| norm(path));
        eprintln!(
            "{prog}: no exact match for '{}'",
            crate::util::err_path(Path::new(want))
        );
        for path in near {
            eprintln!("{prog}: near miss: '{}'", crate::util::err_path(path));
        }
    }
}

fn lexical_abs(path: &Path) -> PathBuf {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut out = PathBuf::new();
    for component in abs.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// The path `put` records for `abs`: the parent resolved through links,
/// then the final name, which is not followed. On Windows a path that is
/// still there is the full canonical path, which is what that `put` writes.
fn put_form(abs: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    if let Ok(full) = std::fs::canonicalize(abs) {
        return Some(crate::util::plain_path(&full));
    }
    let name = abs.file_name()?.to_os_string();
    let parent = abs.parent().filter(|p| !p.as_os_str().is_empty())?;
    Some(resolve_parent(parent).join(name))
}

fn resolve_parent(parent: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut cur = parent.to_path_buf();
    loop {
        if cur.as_os_str().is_empty() {
            break;
        }
        if let Ok(real) = std::fs::canonicalize(&cur) {
            let mut out = crate::util::plain_path(&real);
            for name in missing.iter().rev() {
                out.push(name);
            }
            return out;
        }
        match cur.file_name() {
            Some(name) => {
                missing.push(name.to_os_string());
                cur.pop();
            }
            None => break,
        }
    }
    crate::util::plain_path(parent)
}

/// NFC on Unix, where Finder may store a decomposed name. Windows has no
/// such trash, and the crate that composes is Unix-only.
fn compose(text: &str) -> String {
    #[cfg(unix)]
    {
        use unicode_normalization::UnicodeNormalization;
        text.nfc().collect()
    }
    #[cfg(not(unix))]
    {
        text.to_string()
    }
}

fn trim_sep(text: &str) -> String {
    if text == "/" || text == "\\" || is_drive_root(text) {
        return text.to_string();
    }
    text.trim_end_matches(['/', '\\']).to_string()
}

fn is_drive_root(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 3 && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rtrash-orig-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("real")).unwrap();
        dir
    }

    #[test]
    fn a_trailing_slash_is_the_same_path() {
        let dir = scratch("slash");
        let with = forms(&format!("{}/real/", dir.display()));
        let bare = forms(&format!("{}/real", dir.display()));
        assert_eq!(with, bare, "{with:?} {bare:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_near_miss_is_a_longer_path_and_not_the_path_itself() {
        let want = Path::new("/home/u/a");
        assert!(continues(want, Path::new("/home/u/ab")));
        assert!(continues(want, Path::new("/home/u/a/b")));
        assert!(!continues(want, Path::new("/home/u/a")));
        assert!(!continues(want, Path::new("/home/u/a/")));
        assert!(!continues(want, Path::new("/home/u/b")));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_in_the_parent_is_the_path_put_records() {
        let dir = scratch("link");
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        let got = forms(&format!("{}/link/a.txt", dir.display()));
        let recorded = norm(&dir.join("real/a.txt"));
        assert!(
            got.iter().any(|form| form == &recorded),
            "{got:?} {recorded}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
