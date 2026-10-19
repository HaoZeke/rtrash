//! Filters for `rtrash du`: `--exclude`, `--exclude-from`, `--filter-from`,
//! `--exclude-caches` and `--gitignore`.
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

/// [`read_text`], with `-` read from standard input.
fn read_text_or_stdin(path: &Path) -> io::Result<String> {
    if path.as_os_str() != "-" {
        return read_text(path);
    }
    let mut b = Vec::new();
    io::Read::read_to_end(&mut io::stdin(), &mut b)?;
    Ok(crate::util::str_from_bytes(b))
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

/// One rule of an rsync filter file (`--filter-from`): `- PATTERN`
/// leaves out what it matches, `+ PATTERN` keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RsyncRule {
    exclude: bool,
    pat: Pattern,
}

/// The rsync rule `pattern` as a gitignore-style [`Pattern`]. A leading
/// `/` anchors it at the scanned PATH, as both read it. A pattern with
/// another `/` matches the end of the path in rsync, so it gains a
/// leading `**/`. A leading `!` or `#` is a literal character there.
fn rsync_pattern(pattern: &str) -> Option<Pattern> {
    let inner = pattern.trim_end_matches('/');
    let text = if !pattern.starts_with('/') && inner.contains('/') {
        format!("**/{pattern}")
    } else if pattern.starts_with(['!', '#']) {
        format!("\\{pattern}")
    } else {
        pattern.to_string()
    };
    Pattern::parse(&text)
}

/// Parse an rsync filter file, rules in file order. Each line is
/// `- PATTERN` or `exclude PATTERN`, `+ PATTERN` or `include PATTERN`,
/// `H` / `hide` as `-`, `S` / `show` as `+`, or `!` / `clear`, which drops
/// the rules above it. `P`, `R`, `protect` and `risk` only guard files on
/// the receiving side, so du skips them. Blank lines and lines starting
/// with `#` or `;` are comments. A merge rule, a rule with modifiers, or
/// any other line is an error that names its line.
pub fn rsync_rules(text: &str) -> Result<Vec<RsyncRule>, String> {
    let mut rules = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        let (word, pattern) = match line.split_once([' ', '_']) {
            Some((w, p)) => (w, Some(p)),
            None => (line, None),
        };
        let exclude = match (word, pattern) {
            ("!" | "clear", None) => {
                rules.clear();
                continue;
            }
            ("P" | "R" | "protect" | "risk", Some(_)) => continue,
            ("-" | "exclude" | "H" | "hide", Some(_)) => true,
            ("+" | "include" | "S" | "show", Some(_)) => false,
            _ => {
                return Err(format!(
                    "line {}: '{line}' is not a rule du reads (+, -, H, S, P, R or !, \
                     without modifiers or merge files)",
                    n + 1
                ))
            }
        };
        let pattern = pattern.unwrap_or_default();
        // `dir/***` matches the folder and everything in it.
        let pats = match pattern.strip_suffix("/***") {
            Some(dir) if !dir.is_empty() => vec![dir.to_string(), format!("{dir}/**")],
            _ => vec![pattern.to_string()],
        };
        for text in pats {
            let Some(pat) = rsync_pattern(&text) else {
                return Err(format!("line {}: '{line}' has no pattern", n + 1));
            };
            rules.push(RsyncRule { exclude, pat });
        }
    }
    Ok(rules)
}

/// Read an rsync filter file (`--filter-from`; `-` is stdin).
pub fn read_rsync_file(path: &Path) -> io::Result<Result<Vec<RsyncRule>, String>> {
    let text = read_text_or_stdin(path)?;
    Ok(rsync_rules(&text))
}

/// What the command line asked to leave out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Spec {
    /// `--exclude` and `--exclude-from` lines, in order.
    pub patterns: Vec<String>,
    /// `--filter-from` rules, in file order: the first that matches
    /// decides, as in rsync.
    pub rsync: Vec<RsyncRule>,
    pub exclude_caches: bool,
    pub gitignore: bool,
    /// `--exclude-regex`: entries whose path matches one are left out.
    pub regex_out: Vec<String>,
    /// `--include-regex`: when given, files whose path matches none are
    /// left out. Directories are still walked.
    pub regex_in: Vec<String>,
    /// Directory that anchored patterns are relative to, when it is not
    /// the scanned PATH (the explorer rescans subdirectories).
    pub base: Option<PathBuf>,
}

impl Spec {
    pub fn is_active(&self) -> bool {
        !self.patterns.is_empty()
            || !self.rsync.is_empty()
            || self.exclude_caches
            || self.gitignore
            || !self.regex_out.is_empty()
            || !self.regex_in.is_empty()
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
    let text = read_text_or_stdin(path)?;
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
    /// `--filter-from` rules and the folder they are relative to.
    rsync: (PathBuf, Vec<RsyncRule>),
    pub exclude_caches: bool,
    pub gitignore: bool,
    /// Ignore rules from the repository that encloses the scan root.
    pub root_chain: Option<Arc<Chain>>,
    /// Git ignores the scan root, or a directory above it in its
    /// repository, so `--gitignore` leaves out all it holds.
    root_ignored: bool,
    regex_out: Vec<regex_lite::Regex>,
    regex_in: Vec<regex_lite::Regex>,
}

/// Compiles the `--exclude-regex` or `--include-regex` value `re`, for
/// the parser to check once and the filters to build again.
pub fn path_regex(re: &str) -> Result<regex_lite::Regex, String> {
    regex_lite::Regex::new(re).map_err(|e| format!("invalid regex '{re}': {e}"))
}

fn compile(list: &[String]) -> Vec<regex_lite::Regex> {
    list.iter().filter_map(|r| path_regex(r).ok()).collect()
}

impl Filter {
    pub fn new(spec: &Spec, root: &Path) -> Filter {
        let enclosing = spec.gitignore.then(|| ignored_from_above(root)).flatten();
        let root_ignored = enclosing.as_ref().is_some_and(|e| e.1);
        let root_chain = enclosing.map(|e| e.0);
        let base = spec.base.as_deref().unwrap_or(root).to_path_buf();
        Filter {
            cli: spec.rules_for(root),
            rsync: (base, spec.rsync.clone()),
            exclude_caches: spec.exclude_caches,
            gitignore: spec.gitignore,
            root_chain,
            root_ignored,
            regex_out: compile(&spec.regex_out),
            regex_in: compile(&spec.regex_in),
        }
    }

    /// Whether entry `name` of directory `dir` is left out.
    pub fn excluded(&self, chain: Option<&Chain>, dir: &Path, name: &[u8], is_dir: bool) -> bool {
        if self.root_ignored {
            return true;
        }
        if !self.cli.is_empty() && self.cli.decide(dir, name, is_dir) == Some(true) {
            return true;
        }
        if self.rsync_excludes(dir, name, is_dir) {
            return true;
        }
        if !self.regex_out.is_empty() || (!is_dir && !self.regex_in.is_empty()) {
            let full = dir.join(String::from_utf8_lossy(name).as_ref());
            let full = full.to_string_lossy();
            if self.regex_out.iter().any(|r| r.is_match(&full)) {
                return true;
            }
            let kept = self.regex_in.iter().any(|r| r.is_match(&full));
            if !is_dir && !self.regex_in.is_empty() && !kept {
                return true;
            }
        }
        match chain {
            Some(c) => c.decide(dir, name, is_dir) == Some(true),
            None => false,
        }
    }

    /// Whether the first `--filter-from` rule that matches leaves the
    /// entry out. A `+` rule keeps it from the later rules of the file,
    /// not from the other filters.
    fn rsync_excludes(&self, dir: &Path, name: &[u8], is_dir: bool) -> bool {
        let (base, rules) = &self.rsync;
        let rel = || rel_segments(base, dir, name);
        rules
            .iter()
            .find(|r| r.pat.matches(name, &rel, is_dir))
            .is_some_and(|r| r.exclude)
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

/// The global ignore file git reads for repository `repo`: the
/// `core.excludesFile` its config sets, asked of git itself, else the
/// default `$XDG_CONFIG_HOME/git/ignore`, or `~/.config/git/ignore` when
/// that variable is unset or empty. Without git only the default counts.
/// A relative path is taken from `repo`, as git takes it from the top of
/// the work tree, and an empty value means none.
fn global_ignore(repo: &Path) -> Option<String> {
    let path = match crate::du_git::config_path(repo, "core.excludesFile") {
        Some(Some(v)) if v.is_empty() => return None,
        Some(Some(v)) => repo.join(v),
        Some(None) | None => repo.join(default_global_ignore()?),
    };
    read_text(&path).ok()
}

fn default_global_ignore() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::util::home_dir().map(|h| h.join(".config")))?;
    Some(base.join("git").join("ignore"))
}

/// The directory whose `info/exclude` applies to `repo`: its `.git`, or,
/// when `.git` is a file as in a linked worktree or a submodule, the git
/// directory that file names, and from there the common directory that
/// worktrees share.
fn exclude_dir(repo: &Path) -> PathBuf {
    let dot = repo.join(".git");
    let from = |base: &Path, text: &str| {
        let p = PathBuf::from(text.trim_end_matches(['\n', '\r']));
        if p.is_relative() {
            base.join(p)
        } else {
            p
        }
    };
    let Ok(text) = fs::read_to_string(&dot) else {
        return dot;
    };
    let Some(gitdir) = text.lines().next().and_then(|l| l.strip_prefix("gitdir:")) else {
        return dot;
    };
    let gitdir = from(repo, gitdir.trim_start());
    match fs::read_to_string(gitdir.join("commondir")) {
        Ok(common) => from(gitdir.as_path(), common.as_str()),
        Err(_) => gitdir,
    }
}

/// The rules every path in repository `repo` starts from, lowest first as
/// git ranks them: the global ignore file, then `info/exclude`. Each
/// `.gitignore` then goes on top.
fn repo_base_chain(repo: &Path) -> Arc<Chain> {
    let mut chain = Arc::new(Chain::default());
    let info = read_text(&exclude_dir(repo).join("info").join("exclude")).ok();
    for text in [global_ignore(repo), info].into_iter().flatten() {
        let r = Rules::new(repo.to_path_buf(), &text);
        if !r.is_empty() {
            chain = Chain::push(Some(chain), r);
        }
    }
    chain
}

/// Rules from `repo` down to the parent of `root`, for a scan that starts
/// inside a repository, and whether those rules leave `root` out: it, or
/// a directory on the way to it, is excluded by the rules above it. Git
/// never opens an excluded directory, so no rule below one, not even a
/// `!` pattern, brings back what is inside (`build/` then
/// `!build/keep.txt` keeps nothing).
fn enclosing_chain(repo: &Path, root: &Path) -> (Arc<Chain>, bool) {
    let mut chain = repo_base_chain(repo);
    let names: Vec<_> = root
        .strip_prefix(repo)
        .map(|r| r.components().map(|c| c.as_os_str().to_owned()).collect())
        .unwrap_or_default();
    let push_dir = |chain: Arc<Chain>, d: &Path| match read_text(&d.join(".gitignore")) {
        Ok(text) => {
            let r = Rules::new(d.to_path_buf(), &text);
            if r.is_empty() {
                chain
            } else {
                Chain::push(Some(chain), r)
            }
        }
        Err(_) => chain,
    };
    if names.is_empty() {
        return (push_dir(chain, repo), false);
    }
    // Each directory from `repo` down to the parent of `root` adds its
    // ignore file, then decides on the next name on the way.
    let mut dir = repo.to_path_buf();
    let mut left_out = false;
    for (i, name) in names.iter().enumerate() {
        chain = push_dir(chain, &dir);
        let is_dir = i + 1 < names.len() || root.is_dir();
        left_out |= chain.decide(&dir, name.as_encoded_bytes(), is_dir) == Some(true);
        dir.push(name);
    }
    (chain, left_out)
}

/// The rules above `root` in its repository and whether they leave
/// `root` out, or `None` outside any repository. A `root` that is a
/// repository of its own starts afresh, so nothing above it counts.
fn ignored_from_above(root: &Path) -> Option<(Arc<Chain>, bool)> {
    let repo = crate::du_git::enclosing_repo(root)?;
    let (chain, left_out) = enclosing_chain(&repo, root);
    let own_repo = root.join(".git").symlink_metadata().is_ok();
    Some((chain, left_out && !own_repo))
}

/// Whether `--gitignore` leaves out all of `root`: git ignores it, or a
/// directory above it in its repository.
pub fn git_ignores(root: &Path) -> bool {
    ignored_from_above(root).is_some_and(|e| e.1)
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

    #[test]
    fn rsync_rules_read_as_patterns() {
        let r = rsync_rules("# c\n; c\n\n+ keep.log\nexclude *.log\n- /t/***\nP x\n").unwrap();
        assert_eq!(r.len(), 4, "{r:?}");
        assert!(!r[0].exclude);
        assert!(r[1].exclude);
        assert_eq!(r[2].pat, pat("/t"));
        assert_eq!(r[3].pat, pat("/t/**"));
        assert_eq!(rsync_rules("- a/b/").unwrap()[0].pat, pat("**/a/b/"));
        assert_eq!(rsync_rules("H !x").unwrap()[0].pat, pat("\\!x"));
        assert_eq!(rsync_rules("- a\n!\nS b").unwrap().len(), 1);
        for bad in [". m", ": .f", "-! x", "merge f", "-", "+ ", "x y"] {
            let e = rsync_rules(bad).unwrap_err();
            assert!(e.starts_with("line 1: "), "{bad}: {e}");
        }
    }

    #[test]
    fn the_first_rsync_rule_that_matches_decides() {
        let spec = Spec {
            rsync: rsync_rules("+ keep.log\n- *.log\n- a/b/\n").unwrap(),
            ..Default::default()
        };
        let f = Filter::new(&spec, Path::new("/b"));
        let dir = Path::new("/b/x/a");
        assert!(!f.excluded(None, dir, b"keep.log", false));
        assert!(f.excluded(None, dir, b"skip.log", false));
        assert!(f.excluded(None, dir, b"b", true));
        assert!(!f.excluded(None, dir, b"b", false));
        assert!(!f.excluded(None, Path::new("/b/x"), b"b", true));
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

    /// No `!` rule brings back what an excluded directory holds, so a
    /// scan root below one is left out whole; a repository of its own
    /// there starts afresh, as git sees it.
    #[test]
    fn a_root_below_an_excluded_directory_is_left_out() {
        let d = std::env::temp_dir().join(format!("rtrash-filter-above-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        for sub in [
            ".git",
            "build/keep",
            "build/lib/.git",
            "sub/deep",
            "sub/other",
        ] {
            fs::create_dir_all(d.join(sub)).unwrap();
        }
        let rules = "build/\n!build/keep/\n/sub/*\n!/sub/deep/\n";
        fs::write(d.join(".gitignore"), rules).unwrap();
        assert!(git_ignores(&d.join("build")));
        assert!(git_ignores(&d.join("build/keep")));
        assert!(!git_ignores(&d.join("build/lib")));
        assert!(git_ignores(&d.join("sub/other")));
        assert!(!git_ignores(&d.join("sub/deep")));
        assert!(!git_ignores(&d.join("sub")));
        let spec = Spec {
            gitignore: true,
            ..Spec::default()
        };
        let f = Filter::new(&spec, &d.join("build/keep"));
        assert!(f.excluded(None, &d.join("build/keep"), b"a.txt", false));
        let f = Filter::new(&spec, &d.join("sub/deep"));
        assert!(!f.excluded(None, &d.join("sub/deep"), b"a.txt", false));
        let _ = fs::remove_dir_all(&d);
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
