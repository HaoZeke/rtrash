//! Filters for `rtrash du`: `--exclude`, `--exclude-from`, `--exclude-caches`
//! and `--gitignore`.
//!
//! Patterns use gitignore syntax. A pattern without a `/` (other than a
//! trailing one) matches a name at any depth; one with a `/` matches the
//! path relative to its base: the scanned PATH for `--exclude` and
//! `--exclude-from`, the directory holding the ignore file for
//! `.gitignore`. A trailing `/` matches directories only, `!` re-includes,
//! `*`, `?` and `[...]` stop at `/`, and `**` spans directories.
//!
//! Excluded entries are skipped: they add nothing to any size or count,
//! and an excluded directory is never opened.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// The signature line every valid `CACHEDIR.TAG` starts with
/// (<https://bford.info/cachedir/>).
pub const CACHEDIR_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";

/// One gitignore-style pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    /// Segments split on `/`; one segment for a pattern that matches names.
    segs: Vec<Vec<u8>>,
    negated: bool,
    dir_only: bool,
    /// Matches the path relative to the base rather than a name.
    anchored: bool,
}

impl Pattern {
    /// Parse one line. Blank lines and `#` comments give `None`.
    pub fn parse(line: &str) -> Option<Pattern> {
        let mut s = line.trim_end_matches(['\r', '\n']);
        // Trailing spaces are dropped unless escaped with a backslash.
        while s.ends_with(' ') && !s.ends_with("\\ ") {
            s = &s[..s.len() - 1];
        }
        if s.is_empty() || s.starts_with('#') {
            return None;
        }
        let mut negated = false;
        if let Some(rest) = s.strip_prefix('!') {
            negated = true;
            s = rest;
        } else if s.starts_with("\\!") || s.starts_with("\\#") {
            s = &s[1..];
        }
        let mut dir_only = false;
        while let Some(rest) = s.strip_suffix('/') {
            dir_only = true;
            s = rest;
        }
        if s.is_empty() {
            return None;
        }
        let anchored = s.contains('/');
        let s = s.strip_prefix('/').unwrap_or(s);
        let mut segs: Vec<Vec<u8>> = s
            .split('/')
            .filter(|p| !p.is_empty())
            .map(pattern_bytes)
            .collect();
        // `**/name` is the same as `name`.
        if segs.len() == 2 && segs[0] == b"**" && segs[1] != b"**" {
            segs.remove(0);
            return Some(Pattern {
                segs,
                negated,
                dir_only,
                anchored: false,
            });
        }
        if segs.is_empty() {
            return None;
        }
        Some(Pattern {
            segs,
            negated,
            dir_only,
            anchored,
        })
    }

    /// Whether this pattern matches `name` (relative path `rel`, given as
    /// segments, when anchored).
    fn matches(&self, name: &[u8], rel: &dyn Fn() -> Vec<Vec<u8>>, is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        if !self.anchored {
            return seg_match(&self.segs[0], name);
        }
        let rel = rel();
        let rel: Vec<&[u8]> = rel.iter().map(Vec::as_slice).collect();
        path_match(&self.segs, &rel)
    }
}

/// The bytes a pattern segment matches. On Unix a stray byte, kept in an
/// argument or ignore file as a private-use character, stands for that
/// raw byte again, so it matches names that are not UTF-8.
fn pattern_bytes(seg: &str) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        crate::util::os_from_str(seg).into_vec()
    }
    #[cfg(not(unix))]
    {
        seg.as_bytes().to_vec()
    }
}

/// Read an ignore or pattern file. Bytes that are not UTF-8 stay as they
/// are, so one stray byte neither drops the whole file nor any pattern.
fn read_text(path: &Path) -> io::Result<String> {
    fs::read(path).map(crate::util::str_from_bytes)
}

/// Match path segments against pattern segments, `**` spanning zero or
/// more of them. A trailing `**` needs at least one segment (`dir/**`
/// matches what is inside `dir`, not `dir`).
fn path_match(pat: &[Vec<u8>], path: &[&[u8]]) -> bool {
    match pat.split_first() {
        None => path.is_empty(),
        Some((first, rest)) if first.as_slice() == b"**" => {
            if rest.is_empty() {
                return !path.is_empty();
            }
            (0..=path.len()).any(|i| path_match(rest, &path[i..]))
        }
        Some((first, rest)) => match path.split_first() {
            Some((p, prest)) => seg_match(first, p) && path_match(rest, prest),
            None => false,
        },
    }
}

/// Glob match of one segment: `*`, `?`, `[...]` (with `!` or `^` to
/// negate and `a-z` ranges) and `\` escapes.
pub fn seg_match(pat: &[u8], text: &[u8]) -> bool {
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < text.len() {
        if pi < pat.len() {
            match pat[pi] {
                b'*' => {
                    star = Some((pi, ti));
                    pi += 1;
                    continue;
                }
                b'?' => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                b'[' => {
                    if let Some((used, ok)) = class(&pat[pi..], text[ti]) {
                        if ok {
                            pi += used;
                            ti += 1;
                            continue;
                        }
                    } else if text[ti] == b'[' {
                        pi += 1;
                        ti += 1;
                        continue;
                    }
                }
                b'\\' if pi + 1 < pat.len() => {
                    if pat[pi + 1] == text[ti] {
                        pi += 2;
                        ti += 1;
                        continue;
                    }
                }
                c if c == text[ti] => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                _ => {}
            }
        }
        match &mut star {
            Some((sp, st)) => {
                *st += 1;
                pi = *sp + 1;
                ti = *st;
            }
            None => return false,
        }
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

/// A `[...]` class at the start of `pat`: bytes used and whether `ch` is
/// in it, or `None` when the class never closes.
fn class(pat: &[u8], ch: u8) -> Option<(usize, bool)> {
    let mut i = 1;
    let negated = matches!(pat.get(1), Some(b'!') | Some(b'^'));
    if negated {
        i += 1;
    }
    let mut hit = false;
    let mut first = true;
    loop {
        let c = *pat.get(i)?;
        if c == b']' && !first {
            return Some((i + 1, hit != negated));
        }
        first = false;
        let (lo, used) = if c == b'\\' {
            (*pat.get(i + 1)?, 2)
        } else {
            (c, 1)
        };
        i += used;
        if pat.get(i) == Some(&b'-') && pat.get(i + 1).is_some_and(|&n| n != b']') {
            let hi = pat[i + 1];
            i += 2;
            if lo <= ch && ch <= hi {
                hit = true;
            }
        } else if lo == ch {
            hit = true;
        }
    }
}

/// Patterns that share a base directory, in file order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rules {
    base: PathBuf,
    pats: Vec<Pattern>,
}

impl Rules {
    pub fn new(base: PathBuf, text: &str) -> Rules {
        Rules {
            base,
            pats: text.lines().filter_map(Pattern::parse).collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.pats.is_empty()
    }

    /// The decision of the last pattern that matches, if any:
    /// `Some(true)` excludes, `Some(false)` re-includes.
    fn decide(&self, dir: &Path, name: &[u8], is_dir: bool) -> Option<bool> {
        let rel = || rel_segments(&self.base, dir, name);
        self.pats
            .iter()
            .rev()
            .find(|p| p.matches(name, &rel, is_dir))
            .map(|p| !p.negated)
    }
}

/// Segments of `dir/name` relative to `base`.
fn rel_segments(base: &Path, dir: &Path, name: &[u8]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = dir
        .strip_prefix(base)
        .map(|r| {
            r.components()
                .filter_map(|c| match c {
                    Component::Normal(s) => Some(s.as_encoded_bytes().to_vec()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    out.push(name.to_vec());
    out
}

/// Ignore files in effect for one directory, deepest last. A chain
/// exists only inside a repository; it may hold no rules at all.
#[derive(Debug, Default)]
pub struct Chain {
    rules: Option<Arc<Rules>>,
    parent: Option<Arc<Chain>>,
}

impl Chain {
    /// `parent` with `rules` added on top.
    pub fn push(parent: Option<Arc<Chain>>, rules: Rules) -> Arc<Chain> {
        Arc::new(Chain {
            rules: Some(Arc::new(rules)),
            parent,
        })
    }

    /// The deepest ignore file that has an opinion decides.
    fn decide(&self, dir: &Path, name: &[u8], is_dir: bool) -> Option<bool> {
        let mut cur = Some(self);
        while let Some(c) = cur {
            if let Some(r) = &c.rules {
                if let Some(d) = r.decide(dir, name, is_dir) {
                    return Some(d);
                }
            }
            cur = c.parent.as_deref();
        }
        None
    }
}

/// What the command line asked to leave out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Spec {
    /// `--exclude` and `--exclude-from` lines, in order.
    pub patterns: Vec<String>,
    pub exclude_caches: bool,
    pub gitignore: bool,
    /// Directory that anchored patterns are relative to, when it is not
    /// the scanned PATH (the explorer rescans subdirectories).
    pub base: Option<PathBuf>,
}

impl Spec {
    pub fn is_active(&self) -> bool {
        !self.patterns.is_empty() || self.exclude_caches || self.gitignore
    }

    /// Patterns relative to `root`. An absolute pattern under `root` is
    /// made relative to it, so `--exclude=$PWD/build` works.
    pub fn rules_for(&self, root: &Path) -> Rules {
        let root = self.base.as_deref().unwrap_or(root);
        let root_s = root.to_string_lossy();
        let root_s = root_s.trim_end_matches(['/', '\\']);
        let mut text = String::new();
        for p in &self.patterns {
            let (neg, body) = match p.strip_prefix('!') {
                Some(b) => ("!", b),
                None => ("", p.as_str()),
            };
            let body = match body.strip_prefix(root_s) {
                Some(rest) if !root_s.is_empty() && rest.starts_with(['/', '\\']) => {
                    rest.replace('\\', "/")
                }
                _ => body.to_string(),
            };
            text.push_str(neg);
            text.push_str(&body);
            text.push('\n');
        }
        Rules::new(root.to_path_buf(), &text)
    }
}

/// The extensions of a `--type` or `--exclude-type` value such as
/// `yaml,json` or `.iso`, each without its dot.
pub fn extensions(list: &str, flag: &str) -> Result<Vec<String>, String> {
    let exts: Vec<String> = list
        .split(',')
        .map(|e| e.trim().trim_start_matches('.').to_string())
        .collect();
    if exts.iter().any(|e| e.is_empty() || e.contains(['/', '\\'])) {
        return Err(format!("{flag}: invalid extension list '{list}'"));
    }
    Ok(exts)
}

/// Patterns that leave out every file except those ending in one of
/// `exts`, while keeping directories so their contents are still walked.
pub fn keep_only(exts: &[String]) -> Vec<String> {
    let mut out = vec!["*".to_string(), "!*/".to_string()];
    out.extend(exts.iter().map(|e| format!("!*.{e}")));
    out
}

/// Read `--exclude-from` lines (gitignore syntax; `-` is stdin).
pub fn read_pattern_file(path: &Path) -> io::Result<Vec<String>> {
    let text = if path.as_os_str() == "-" {
        let mut b = Vec::new();
        io::Read::read_to_end(&mut io::stdin(), &mut b)?;
        crate::util::str_from_bytes(b)
    } else {
        read_text(path)?
    };
    Ok(text
        .lines()
        .filter(|l| Pattern::parse(l).is_some())
        .map(str::to_string)
        .collect())
}

/// The filters of one scan root.
#[derive(Debug)]
pub struct Filter {
    cli: Rules,
    pub exclude_caches: bool,
    pub gitignore: bool,
    /// Ignore rules from the repository that encloses the scan root.
    pub root_chain: Option<Arc<Chain>>,
}

impl Filter {
    pub fn new(spec: &Spec, root: &Path) -> Filter {
        let root_chain = if spec.gitignore {
            crate::du_git::enclosing_repo(root).map(|repo| enclosing_chain(&repo, root))
        } else {
            None
        };
        Filter {
            cli: spec.rules_for(root),
            exclude_caches: spec.exclude_caches,
            gitignore: spec.gitignore,
            root_chain,
        }
    }

    /// Whether entry `name` of directory `dir` is left out.
    pub fn excluded(&self, chain: Option<&Chain>, dir: &Path, name: &[u8], is_dir: bool) -> bool {
        if !self.cli.is_empty() && self.cli.decide(dir, name, is_dir) == Some(true) {
            return true;
        }
        match chain {
            Some(c) => c.decide(dir, name, is_dir) == Some(true),
            None => false,
        }
    }

    /// The chain for directory `dir`, given its parent's chain and
    /// whether it holds a `.git` and a `.gitignore`. A repository starts
    /// afresh: its parent's ignore files do not reach into it.
    pub fn chain_for(
        &self,
        parent: Option<&Arc<Chain>>,
        dir: &Path,
        has_git: bool,
        has_gitignore: bool,
    ) -> Option<Arc<Chain>> {
        if !self.gitignore {
            return None;
        }
        let mut chain = if has_git {
            repo_base_chain(dir)
        } else {
            // Outside a repository ignore files mean nothing.
            parent?.clone()
        };
        if has_gitignore {
            if let Ok(text) = read_text(&dir.join(".gitignore")) {
                let r = Rules::new(dir.to_path_buf(), &text);
                if !r.is_empty() {
                    chain = Chain::push(Some(chain), r);
                }
            }
        }
        Some(chain)
    }
}

/// The user's global ignore file at its default place
/// (`$XDG_CONFIG_HOME/git/ignore`).
fn global_ignore() -> Option<String> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| crate::util::home_dir().map(|h| h.join(".config")))?;
    read_text(&base.join("git").join("ignore")).ok()
}

/// The rules every path in repository `repo` starts from: the global
/// ignore file, then `.git/info/exclude`.
fn repo_base_chain(repo: &Path) -> Arc<Chain> {
    let mut chain = Arc::new(Chain::default());
    let info = read_text(&repo.join(".git").join("info").join("exclude")).ok();
    for text in [global_ignore(), info].into_iter().flatten() {
        let r = Rules::new(repo.to_path_buf(), &text);
        if !r.is_empty() {
            chain = Chain::push(Some(chain), r);
        }
    }
    chain
}

/// Rules from `repo` down to the parent of `root`, for a scan that starts
/// inside a repository.
fn enclosing_chain(repo: &Path, root: &Path) -> Arc<Chain> {
    let mut chain = repo_base_chain(repo);
    let mut dir = repo.to_path_buf();
    let rel: Vec<_> = root
        .parent()
        .and_then(|p| p.strip_prefix(repo).ok())
        .map(|r| r.components().collect())
        .unwrap_or_default();
    let mut steps = vec![dir.clone()];
    for c in rel {
        dir.push(c.as_os_str());
        steps.push(dir.clone());
    }
    for d in steps {
        if let Ok(text) = read_text(&d.join(".gitignore")) {
            let r = Rules::new(d, &text);
            if !r.is_empty() {
                chain = Chain::push(Some(chain), r);
            }
        }
    }
    chain
}

/// Whether `dir` holds a valid `CACHEDIR.TAG`.
pub fn has_cachedir_tag(dir: &Path) -> bool {
    use io::Read;
    let mut buf = [0u8; CACHEDIR_SIGNATURE.len()];
    fs::File::open(dir.join("CACHEDIR.TAG"))
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok_and(|()| buf == CACHEDIR_SIGNATURE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_lists_drop_dots_and_refuse_blanks() {
        assert_eq!(
            extensions("yaml, .json", "--type").unwrap(),
            ["yaml", "json"]
        );
        assert!(extensions("iso,", "--type").is_err());
        assert!(extensions("a/b", "--exclude-type").is_err());
        assert_eq!(keep_only(&["iso".into()]), ["*", "!*/", "!*.iso"]);
    }

    fn pat(s: &str) -> Pattern {
        Pattern::parse(s).unwrap()
    }

    fn hit(rules: &str, rel: &str, is_dir: bool) -> Option<bool> {
        let r = Rules::new(PathBuf::from("/b"), rules);
        let p = Path::new("/b").join(rel);
        let dir = p.parent().unwrap();
        let name = p.file_name().unwrap().as_encoded_bytes();
        r.decide(dir, name, is_dir)
    }

    #[cfg(unix)]
    #[test]
    fn stray_bytes_in_patterns_match_raw_names() {
        use std::os::unix::ffi::OsStrExt;
        // An argument or ignore line with byte 0xE9 keeps it as a
        // private-use character; the pattern must match the raw byte.
        let line = crate::util::str_from_bytes(b"n\xe9e*".to_vec());
        let r = Rules::new(PathBuf::from("/b"), &line);
        let raw = std::ffi::OsStr::from_bytes(b"n\xe9e menu");
        assert_eq!(r.decide(Path::new("/b"), raw.as_bytes(), false), Some(true));
        let other = std::ffi::OsStr::from_bytes("née menu".as_bytes());
        assert_eq!(r.decide(Path::new("/b"), other.as_bytes(), false), None);
        // An anchored pattern with a stray byte in a directory segment.
        let line = crate::util::str_from_bytes(b"d\xff/*.log".to_vec());
        let r = Rules::new(PathBuf::from("/b"), &line);
        let dir = Path::new("/b").join(std::ffi::OsStr::from_bytes(b"d\xff"));
        assert_eq!(r.decide(&dir, b"x.log", false), Some(true));
    }

    #[test]
    fn ignore_file_with_a_stray_byte_keeps_every_rule() {
        let dir = std::env::temp_dir().join(format!("rtrash-filter-bytes-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("ignore");
        fs::write(&f, b"*.tmp\nn\xe9e\n").unwrap();
        let lines = read_pattern_file(&f).unwrap();
        assert_eq!(lines.len(), 2, "{lines:?}");
        let r = Rules::new(PathBuf::from("/b"), &lines.join("\n"));
        assert_eq!(r.decide(Path::new("/b"), b"a.tmp", false), Some(true));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn segment_globs() {
        assert!(seg_match(b"*.o", b"a.o"));
        assert!(!seg_match(b"*.o", b"a.oo"));
        assert!(seg_match(b"a?c", b"abc"));
        assert!(seg_match(b"[a-c]x", b"bx"));
        assert!(!seg_match(b"[!a-c]x", b"bx"));
        assert!(seg_match(b"[^a-c]x", b"dx"));
        assert!(seg_match(b"\\*", b"*"));
        assert!(!seg_match(b"\\*", b"a"));
        assert!(seg_match(b"[]]", b"]"));
        assert!(seg_match(b"[", b"["));
        assert!(seg_match(b"*", b""));
        assert!(seg_match(b"**", b"anything"));
    }

    #[test]
    fn parse_lines() {
        assert_eq!(Pattern::parse("# c"), None);
        assert_eq!(Pattern::parse("   "), None);
        assert_eq!(Pattern::parse("/"), None);
        let p = pat("build/");
        assert!(p.dir_only && !p.anchored);
        let p = pat("/build");
        assert!(p.anchored && p.segs == [b"build".to_vec()]);
        let p = pat("!keep.log");
        assert!(p.negated);
        let p = pat("\\#x");
        assert_eq!(p.segs, [b"#x".to_vec()]);
        let p = pat("a\\ ");
        assert_eq!(p.segs, [b"a\\ ".to_vec()]);
        let p = pat("**/foo");
        assert!(!p.anchored);
    }

    #[test]
    fn gitignore_semantics() {
        assert_eq!(hit("*.log", "x/y/a.log", false), Some(true));
        assert_eq!(hit("*.log\n!keep.log", "keep.log", false), Some(false));
        assert_eq!(hit("build/", "x/build", true), Some(true));
        assert_eq!(hit("build/", "x/build", false), None);
        assert_eq!(hit("/build", "build", true), Some(true));
        assert_eq!(hit("/build", "x/build", true), None);
        assert_eq!(hit("doc/*.txt", "doc/a.txt", false), Some(true));
        assert_eq!(hit("doc/*.txt", "doc/x/a.txt", false), None);
        assert_eq!(hit("doc/**/*.txt", "doc/x/y/a.txt", false), Some(true));
        assert_eq!(hit("doc/**/*.txt", "doc/a.txt", false), Some(true));
        assert_eq!(hit("a/**", "a", true), None);
        assert_eq!(hit("a/**", "a/b", false), Some(true));
        assert_eq!(hit("**/logs/x", "q/logs/x", false), Some(true));
        assert_eq!(hit("**/logs", "q/r/logs", true), Some(true));
    }

    #[test]
    fn deeper_ignore_files_win() {
        let top = Chain::push(None, Rules::new(PathBuf::from("/r"), "*.dat"));
        let sub = Chain::push(Some(top), Rules::new(PathBuf::from("/r/s"), "!big.dat"));
        assert_eq!(
            sub.decide(Path::new("/r/s"), b"big.dat", false),
            Some(false)
        );
        assert_eq!(
            sub.decide(Path::new("/r/s"), b"other.dat", false),
            Some(true)
        );
    }

    #[test]
    fn absolute_cli_patterns_become_relative() {
        let spec = Spec {
            patterns: vec!["/scan/root/build".into(), "!/scan/root/keep".into()],
            ..Spec::default()
        };
        let r = spec.rules_for(Path::new("/scan/root"));
        assert_eq!(
            r.decide(Path::new("/scan/root"), b"build", true),
            Some(true)
        );
        assert_eq!(
            r.decide(Path::new("/scan/root"), b"keep", true),
            Some(false)
        );
        assert_eq!(r.decide(Path::new("/scan/root/x"), b"build", true), None);
    }
}
