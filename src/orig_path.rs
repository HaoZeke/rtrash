//! The original path `put` records, and the near misses of an exact one.
//!
//! `rm` and `empty` take a bare path as that original path, not as a glob.
//! A relative path is resolved from the current directory, and a trailing
//! slash is not part of the path. When nothing matches, the longer paths
//! that start the same way are named and left alone.
//!
//! On Windows the comparison is [`crate::win_recycle::path_key`]: letter
//! case does not matter, `/` and `\` are the same separator, and a volume
//! GUID path is its mount point. [`forms`] adds the short and long
//! spellings of a parent that still exists, with the final name as given,
//! so a short path in a `$I` record matches the long path that was typed
//! and the other way around. When that final component is still on disk
//! and is not a link, its own short and long names are forms too. After
//! the item has moved, the directory entry is gone, so the final name is
//! only the one the `$I` record stored.

use std::path::{Component, Path, PathBuf};

/// Every spelling of `typed` that can equal the path a backend recorded:
/// the path made absolute from the current directory, and the same path
/// with a link in a parent resolved, which is the path `put` writes down.
/// On Windows, also the short and long names of that parent, and of the
/// final component when it is still on disk and is not a link.
pub fn forms(typed: &str) -> Vec<String> {
    let lexical = lexical_abs(Path::new(typed));
    let mut out = Vec::new();
    push_form(&mut out, &lexical);
    if let Some(put) = put_form(&lexical) {
        push_form(&mut out, &put);
    }
    #[cfg(windows)]
    push_windows_forms(&mut out, &lexical);
    out
}

fn push_form(out: &mut Vec<String>, path: &Path) {
    let recorded = norm(path);
    if out.iter().any(|have| have == &recorded) {
        return;
    }
    out.push(recorded);
}

/// `forms` of each typed path, in order, without repeats.
pub fn expand(typed: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for one in typed {
        for form in forms(one) {
            if !out.contains(&form) {
                out.push(form);
            }
        }
    }
    out
}

/// The path two originals are compared in. On Windows this is
/// [`crate::win_recycle::path_key`]: `\\?\` gone, a volume GUID read as
/// its mount, `/` and `\` the same, no trailing separator, and lower
/// case. Elsewhere it is the plain path with no trailing separator, and
/// on macOS it is NFC.
pub fn norm(path: &Path) -> String {
    let plain = crate::util::plain_path(path);
    let text = crate::util::str_from_os_ref(plain.as_os_str());
    let text = trim_sep(&compose(&text));
    #[cfg(windows)]
    {
        crate::win_recycle::path_key(&text, windows_mounts())
    }
    #[cfg(not(windows))]
    {
        text
    }
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

/// The phrase for an exact path that is in the trash and fails the age
/// filter, the size filter, or both.
pub fn window(age: bool, size: bool) -> &'static str {
    match (age, size) {
        (true, true) => "the age or size window",
        (false, true) => "the size window",
        _ => "the age window",
    }
}

/// What to say about one wanted form. `have` is every original in scope,
/// before the age and size filters. `kept` is the subset those filters
/// still allow.
enum Notice {
    /// The path is in `have` and missing from `kept`.
    Outside,
    /// The path is in neither, and these longer paths start with it.
    Miss(Vec<PathBuf>),
}

fn notices(wants: &[String], have: &[PathBuf], kept: &[PathBuf]) -> Vec<(String, Notice)> {
    let got: Vec<String> = have.iter().map(|path| norm(path)).collect();
    let stay: Vec<String> = kept.iter().map(|path| norm(path)).collect();
    let mut named = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for want in wants {
        if stay.iter().any(|one| one == want) {
            continue;
        }
        if got.iter().any(|one| one == want) {
            out.push((want.clone(), Notice::Outside));
            continue;
        }
        let mut near = Vec::new();
        for (path, one) in have.iter().zip(got.iter()) {
            let longer = one.starts_with(want.as_str()) && one != want;
            let exact_elsewhere = wants.iter().any(|w| w == one);
            if longer && !exact_elsewhere && named.insert(one.clone()) {
                near.push(path.clone());
            }
        }
        if near.is_empty() {
            continue;
        }
        near.sort_by_key(|path| norm(path));
        out.push((want.clone(), Notice::Miss(near)));
    }
    out
}

/// When a wanted form matches nothing in `kept`, either name it as outside
/// `window` or name the longer paths in `have`. An original that equals
/// some other wanted form is a hit of that form, so it is not named here.
pub fn hint(prog: &str, wants: &[String], have: &[PathBuf], kept: &[PathBuf], window: &str) {
    for (want, notice) in notices(wants, have, kept) {
        match notice {
            Notice::Outside => eprintln!(
                "{prog}: '{}' is in the trash, outside {window}",
                crate::util::err_path(Path::new(&want))
            ),
            Notice::Miss(near) => {
                eprintln!(
                    "{prog}: no exact match for '{}'",
                    crate::util::err_path(Path::new(&want))
                );
                for path in near {
                    eprintln!("{prog}: near miss: '{}'", crate::util::err_path(&path));
                }
            }
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
/// then the final name, which is not followed. A symlink or junction in
/// that final component stays the link, on every system.
fn put_form(abs: &Path) -> Option<PathBuf> {
    let name = abs.file_name()?.to_os_string();
    let parent = abs.parent().filter(|p| !p.as_os_str().is_empty())?;
    Some(resolve_parent(parent).join(name))
}

/// Short and long parents, and the final component's own short and long
/// names when that component is still on disk and is not a link.
/// `GetShortPathNameW` on a file that has moved fails, and the `$I`
/// record from [`crate::win_recycle::test_bin_put`] stores one path
/// (`absolute` of the operand), so a deleted file's own 8.3 name is not
/// recovered. The parent is still there, and both of its spellings are.
#[cfg(windows)]
fn push_windows_forms(out: &mut Vec<String>, abs: &Path) {
    let Some(parent) = abs.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return;
    };
    let mut names = Vec::new();
    if let Some(name) = abs.file_name() {
        names.push(name.to_os_string());
    }
    // A link's short path can be the target's. Ask only for a real entry.
    let ask = std::fs::symlink_metadata(abs).is_ok_and(|m| !m.file_type().is_symlink());
    if ask {
        for alias in [
            crate::win_recycle::short_path(abs),
            crate::win_recycle::long_path(abs),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(name) = alias.file_name() {
                names.push(name.to_os_string());
            }
        }
    }
    let mut parents = vec![resolve_parent(parent)];
    if let Some(short) = crate::win_recycle::short_path(parent) {
        parents.push(crate::util::plain_path(&short));
    }
    for dir in parents {
        for name in &names {
            push_form(out, &dir.join(name));
        }
    }
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

/// Mounts for [`crate::win_recycle::path_key`], read once. An empty list
/// still folds case and separators; a volume GUID then stays as written.
#[cfg(windows)]
fn windows_mounts() -> &'static [(String, String)] {
    use std::sync::OnceLock;
    static MOUNTS: OnceLock<Vec<(String, String)>> = OnceLock::new();
    MOUNTS.get_or_init(crate::win_recycle::volume_mounts)
}

/// NFC on macOS, where Finder may store a decomposed name. FreeDesktop
/// paths compare as bytes, so a composed `é` and a decomposed one stay
/// two files. Windows has no such trash.
fn compose(text: &str) -> String {
    #[cfg(target_os = "macos")]
    {
        use unicode_normalization::UnicodeNormalization;
        text.nfc().collect()
    }
    #[cfg(not(target_os = "macos"))]
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
    fn a_final_symlink_is_not_the_path_put_records() {
        let dir = scratch("finallink");
        std::fs::write(dir.join("target"), b"t").unwrap();
        std::os::unix::fs::symlink("target", dir.join("link")).unwrap();
        let got = forms(&format!("{}/link", dir.display()));
        assert!(got.iter().all(|f| f.ends_with("/link")), "{got:?}");
        assert!(got.iter().all(|f| !f.ends_with("/target")), "{got:?}");
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn an_exact_path_outside_the_window_is_not_a_near_miss() {
        let exact = PathBuf::from("/home/u/a");
        let sibling = PathBuf::from("/home/u/ab");
        let child = PathBuf::from("/home/u/a/b");
        let wants = vec![norm(&exact)];
        let have = vec![exact.clone(), sibling.clone(), child.clone()];
        let kept = vec![sibling.clone(), child.clone()];
        let notes = notices(&wants, &have, &kept);
        assert_eq!(notes.len(), 1);
        assert!(matches!(notes[0].1, Notice::Outside));

        let absent = notices(&wants, &[sibling.clone(), child.clone()], &[sibling, child]);
        match &absent[0].1 {
            Notice::Miss(near) => {
                let names: Vec<_> = near.iter().map(|p| norm(p)).collect();
                assert!(names.iter().any(|n| n.ends_with("/ab")));
                assert!(names.iter().any(|n| n.ends_with("/a/b")));
            }
            Notice::Outside => panic!("a missing path is a near miss"),
        }
    }

    /// Windows compares through `path_key`: case and `/` against `\` are
    /// the same path, and a shorter name is not.
    #[cfg(windows)]
    #[test]
    fn windows_exact_paths_match_across_case_and_separators() {
        let typed = Path::new(r"C:\Users\A\File.TXT");
        let slash = Path::new("c:/users/a/file.txt");
        let verbatim = Path::new(r"\\?\C:\Users\A\File.TXT");
        let key = crate::win_recycle::path_key(r"C:\Users\A\File.TXT", &[]);
        assert_eq!(norm(typed), key);
        assert_eq!(norm(slash), key);
        assert_eq!(norm(verbatim), key);
        assert!(selects(&[norm(typed)], slash));
        assert!(selects(&[norm(slash)], verbatim));
        assert!(!selects(&[norm(typed)], Path::new(r"C:\Users\A\File.TX")));
        assert!(!selects(&[norm(typed)], Path::new(r"C:\Users\A\Other.TXT")));
    }
}
