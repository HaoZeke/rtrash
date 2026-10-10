//! Git state for `rtrash du` rows: is a listed path ignored, tracked in a
//! clean repo, or does it hold uncommitted, stashed or unpushed work?
//! Uses the `git` binary; no library dependency.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
    c.arg("-C")
        .arg(repo)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        // Read-only probes: never take the index lock to refresh stat data.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    c
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

/// State of a whole repository. The unpushed and status probes run at the
/// same time; a stash shows in the status output when git can print it.
fn repo_state(bin: &Path, repo: &Path, show_stash: bool) -> GitState {
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
    let status = status.spawn();
    let log = log.and_then(|c| c.wait_with_output());
    let status = status.and_then(|c| c.wait_with_output());
    let Ok(log) = log else {
        return GitState::Unknown;
    };
    if !log.status.success() {
        return GitState::Unknown;
    }
    if !log.stdout.iter().all(u8::is_ascii_whitespace) {
        return GitState::Unpushed;
    }
    let Ok(status) = status else {
        return GitState::Unknown;
    };
    if !status.status.success() {
        return GitState::Unknown;
    }
    if status_is_dirty(&status.stdout) {
        return GitState::Dirty;
    }
    if show_stash {
        return GitState::Clean;
    }
    let mut stash = git(bin, repo);
    stash.args(["rev-parse", "-q", "--verify", "refs/stash"]);
    stash.stdout(Stdio::null());
    match stash.output() {
        Ok(o) if o.status.success() => GitState::Dirty,
        Ok(_) => GitState::Clean,
        Err(_) => GitState::Unknown,
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
        let show_stash = scope.spawn(|| status_shows_stash(bin));
        let ignore_jobs: Vec<_> = by_repo
            .iter()
            .map(|(repo, rels)| (*repo, scope.spawn(move || ignored_in(bin, repo, rels))))
            .collect();
        let show_stash = show_stash.join().unwrap_or(false);
        let mut state_jobs: HashMap<&Path, std::thread::ScopedJoinHandle<'_, GitState>> =
            HashMap::new();
        for row in rows {
            for r in row.repos {
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
                state_jobs
                    .entry(repo.as_path())
                    .or_insert_with(|| scope.spawn(move || repo_state(bin, repo, show_stash)));
            }
        }
        let states: HashMap<&Path, GitState> = state_jobs
            .into_iter()
            .map(|(repo, h)| (repo, h.join().unwrap_or(GitState::Unknown)))
            .collect();
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
    fn git_dir_detection() {
        assert!(inside_git_dir(Path::new("/r/.git/objects/pack")));
        assert!(!inside_git_dir(Path::new("/r/src/.gitignore")));
    }
}
