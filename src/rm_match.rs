//! Matching safeguards shared by the permanent `rm` backends.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Whether an item id was selected explicitly.
pub fn id_hit(ids: &[String], id: &str) -> bool {
    ids.iter().any(|want| want == id)
}

/// An `--id` selects that entry and no other. A path beside it does not
/// widen the selection, and it does not turn the duplicate check off for
/// every path match.
pub fn chosen(pattern_hit: bool, id_hit: bool, ids_given: bool) -> bool {
    if ids_given {
        id_hit
    } else {
        pattern_hit
    }
}

/// `--batch` with no pattern and no `--id` is the whole put run. An id
/// is a selection of its own, so the run is not then taken as `*`.
pub fn batch_takes_the_whole_run(has_batch: bool, no_pattern: bool, no_id: bool) -> bool {
    has_batch && no_pattern && no_id
}

/// The path exact and prefix matching treat as one place.
///
/// `~` is this user's home. `.` and `..` are applied, and a symlink in a
/// parent is resolved. The last component stays as named, so a link there
/// is the item and not its target. An empty path stays empty: it is a
/// prefix of every path.
pub fn match_key(path: &Path) -> PathBuf {
    if path.as_os_str().is_empty() {
        return PathBuf::new();
    }
    let expanded = expand_tilde(path);
    let lexical = lexical_normalize(&expanded);
    if lexical.as_os_str().is_empty() {
        return std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    }
    if lexical == Path::new("/") {
        return lexical;
    }
    crate::allow_roots::resolve(&lexical).unwrap_or(lexical)
}

/// Whether `key` (already a [`match_key`]) is this user's home, the home
/// trash, `~/.Trash`, or `$XDG_DATA_HOME`. A prefix of one of those takes
/// the whole place. When neither the home nor an absolute `XDG_DATA_HOME`
/// can be read, every key is treated as one: a prefix then fails closed.
pub fn is_scope_root(key: &Path) -> bool {
    scope_listed(key, known_scope_roots().as_deref())
}

pub(crate) fn scope_listed(key: &Path, roots: Option<&[PathBuf]>) -> bool {
    let Some(roots) = roots else {
        return true;
    };
    roots.iter().any(|root| scope_same(root, key))
}

pub(crate) fn scope_same(root: &Path, key: &Path) -> bool {
    #[cfg(windows)]
    {
        windows_scope_same(root, key)
    }
    #[cfg(not(windows))]
    {
        root == key
    }
}

/// Windows scope roots compare by [`crate::win_recycle::path_key`], so
/// `C:\Users\Alice` and `c:\users\alice` are one home. A longer name is
/// not: `Alice` does not cover `Alice2`.
#[cfg(any(windows, test))]
pub(crate) fn windows_scope_same(root: &Path, key: &Path) -> bool {
    let mounts = &[];
    crate::win_recycle::path_key(&root.to_string_lossy(), mounts)
        == crate::win_recycle::path_key(&key.to_string_lossy(), mounts)
}

fn expand_tilde(path: &Path) -> PathBuf {
    let Some(home) = crate::util::home_dir() else {
        return path.to_path_buf();
    };
    let s = path.to_string_lossy();
    if s == "~" {
        return home;
    }
    if let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        return if rest.is_empty() {
            home
        } else {
            home.join(rest)
        };
    }
    path.to_path_buf()
}

/// Drop `.`, apply `..`, and leave a rooted path at its root.
pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component::{Normal, ParentDir};
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            ParentDir => {
                let pop_normal = matches!(out.components().next_back(), Some(Normal(_)));
                let push_parent = matches!(out.components().next_back(), Some(ParentDir) | None);
                if pop_normal {
                    out.pop();
                } else if push_parent {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Home, `~/.Trash`, the home trash, `$XDG_DATA_HOME` and its `Trash`,
/// each also in the form `put` records (the directory itself resolved).
/// `None` when neither an absolute home nor an absolute `XDG_DATA_HOME`
/// is known: callers fail closed rather than treat "no roots" as "nothing
/// is in scope".
pub(crate) fn known_scope_roots() -> Option<Vec<PathBuf>> {
    let home = crate::util::home_dir();
    let xdg = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
    scope_roots_from(home.as_deref(), xdg.as_deref())
}

pub(crate) fn scope_roots_are_known() -> bool {
    crate::util::home_dir().is_some_and(|path| path.is_absolute())
        || std::env::var_os("XDG_DATA_HOME").is_some_and(|path| Path::new(&path).is_absolute())
}

pub(crate) fn scope_roots_from(home: Option<&Path>, xdg: Option<&Path>) -> Option<Vec<PathBuf>> {
    let home = home.filter(|p| p.is_absolute());
    let xdg = xdg.filter(|p| p.is_absolute());
    if home.is_none() && xdg.is_none() {
        return None;
    }
    let mut roots = Vec::new();
    if let Some(home) = home {
        roots.push(home.to_path_buf());
        roots.push(home.join(".Trash"));
        roots.push(home.join(".local").join("share").join("Trash"));
    }
    if let Some(xdg) = xdg {
        roots.push(xdg.to_path_buf());
        roots.push(xdg.join("Trash"));
    }
    let mut forms = Vec::new();
    for root in roots {
        for form in scope_forms(&root) {
            if !forms.contains(&form) {
                forms.push(form);
            }
        }
    }
    Some(forms)
}

/// `path` as a scope root, and the same directory with symlinks resolved
/// when it exists. `put` records a file under a symlinked home by the
/// resolved directory, so both spellings are the same scope.
fn scope_forms(path: &Path) -> Vec<PathBuf> {
    let mut out = vec![match_key(path)];
    if let Ok(real) = std::fs::canonicalize(path) {
        let real = match_key(&crate::util::plain_path(&real));
        if real != out[0] {
            out.push(real);
        }
    }
    out
}

/// Whether `selected` holds every recorded original, or every original
/// under one scope root.
///
/// The keys are the paths `put` records. On Windows, and for a
/// Windows-shaped path, they are folded the way the Recycle Bin matcher
/// folds them. An exact operand is not passed here.
pub fn selection_wipes(selected: &[PathBuf], all: &[PathBuf]) -> bool {
    if selected.is_empty() || all.is_empty() {
        return false;
    }
    let sel: Vec<String> = selected.iter().map(|p| cover_key(p)).collect();
    let every: Vec<String> = all.iter().map(|p| cover_key(p)).collect();
    if every.iter().all(|k| sel.contains(k)) {
        return true;
    }
    for roots in coverage_root_groups(all) {
        let under: Vec<&String> = every
            .iter()
            .filter(|key| roots.iter().any(|root| key_under(key, root)))
            .collect();
        if !under.is_empty() && under.iter().all(|k| sel.contains(k)) {
            return true;
        }
    }
    false
}

fn cover_key(path: &Path) -> String {
    let s = path.to_string_lossy();
    if cfg!(windows) || windows_shaped(&s) {
        return crate::win_recycle::path_key(&s, &[]);
    }
    if s == "/" {
        return s.into();
    }
    s.trim_end_matches('/').to_string()
}

fn windows_shaped(s: &str) -> bool {
    let b = s.as_bytes();
    (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
        || s.starts_with(r"\\")
        || s.contains('\\')
}

fn key_under(path: &str, root: &str) -> bool {
    if path == root {
        return true;
    }
    if root == "/" {
        return path.starts_with('/');
    }
    if windows_shaped(root) || windows_shaped(path) {
        return crate::win_recycle::win_path_matches(
            crate::filters::Match::Prefix,
            path,
            std::slice::from_ref(&root.to_string()),
        );
    }
    let root = root.trim_end_matches('/');
    !root.is_empty() && path.starts_with(root) && path[root.len()..].starts_with('/')
}

fn coverage_root_groups(entries: &[PathBuf]) -> Vec<Vec<String>> {
    let mut groups = Vec::new();
    // A symlink or macOS firmlink can give one scope several spellings.
    // Coverage is over their union, not each spelling in isolation.
    let mut push = |roots: Vec<String>| {
        let roots: Vec<String> =
            roots
                .into_iter()
                .filter(|key| !key.is_empty())
                .fold(Vec::new(), |mut out, key| {
                    if !out.contains(&key) {
                        out.push(key);
                    }
                    out
                });
        if !roots.is_empty() && !groups.contains(&roots) {
            groups.push(roots);
        }
    };
    if let Some(home) = crate::util::home_dir() {
        let mut roots = vec![cover_key(&home)];
        if let Ok(real) = std::fs::canonicalize(&home) {
            roots.push(cover_key(&crate::util::plain_path(&real)));
        }
        push(roots);
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        let xdg = PathBuf::from(xdg);
        if xdg.is_absolute() {
            let mut roots = vec![cover_key(&xdg)];
            if let Ok(real) = std::fs::canonicalize(&xdg) {
                roots.push(cover_key(&crate::util::plain_path(&real)));
            }
            push(roots);
        }
    }
    if !cfg!(windows) {
        push(vec!["/".to_string()]);
    }
    for entry in entries {
        if let Some(vol) = volume_root_of(entry) {
            push(vec![vol]);
        }
    }
    groups
}

fn volume_root_of(path: &Path) -> Option<String> {
    let s = path.to_string_lossy();
    if cfg!(windows) || windows_shaped(&s) {
        let key = crate::win_recycle::path_key(&s, &[]);
        return drive_or_unc_root(&key);
    }
    s.starts_with('/').then(|| "/".to_string())
}

fn drive_or_unc_root(key: &str) -> Option<String> {
    let b = key.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return Some(format!("{}:\\", char::from(b[0])));
    }
    let rest = key.strip_prefix(r"\\")?;
    let mut parts = rest.split('\\').filter(|part| !part.is_empty());
    let server = parts.next()?;
    let share = parts.next()?;
    Some(format!(r"\\{server}\{share}"))
}

/// The key [`refuse_ambiguous`] groups by. Windows spellings that differ
/// only by case stay apart here: the Recycle Bin groups those with
/// [`refuse_ambiguous_keys`]. Everywhere else the key is [`match_key`].
fn ambiguity_key(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        path.to_path_buf()
    }
    #[cfg(not(windows))]
    {
        match_key(path)
    }
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
    let mut counts: BTreeMap<PathBuf, (usize, PathBuf)> = BTreeMap::new();
    for path in paths {
        let entry = counts
            .entry(ambiguity_key(path))
            .or_insert_with(|| (0, path.to_path_buf()));
        entry.0 += 1;
    }
    let ambiguous: Vec<(usize, PathBuf)> = counts.into_values().filter(|(n, _)| *n > 1).collect();
    if ambiguous.is_empty() {
        return false;
    }
    for (n, path) in ambiguous {
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

/// Apply rm's pattern safeguards before scanning (`selected` and `census`
/// empty) or after matching. Every backend calls this same decision.
pub fn refuse_pattern_selection(
    prog: &str,
    patterns: &crate::filters::Patterns,
    shown: &[String],
    force: bool,
    scoped: bool,
    selected: &[PathBuf],
    census: &[PathBuf],
) -> bool {
    if force || scoped {
        return false;
    }
    if patterns.is_mass() {
        if patterns.unknown_scope_needs_force() {
            refuse_unknown_scope(prog);
        } else {
            eprintln!(
                "{prog}: refusing mass pattern {:?} without --force \
                 (covers every name below a matched directory or every entry in a known scope)",
                shown
            );
            eprintln!("Try '{prog} --force …' only if you intend a full selective purge.");
        }
        return true;
    }
    if crate::filters::selection_covers_scope(patterns, selected, census) {
        eprintln!(
            "{prog}: refusing mass pattern {:?} without --force \
             (covers every name below a matched directory or every entry in a known scope)",
            shown
        );
        eprintln!("Try '{prog} --force …' only if you intend a full selective purge.");
        return true;
    }
    false
}

pub const UNKNOWN_SCOPE_RULE: &str = "When the scope is unknown, regex and substring matching and \
globs containing a path separator require --force; use an exact path, --id, or a basename glob \
instead.";

pub fn refuse_unknown_scope(prog: &str) {
    eprintln!("{prog}: {UNKNOWN_SCOPE_RULE}");
}

#[cfg(test)]
mod tests {
    /// Two originals whose parents are the same directory, one of them
    /// reached through a symlink. Path equality keeps the spellings apart;
    /// the match key the exact matcher uses does not.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_parent_is_the_same_original() {
        let root = std::env::temp_dir().join(format!("rtrash-match-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let real = root.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let via_real = real.join("a.txt");
        let via_link = link.join("a.txt");
        let refused =
            super::refuse_ambiguous("rtrash", [via_real.as_path(), via_link.as_path()], false);
        let _ = std::fs::remove_dir_all(&root);
        assert!(refused);
    }

    #[test]
    fn an_id_is_the_whole_selection_and_a_batch_needs_no_id() {
        assert!(super::chosen(true, false, false));
        assert!(!super::chosen(true, false, true));
        assert!(super::chosen(false, true, true));
        assert!(super::batch_takes_the_whole_run(true, true, true));
        assert!(!super::batch_takes_the_whole_run(true, true, false));
        assert!(!super::batch_takes_the_whole_run(true, false, true));
    }

    #[test]
    fn an_unknown_home_treats_every_prefix_as_a_scope_root() {
        assert!(super::scope_listed(
            &std::env::temp_dir().join("rtrash-unknown-scope"),
            None
        ));
        let home = std::env::temp_dir().join("rtrash-scope-home");
        let roots = super::scope_roots_from(Some(&home), None).unwrap();
        assert!(super::scope_listed(&super::match_key(&home), Some(&roots)));
        assert!(!super::scope_listed(
            &super::match_key(&home.join("proj")),
            Some(&roots)
        ));
    }

    #[test]
    fn windows_scope_roots_match_without_case() {
        let home = std::path::PathBuf::from(r"C:\Users\Alice");
        let roots = [home.clone()];
        assert!(super::windows_scope_same(
            &home,
            std::path::Path::new(r"c:\users\alice")
        ));
        assert!(super::windows_scope_same(
            &home,
            std::path::Path::new(r"C:/Users/Alice")
        ));
        assert!(!super::windows_scope_same(
            &home,
            std::path::Path::new(r"C:\Users\Alice2")
        ));
        assert!(!super::windows_scope_same(
            &home,
            std::path::Path::new(r"D:\Users\Alice")
        ));
        assert!(roots
            .iter()
            .any(|root| super::windows_scope_same(root, std::path::Path::new(r"c:\users\alice"))));
        assert!(super::scope_listed(
            std::path::Path::new(r"C:\anywhere"),
            None
        ));
    }

    #[test]
    fn an_xdg_data_home_is_a_scope_root_without_home() {
        let xdg = std::env::temp_dir().join("rtrash-scope-xdg");
        let roots = super::scope_roots_from(None, Some(&xdg)).expect("xdg is a root");
        assert!(super::scope_listed(&super::match_key(&xdg), Some(&roots)));
        assert!(!super::scope_listed(
            &super::match_key(&std::env::temp_dir().join("rtrash-other")),
            Some(&roots)
        ));
        assert!(super::scope_roots_from(None, None).is_none());
    }
}
