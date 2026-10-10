//! Git state for `rtrash du` rows: is a listed path ignored, tracked in a
//! clean repo, or does it hold uncommitted, stashed or unpushed work?
//! Uses the `git` binary; no library dependency.

use std::collections::HashMap;
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

/// Caches per-repo answers across rows.
pub struct GitProbe {
    repo_states: HashMap<PathBuf, GitState>,
    /// Set when spawning `git` failed (binary missing).
    pub unavailable: bool,
}

impl Default for GitProbe {
    fn default() -> Self {
        Self::new()
    }
}

fn git(repo: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C")
        .arg(repo)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
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

impl GitProbe {
    pub fn new() -> Self {
        GitProbe {
            repo_states: HashMap::new(),
            unavailable: false,
        }
    }

    fn output(&mut self, mut cmd: Command) -> Option<std::process::Output> {
        match cmd.output() {
            Ok(o) => Some(o),
            Err(_) => {
                self.unavailable = true;
                None
            }
        }
    }

    /// State of a whole repository (cached).
    pub fn repo_state(&mut self, repo: &Path) -> GitState {
        if let Some(s) = self.repo_states.get(repo) {
            return *s;
        }
        let s = self.compute_repo_state(repo);
        self.repo_states.insert(repo.to_path_buf(), s);
        s
    }

    fn compute_repo_state(&mut self, repo: &Path) -> GitState {
        // Commits reachable from a local branch but from no remote ref.
        let mut log = git(repo);
        log.args([
            "log",
            "--branches",
            "--not",
            "--remotes",
            "-n1",
            "--format=%H",
        ]);
        let Some(out) = self.output(log) else {
            return GitState::Unknown;
        };
        if !out.status.success() {
            return GitState::Unknown;
        }
        if !out.stdout.iter().all(u8::is_ascii_whitespace) {
            return GitState::Unpushed;
        }
        let mut status = git(repo);
        status.args(["status", "--porcelain", "--untracked-files=normal"]);
        let Some(out) = self.output(status) else {
            return GitState::Unknown;
        };
        if !out.status.success() {
            return GitState::Unknown;
        }
        if !out.stdout.is_empty() {
            return GitState::Dirty;
        }
        let mut stash = git(repo);
        stash.args(["rev-parse", "-q", "--verify", "refs/stash"]);
        stash.stdout(Stdio::null());
        match self.output(stash) {
            Some(o) if o.status.success() => GitState::Dirty,
            Some(_) => GitState::Clean,
            None => GitState::Unknown,
        }
    }

    /// Whether `path` (inside `repo`) matches an ignore rule.
    pub fn is_ignored(&mut self, repo: &Path, path: &Path, is_dir: bool) -> bool {
        let Ok(rel) = path.strip_prefix(repo) else {
            return false;
        };
        let mut arg = rel.as_os_str().to_os_string();
        if is_dir {
            // A trailing slash lets `dir/` patterns match.
            arg.push("/");
        }
        let mut c = git(repo);
        c.args(["check-ignore", "-q", "--"]).arg(arg);
        c.stdout(Stdio::null());
        matches!(self.output(c), Some(o) if o.status.success())
    }

    /// Riskiest state for a listed path: its own standing in the enclosing
    /// repo, plus every repo found inside it. Returns the state and the repo
    /// that decided it, or `None` when no repo is involved.
    pub fn classify(
        &mut self,
        path: &Path,
        is_dir: bool,
        repos_inside: &[PathBuf],
    ) -> Option<(GitState, PathBuf)> {
        let mut best: Option<(GitState, PathBuf)> = None;
        let consider = |s: GitState, r: &Path, best: &mut Option<(GitState, PathBuf)>| {
            if best.as_ref().map(|(b, _)| s > *b).unwrap_or(true) {
                *best = Some((s, r.to_path_buf()));
            }
        };
        if let Some(repo) = enclosing_repo(path) {
            let s = if self.is_ignored(&repo, path, is_dir) {
                GitState::Ignored
            } else {
                self.repo_state(&repo)
            };
            consider(s, &repo, &mut best);
        }
        for r in repos_inside {
            let s = self.repo_state(r);
            consider(s, r, &mut best);
        }
        best
    }
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
    fn git_dir_detection() {
        assert!(inside_git_dir(Path::new("/r/.git/objects/pack")));
        assert!(!inside_git_dir(Path::new("/r/src/.gitignore")));
    }
}
