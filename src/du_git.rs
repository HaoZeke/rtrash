//! Git state for `rtrash du` rows: is a listed path ignored, tracked in a
//! clean repo, or does it hold uncommitted, stashed or unpushed work?
//! Uses the `git` binary; no library dependency.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

/// Ordered from safest to riskiest; a row shows the riskiest state found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GitState {
    /// Inside a repo and matched by an ignore rule.
    Ignored,
    /// Tracked or untracked content of a repo with nothing to lose.
    Clean,
    /// Uncommitted changes, untracked files or a stash.
    Dirty,
    /// git could not answer (not a repo it trusts, corrupt, ...).
    Unknown,
    /// Commits on a local branch that no remote has.
    Unpushed,
}

impl GitState {
    pub fn as_str(self) -> &'static str {
        match self {
            GitState::Ignored => "ignored",
            GitState::Clean => "clean",
            GitState::Dirty => "dirty",
            GitState::Unknown => "unknown",
            GitState::Unpushed => "unpushed",
        }
    }

    /// Work that deleting the path could lose for good.
    pub fn is_risky(self) -> bool {
        self >= GitState::Dirty
    }
}

/// Git work a put can lose. `du` still ranks unpushed above dirty for a
/// row's single state. This names dirty as well, so a checkout with both
/// is never described only as unpushed. Unknown is git missing or failing
/// to answer, and it is risky: it must not be treated as safe to remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RiskyGit {
    pub dirty: bool,
    pub unpushed: bool,
    pub unknown: bool,
}

impl RiskyGit {
    /// Git is missing, or a probe could not answer.
    pub fn unknown() -> Self {
        Self {
            dirty: false,
            unpushed: false,
            unknown: true,
        }
    }

    pub fn is_risky(self) -> bool {
        self.dirty || self.unpushed || self.unknown
    }

    /// Words after "with". Dirty is named whenever the checkout is dirty.
    /// Unknown stays in the label when a probe also saw dirty or unpushed
    /// work, so a partial failure is not described only as that work.
    pub fn label(self) -> &'static str {
        match (self.dirty, self.unpushed, self.unknown) {
            (true, true, true) => "dirty, unpushed and unknown",
            (true, true, false) => "dirty and unpushed",
            (true, false, true) => "dirty and unknown",
            (true, false, false) => "dirty",
            (false, true, true) => "unpushed and unknown",
            (false, true, false) => "unpushed",
            _ => "unknown",
        }
    }

    /// Both probes, so a fact either one saw is kept.
    pub fn union(self, other: Self) -> Self {
        Self {
            dirty: self.dirty || other.dirty,
            unpushed: self.unpushed || other.unpushed,
            unknown: self.unknown || other.unknown,
        }
    }
}

/// `Some` when either side is risky, with the bits combined.
pub fn union_work(a: Option<RiskyGit>, b: Option<RiskyGit>) -> Option<RiskyGit> {
    match (a, b) {
        (None, x) | (x, None) => x.filter(|w| w.is_risky()),
        (Some(a), Some(b)) => {
            let u = a.union(b);
            u.is_risky().then_some(u)
        }
    }
}

/// For a directory that holds a git repository at its top, the risky
/// work in that repository, if any. Not a repository, or a clean one,
/// is `None`. Git missing, or a probe that cannot answer, is
/// [`RiskyGit::unknown`] and never `None`. Shared by put on Unix and on
/// Windows: `safety` is Unix-only.
pub fn risky_repo_state(abs: &Path) -> Option<RiskyGit> {
    let is_repo = abs.join(".git").symlink_metadata().is_ok() || crate::du::is_bare_repo(abs);
    if !is_repo {
        return None;
    }
    let Some(bin) = find_git() else {
        return Some(RiskyGit::unknown());
    };
    let probe = repo_probe(&bin, abs, status_shows_stash(&bin));
    let work = RiskyGit {
        dirty: probe.dirty,
        unpushed: probe.unpushed,
        unknown: probe.unknown || probe.state == GitState::Unknown,
    };
    work.is_risky().then_some(work)
}

/// Repositories kept from one tree. Past this, the rest are unscanned
/// and the tree is unknown, the same cap `du` uses for nested repos.
const REPO_CAP: usize = 64;

/// Repositories at or under `root`, and whether every directory was read.
struct RepoScan {
    repos: Vec<PathBuf>,
    complete: bool,
}

/// Bounded walk for git repositories. A directory that cannot be read,
/// or that sits past [`crate::du::MAX_DEPTH`], leaves the scan incomplete
/// so a removal hint does not treat the tree as safe.
fn find_repos(root: &Path) -> RepoScan {
    let mut repos = Vec::new();
    let mut complete = true;
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > crate::du::MAX_DEPTH {
            complete = false;
            continue;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => {
                complete = false;
                continue;
            }
        };
        let mut names = Vec::new();
        let mut children = Vec::new();
        for ent in entries {
            let Ok(ent) = ent else {
                complete = false;
                continue;
            };
            let name = ent.file_name();
            let child = dir.join(&name);
            let meta = match std::fs::symlink_metadata(&child) {
                Ok(m) => m,
                Err(_) => {
                    complete = false;
                    continue;
                }
            };
            names.push(name.clone());
            if meta.is_dir() && !meta.file_type().is_symlink() && name != ".git" {
                children.push(child);
            }
        }
        let has_dot_git = names.iter().any(|n| n == ".git");
        let bare = !has_dot_git && crate::du::is_bare_repo(&dir);
        if has_dot_git || bare {
            if repos.len() >= REPO_CAP {
                complete = false;
            } else {
                repos.push(dir.clone());
            }
            if bare {
                continue;
            }
        }
        if depth == crate::du::MAX_DEPTH {
            if !children.is_empty() {
                complete = false;
            }
            continue;
        }
        for child in children {
            stack.push((child, depth + 1));
        }
    }
    RepoScan { repos, complete }
}

/// Risky git work at or below `path`. A directory that is not fully
/// scanned is unknown, so it is never described as safe to remove. Not a
/// directory, a path that is gone (nothing is left to lose) or a fully
/// scanned tree with nothing risky is `None`; a path that cannot be
/// reached is unknown.
pub fn risky_tree(path: &Path) -> Option<RiskyGit> {
    let meta = match crate::util::look(path) {
        crate::util::Look::There(m) => m,
        crate::util::Look::Gone => return None,
        crate::util::Look::Unreachable(_) => return Some(RiskyGit::unknown()),
    };
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return None;
    }
    let scan = find_repos(path);
    let mut work = RiskyGit {
        dirty: false,
        unpushed: false,
        unknown: !scan.complete,
    };
    if scan.repos.is_empty() {
        after_probe_seam(path);
        return work.is_risky().then_some(work);
    }
    let Some(bin) = find_git() else {
        work.unknown = true;
        after_probe_seam(path);
        return Some(work);
    };
    let show = status_shows_stash(&bin);
    for repo in &scan.repos {
        let probe = repo_probe(&bin, repo, show);
        let one = RiskyGit {
            dirty: probe.dirty,
            unpushed: probe.unpushed,
            unknown: probe.unknown || probe.state == GitState::Unknown,
        };
        if one.is_risky() {
            work = work.union(one);
        }
    }
    after_probe_seam(path);
    work.is_risky().then_some(work)
}

/// Debug-only: after the probe of `RTRASH_TEST_AFTER_PROBE`, write a file
/// there so a later probe of the moved tree sees uncommitted work.
fn after_probe_seam(dir: &Path) {
    if cfg!(debug_assertions) {
        if let Ok(target) = std::env::var("RTRASH_TEST_AFTER_PROBE") {
            if Path::new(&target) == dir {
                let _ = crate::file_replace::write_own(&dir.join(".rtrash-probe-seam"), b"x");
            }
        }
    }
}

/// One listed row to classify.
#[derive(Debug, Clone, Copy)]
pub struct RowQuery<'a> {
    pub path: &'a Path,
    pub is_dir: bool,
    /// Repositories found at or below the row (directories only).
    pub repos: &'a [PathBuf],
}

/// The `git` executable: the first `git` file on `PATH`, resolved once so
/// each probe skips the search.
pub fn find_git() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exe = if cfg!(windows) { "git.exe" } else { "git" };
    std::env::split_paths(&path)
        .map(|d| d.join(exe))
        .find(|p| is_executable(p))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_executable(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

fn git(bin: &Path, repo: &Path) -> Command {
    let mut c = Command::new(bin);
    c.arg("--no-pager")
        // These read-only commands need repository data, but none may run a
        // program selected by an untrusted repository configuration.
        .args(["-c", "core.fsmonitor=false"])
        .args(["-c", "core.hooksPath="])
        .args(["-c", "diff.external="])
        .arg("-C")
        .arg(repo)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_EXTERNAL_DIFF")
        .env("GIT_PAGER", "cat")
        // Read-only probes: never take the index lock to refresh stat data.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    c
}

/// A Git command that reads every configuration source. This is only for
/// `git config`, which prints configuration data and does not run programs
/// named by it.
fn config_git(bin: &Path, repo: &Path) -> Command {
    let mut c = Command::new(bin);
    c.arg("--no-pager")
        .arg("-C")
        .arg(repo)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_PAGER", "cat")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    c
}

/// What `git config --path --get KEY` prints in `repo`, so system, global
/// and repository config, includes and `GIT_CONFIG_*` all count:
/// `Some(Some(value))` when set, `Some(None)` when unset, `None` when git
/// is missing or cannot answer. Git refuses the config of a repository it
/// does not trust, so then it is asked from outside any repository.
pub fn config_path(repo: &Path, key: &str) -> Option<Option<String>> {
    let bin = find_git()?;
    let ask = |dir: &Path| {
        let out = config_git(&bin, dir)
            .args(["config", "--path", "--get", key])
            .output()
            .ok()?;
        match out.status.code() {
            Some(0) => {
                let v = crate::util::str_from_bytes(out.stdout);
                Some(Some(v.trim_end_matches(['\n', '\r']).to_string()))
            }
            Some(1) => Some(None),
            _ => None,
        }
    };
    ask(repo).or_else(|| ask(&std::env::temp_dir()))
}

/// Nearest ancestor of `path` (not `path` itself) that has a `.git` entry.
pub fn enclosing_repo(path: &Path) -> Option<PathBuf> {
    let mut cur = path.parent();
    while let Some(dir) = cur {
        if dir.join(".git").symlink_metadata().is_ok() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

/// True when `path` is inside a `.git` directory.
pub fn inside_git_dir(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == ".git")
}

/// `git version 2.47.3` → (2, 47).
pub fn parse_version(s: &str) -> Option<(u32, u32)> {
    let v = s.trim().strip_prefix("git version ")?;
    let mut it = v.split(|c: char| !c.is_ascii_digit());
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    Some((major, minor))
}

/// Porcelain v2 prints `# stash N` with `--show-stash` from git 2.35 on.
fn status_shows_stash(bin: &Path) -> bool {
    Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .and_then(|o| parse_version(&String::from_utf8_lossy(&o.stdout)))
        .is_some_and(|v| v >= (2, 35))
}

/// Read a porcelain v2 status: any change or untracked entry, or a
/// `# stash` header, means work that deleting the repo would lose.
pub fn status_is_dirty(out: &[u8]) -> bool {
    out.split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .any(|l| !l.starts_with(b"# ") || l.starts_with(b"# stash "))
}

/// What [`repo_state`] reports to `du`, plus whether the same probe saw
/// uncommitted work. `du` still prefers unpushed over dirty. `dirty` stays
/// set when both are true, so a trash hint does not drop the uncommitted work.
struct RepoProbe {
    state: GitState,
    dirty: bool,
    unpushed: bool,
    /// A required status or stash probe failed. `du` still reads `state`.
    unknown: bool,
}

impl RepoProbe {
    fn unknown() -> Self {
        Self {
            state: GitState::Unknown,
            dirty: false,
            unpushed: false,
            unknown: true,
        }
    }

    fn unpushed(dirty: bool, unknown: bool) -> Self {
        Self {
            state: GitState::Unpushed,
            dirty,
            unpushed: true,
            unknown,
        }
    }

    fn dirty() -> Self {
        Self {
            state: GitState::Dirty,
            dirty: true,
            unpushed: false,
            unknown: false,
        }
    }

    fn clean() -> Self {
        Self {
            state: GitState::Clean,
            dirty: false,
            unpushed: false,
            unknown: false,
        }
    }
}

/// `git rev-parse -q --verify refs/stash`: 0 is a stash, 1 is none, and
/// any other status (128, a signal) did not answer.
fn stash_from_code(code: Option<i32>) -> Option<bool> {
    match code {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

/// State of a whole repository. The unpushed and status probes run at the
/// same time; a stash shows in the status output when git can print it.
/// A bare repository (no `.git` entry) has no work tree to be dirty, so
/// only its branches and stash count. Unpushed still outranks dirty here;
/// [`repo_probe`] keeps the dirty bit beside that ranking.
fn repo_state(bin: &Path, repo: &Path, show_stash: bool) -> GitState {
    repo_probe(bin, repo, show_stash).state
}

/// `Some(true)` when `refs/stash` exists, `Some(false)` when it does not,
/// `None` when git could not be run.
fn stash_dirty(bin: &Path, repo: &Path) -> Option<bool> {
    let mut stash = git(bin, repo);
    stash.args(["rev-parse", "-q", "--verify", "refs/stash"]);
    stash.stdout(Stdio::null());
    match stash.output() {
        Ok(o) => stash_from_code(o.status.code()),
        Err(_) => None,
    }
}

fn repo_probe(bin: &Path, repo: &Path, show_stash: bool) -> RepoProbe {
    let bare = !matches!(
        crate::util::look(&repo.join(".git")),
        crate::util::Look::There(_)
    );
    // Commits reachable from a local branch but from no remote ref.
    let mut log = git(bin, repo);
    log.args([
        "log",
        "--branches",
        "--not",
        "--remotes",
        "-n1",
        "--format=%H",
    ]);
    let mut status = git(bin, repo);
    status.args([
        "status",
        "--porcelain=v2",
        "--untracked-files=normal",
        "--ignored=no",
        "--no-renames",
    ]);
    if show_stash {
        status.arg("--show-stash");
    }
    let log = log.spawn();
    let status = if bare { None } else { Some(status.spawn()) };
    let log = log.and_then(|c| c.wait_with_output());
    let status = status.map(|s| s.and_then(|c| c.wait_with_output()));
    let Ok(log) = log else {
        return RepoProbe::unknown();
    };
    if !log.status.success() {
        return RepoProbe::unknown();
    }
    let unpushed = !log.stdout.iter().all(u8::is_ascii_whitespace);
    if let Some(status) = status {
        let Ok(status) = status else {
            return if unpushed {
                RepoProbe::unpushed(false, true)
            } else {
                RepoProbe::unknown()
            };
        };
        if !status.status.success() {
            return if unpushed {
                RepoProbe::unpushed(false, true)
            } else {
                RepoProbe::unknown()
            };
        }
        let from_status = status_is_dirty(&status.stdout);
        if unpushed {
            // Dirty is read even though du ranks unpushed first. A stash on
            // an older git is not in the status output, so it is asked too.
            // A stash probe that does not answer leaves the unknown bit set.
            let stash = if show_stash {
                None
            } else {
                stash_dirty(bin, repo)
            };
            let dirty = from_status || stash == Some(true);
            let unknown = !show_stash && stash.is_none();
            return RepoProbe::unpushed(dirty, unknown);
        }
        if from_status {
            return RepoProbe::dirty();
        }
        if show_stash {
            return RepoProbe::clean();
        }
    } else if unpushed {
        let stash = stash_dirty(bin, repo);
        return RepoProbe::unpushed(stash == Some(true), stash.is_none());
    }
    match stash_dirty(bin, repo) {
        Some(true) => RepoProbe::dirty(),
        Some(false) => RepoProbe::clean(),
        None => RepoProbe::unknown(),
    }
}

/// Repo probes started while the walk is still running: the walk calls
/// [`Prefetch::repo`] as soon as it sees a repository, so git works on it
/// next to the walk instead of after it. Only the first [`Prefetch::CAP`]
/// repositories start early; [`classify_all`] probes the rest on demand.
pub struct Prefetch {
    bin: Arc<PathBuf>,
    show_stash: Arc<OnceLock<bool>>,
    jobs: Mutex<HashMap<PathBuf, JoinHandle<GitState>>>,
}

/// The probes a prefetch started, and whether `git status` prints
/// stashes. A probe no row needs is never waited for.
pub struct Prefetched {
    jobs: Mutex<HashMap<PathBuf, JoinHandle<GitState>>>,
    show_stash: Option<bool>,
}

impl Prefetched {
    fn has(&self, repo: &Path) -> bool {
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(repo)
    }

    /// The state of `repo`, waiting for its probe.
    fn take(&self, repo: &Path) -> Option<GitState> {
        let job = self
            .jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(repo)?;
        Some(job.join().unwrap_or(GitState::Unknown))
    }
}

impl Prefetch {
    /// Most repositories probed ahead of need. A tree of thousands of
    /// clones then costs no more processes than the rows can use.
    pub const CAP: usize = 64;

    /// `None` when `git` is not on `PATH`.
    pub fn start() -> Option<Prefetch> {
        let bin = Arc::new(find_git()?);
        let show_stash = Arc::new(OnceLock::new());
        let (b, s) = (bin.clone(), show_stash.clone());
        // The version probe gates every status probe; start it first.
        std::thread::spawn(move || {
            s.get_or_init(|| status_shows_stash(&b));
        });
        Some(Prefetch {
            bin,
            show_stash,
            jobs: Mutex::new(HashMap::new()),
        })
    }

    /// Start probing `repo` unless it already started or the cap is hit.
    pub fn repo(&self, repo: &Path) {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        if jobs.len() >= Self::CAP || jobs.contains_key(repo) {
            return;
        }
        let (bin, show_stash) = (self.bin.clone(), self.show_stash.clone());
        let path = repo.to_path_buf();
        let job = std::thread::Builder::new().spawn(move || {
            let show = *show_stash.get_or_init(|| status_shows_stash(&bin));
            repo_state(&bin, &path, show)
        });
        if let Ok(job) = job {
            jobs.insert(repo.to_path_buf(), job);
        }
    }

    /// Stop taking new repositories and hand over the started probes.
    pub fn finish(self) -> Prefetched {
        Prefetched {
            jobs: self.jobs,
            show_stash: self.show_stash.get().copied(),
        }
    }
}

/// Paths relative to `repo` (a trailing slash marks a directory) that match
/// an ignore rule, from one `git check-ignore --stdin -z` run.
fn ignored_in(bin: &Path, repo: &Path, rels: &[OsString]) -> HashSet<OsString> {
    let mut c = git(bin, repo);
    c.args(["check-ignore", "--stdin", "-z"])
        .stdin(Stdio::piped());
    let Ok(mut child) = c.spawn() else {
        return HashSet::new();
    };
    let mut input = Vec::new();
    for r in rels {
        input.extend_from_slice(r.as_encoded_bytes());
        input.push(0);
    }
    // Write from a thread so a full stdout pipe cannot stall the writer.
    let stdin = child.stdin.take();
    let writer = std::thread::spawn(move || {
        if let Some(mut s) = stdin {
            let _ = s.write_all(&input);
        }
    });
    let out = child.wait_with_output();
    let _ = writer.join();
    match out {
        // Exit 1 means nothing matched; 128 is a fatal error.
        Ok(o) if o.status.success() => o
            .stdout
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            // SAFETY: git echoes back bytes we gave it, which came from OsStr.
            .map(|p| unsafe { OsStr::from_encoded_bytes_unchecked(p) }.to_os_string())
            .collect(),
        _ => HashSet::new(),
    }
}

/// Riskiest git state for each row: its own standing in the enclosing repo
/// (ignored, or that repo's state) plus every repo found inside it, with the
/// repo that decided it. `None` for a row no repo touches. Returns `None`
/// when `git` is not on `PATH`.
///
/// Ignore checks run as one batch per repo, all repos at once, and repo
/// states are probed in parallel; only repos some row needs are probed.
pub fn classify_all(rows: &[RowQuery<'_>]) -> Option<Vec<Option<(GitState, PathBuf)>>> {
    classify_all_with(rows, None)
}

/// [`classify_all`], reusing the repo states a [`Prefetch`] already has.
pub fn classify_all_with(
    rows: &[RowQuery<'_>],
    prefetched: Option<&Prefetched>,
) -> Option<Vec<Option<(GitState, PathBuf)>>> {
    let started = |repo: &Path| prefetched.is_some_and(|p| p.has(repo));
    let enclosing: Vec<Option<PathBuf>> = rows.iter().map(|r| enclosing_repo(r.path)).collect();
    let touched = enclosing.iter().any(Option::is_some) || rows.iter().any(|r| !r.repos.is_empty());
    if !touched {
        return Some(vec![None; rows.len()]);
    }
    let bin = find_git()?;
    let bin = bin.as_path();

    let mut by_repo: HashMap<&Path, Vec<OsString>> = HashMap::new();
    let rel_args: Vec<Option<OsString>> = rows
        .iter()
        .zip(&enclosing)
        .map(|(row, repo)| {
            let rel = row.path.strip_prefix(repo.as_ref()?).ok()?;
            let mut arg = rel.as_os_str().to_os_string();
            if row.is_dir {
                // A trailing slash lets `dir/` patterns match.
                arg.push("/");
            }
            Some(arg)
        })
        .collect();
    for (repo, arg) in enclosing.iter().zip(&rel_args) {
        if let (Some(repo), Some(arg)) = (repo, arg) {
            by_repo.entry(repo.as_path()).or_default().push(arg.clone());
        }
    }

    // Every probe is a short-lived child process, so each gets its own
    // thread. Repos found inside rows are needed whatever the ignore rules
    // say; their probes start at once, next to the ignore checks.
    std::thread::scope(|scope| {
        let cached_stash = prefetched.and_then(|p| p.show_stash);
        let show_stash =
            scope.spawn(move || cached_stash.unwrap_or_else(|| status_shows_stash(bin)));
        let ignore_jobs: Vec<_> = by_repo
            .iter()
            .map(|(repo, rels)| (*repo, scope.spawn(move || ignored_in(bin, repo, rels))))
            .collect();
        let show_stash = show_stash.join().unwrap_or(false);
        let mut state_jobs: HashMap<&Path, std::thread::ScopedJoinHandle<'_, GitState>> =
            HashMap::new();
        // Repos the walk already started on: waited for last, so the
        // probes still to start are not held up behind them.
        let mut early: Vec<&Path> = Vec::new();
        for row in rows {
            for r in row.repos {
                if started(r) {
                    early.push(r.as_path());
                    continue;
                }
                state_jobs
                    .entry(r.as_path())
                    .or_insert_with(|| scope.spawn(move || repo_state(bin, r, show_stash)));
            }
        }
        let ignored: HashMap<&Path, HashSet<OsString>> = ignore_jobs
            .into_iter()
            .map(|(repo, h)| (repo, h.join().unwrap_or_default()))
            .collect();
        let is_ignored: Vec<bool> = enclosing
            .iter()
            .zip(&rel_args)
            .map(|(repo, arg)| match (repo, arg) {
                (Some(repo), Some(arg)) => ignored
                    .get(repo.as_path())
                    .is_some_and(|set| set.contains(arg)),
                _ => false,
            })
            .collect();
        for (repo, ign) in enclosing.iter().zip(&is_ignored) {
            if let (Some(repo), false) = (repo, ign) {
                if started(repo) {
                    early.push(repo.as_path());
                    continue;
                }
                state_jobs
                    .entry(repo.as_path())
                    .or_insert_with(|| scope.spawn(move || repo_state(bin, repo, show_stash)));
            }
        }
        let mut states: HashMap<&Path, GitState> = HashMap::new();
        for repo in early {
            if let Some(s) = prefetched.and_then(|p| p.take(repo)) {
                states.insert(repo, s);
            }
        }
        states.extend(
            state_jobs
                .into_iter()
                .map(|(repo, h)| (repo, h.join().unwrap_or(GitState::Unknown))),
        );
        Some(assemble(rows, &enclosing, &is_ignored, &states))
    })
}

fn assemble(
    rows: &[RowQuery<'_>],
    enclosing: &[Option<PathBuf>],
    is_ignored: &[bool],
    states: &HashMap<&Path, GitState>,
) -> Vec<Option<(GitState, PathBuf)>> {
    rows.iter()
        .enumerate()
        .map(|(i, row)| {
            let mut best: Option<(GitState, PathBuf)> = None;
            let mut consider = |s: GitState, r: &Path| {
                if best.as_ref().map(|(b, _)| s > *b).unwrap_or(true) {
                    best = Some((s, r.to_path_buf()));
                }
            };
            if let Some(repo) = &enclosing[i] {
                let s = if is_ignored[i] {
                    GitState::Ignored
                } else {
                    states[repo.as_path()]
                };
                consider(s, repo);
            }
            for r in row.repos {
                consider(states[r.as_path()], r);
            }
            best
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_and_risk() {
        assert!(GitState::Unpushed > GitState::Dirty);
        assert!(GitState::Dirty > GitState::Clean);
        assert!(GitState::Clean > GitState::Ignored);
        assert!(GitState::Unknown.is_risky());
        assert!(!GitState::Clean.is_risky());
        assert_eq!(GitState::Unpushed.as_str(), "unpushed");
    }

    #[test]
    fn version_and_status_parsing() {
        assert_eq!(parse_version("git version 2.47.3\n"), Some((2, 47)));
        assert_eq!(parse_version("git version 2.35.0.windows.1"), Some((2, 35)));
        assert_eq!(parse_version("nope"), None);
        assert!(!status_is_dirty(b""));
        assert!(!status_is_dirty(b"# branch.oid abc\n# branch.head main\n"));
        assert!(status_is_dirty(b"# stash 2\n"));
        assert!(status_is_dirty(b"? new.rs\n"));
        assert!(status_is_dirty(
            b"1 .M N... 100644 100644 100644 a b src/x.rs\n"
        ));
    }

    #[test]
    fn prefetched_states_match_probing_after_the_walk() {
        let Some(bin) = find_git() else { return };
        let root = std::env::temp_dir().join(format!("rtrash-prefetch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("r");
        std::fs::create_dir_all(&repo).unwrap();
        let ok = Command::new(&bin)
            .args(["init", "-q"])
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            return;
        }
        std::fs::write(repo.join("new.txt"), b"x").unwrap();
        let repos = [repo.clone()];
        let rows = [RowQuery {
            path: &root,
            is_dir: true,
            repos: &repos,
        }];
        let p = Prefetch::start().unwrap();
        p.repo(&repo);
        p.repo(&repo);
        let done = p.finish();
        assert!(done.has(&repo));
        let early = classify_all_with(&rows, Some(&done)).unwrap();
        let late = classify_all(&rows).unwrap();
        assert_eq!(early, late);
        assert_eq!(early[0].as_ref().map(|(s, _)| *s), Some(GitState::Dirty));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn git_dir_detection() {
        assert!(inside_git_dir(Path::new("/r/.git/objects/pack")));
        assert!(!inside_git_dir(Path::new("/r/src/.gitignore")));
    }

    /// du still reports unpushed as the single state. The trash hint names
    /// the uncommitted file as well, which a probe that returns on the first
    /// unpushed commit would drop.
    #[test]
    fn a_dirty_unpushed_repo_stays_unpushed_for_du_and_names_both_for_a_hint() {
        let Some(bin) = find_git() else { return };
        let root = std::env::temp_dir().join(format!("rtrash-git-both-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("r");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            Command::new(&bin)
                .args(args)
                .current_dir(&repo)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "Trash Test")
                .env("GIT_AUTHOR_EMAIL", "trash-test@example.com")
                .env("GIT_COMMITTER_NAME", "Trash Test")
                .env("GIT_COMMITTER_EMAIL", "trash-test@example.com")
                .status()
                .is_ok_and(|s| s.success())
        };
        if !git(&["init", "-q"]) {
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        std::fs::write(repo.join("f"), b"x").unwrap();
        assert!(git(&["add", "f"]) && git(&["commit", "-qm", "work"]));
        std::fs::write(repo.join("extra"), b"y").unwrap();
        let repos = [repo.clone()];
        let rows = [RowQuery {
            path: &repo,
            is_dir: true,
            repos: &repos,
        }];
        let classified = classify_all(&rows).unwrap();
        assert_eq!(
            classified[0].as_ref().map(|(s, _)| *s),
            Some(GitState::Unpushed)
        );
        let risk = risky_repo_state(&repo).unwrap();
        assert!(risk.dirty && risk.unpushed && !risk.unknown);
        assert_eq!(risk.label(), "dirty and unpushed");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A `.git` git cannot read is unknown, not the absence of risk.
    #[test]
    fn a_repo_git_cannot_read_is_unknown() {
        let Some(_bin) = find_git() else { return };
        let root = std::env::temp_dir().join(format!("rtrash-git-unk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("r");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let got = risky_repo_state(&repo);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(got, Some(RiskyGit::unknown()));
        assert_eq!(RiskyGit::unknown().label(), "unknown");
    }

    #[test]
    fn a_fatal_stash_probe_is_not_a_clean_stash() {
        assert_eq!(stash_from_code(Some(0)), Some(true));
        assert_eq!(stash_from_code(Some(1)), Some(false));
        assert_eq!(stash_from_code(Some(128)), None);
        assert_eq!(stash_from_code(None), None);
    }

    /// A path that is gone has nothing to lose, so a dangling record is not
    /// refused as unknown; one that cannot be reached still is unknown.
    #[test]
    fn a_gone_path_holds_no_risk() {
        let root = std::env::temp_dir().join(format!("rtrash-git-gone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        assert!(risky_tree(&root.join("missing")).is_none());
        std::fs::write(root.join("file"), b"x").unwrap();
        assert!(risky_tree(&root.join("file/below")).is_none());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A parent that only contains a repository is risky, and a tree that
    /// holds more repositories than the cap is not treated as fully scanned.
    #[test]
    fn a_nested_repo_is_risky_and_an_unscanned_tree_is_unknown() {
        let Some(bin) = find_git() else { return };
        let root = std::env::temp_dir().join(format!("rtrash-git-nest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let parent = root.join("parent");
        let repo = parent.join("child");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            Command::new(&bin)
                .args(args)
                .current_dir(&repo)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "Trash Test")
                .env("GIT_AUTHOR_EMAIL", "trash-test@example.com")
                .env("GIT_COMMITTER_NAME", "Trash Test")
                .env("GIT_COMMITTER_EMAIL", "trash-test@example.com")
                .status()
        };
        assert!(git(&["init", "-q"]).unwrap().success());
        std::fs::write(repo.join("f"), b"x").unwrap();
        let risk = risky_tree(&parent).unwrap();
        assert!(risk.dirty, "{risk:?}");
        assert!(!risk.label().contains("only"));
        let wide = root.join("wide");
        std::fs::create_dir_all(&wide).unwrap();
        for i in 0..=REPO_CAP {
            let d = wide.join(format!("r{i}"));
            std::fs::create_dir_all(d.join(".git")).unwrap();
        }
        let over = risky_tree(&wide).unwrap();
        assert!(over.unknown, "{over:?}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
