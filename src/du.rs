//! `rtrash du`: find what to delete. Walks one or more paths in parallel and
//! reports the largest directories and files, with on-disk and apparent sizes.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use rayon::prelude::*;

use crate::du_git::{self, GitProbe, GitState};
use crate::fastdelete::format_bytes;
use crate::info;
use crate::list::json_escape;
use crate::status;
use crate::trashdir::{self, TrashDir};

const HELP: &str = "\
Usage: {prog} [OPTION]... [PATH]...
Find what to delete: walk each PATH (default .) in parallel and list the
largest directories and files, by on-disk size unless --apparent is given.

Symlinks are never followed, and a file with more than one hard link
counts once. When one subdirectory accounts for 90% or more of a
directory's size, only the subdirectory is listed (--all-dirs lists both).
Regenerable artefacts are tagged and listed as one row each; their
contents never appear on their own.

Each row inside or containing a git repository gets a git state: ignored,
clean, dirty (uncommitted changes, untracked files or a stash), unknown, or
unpushed (local commits no remote has). --print0 leaves out dirty,
unknown and unpushed rows unless --include-dirty is given. Paths inside
.git are never listed.

The closing summary shows the trash on the scanned file systems: trashed
files still take space there until `rtrash empty` removes them (preview
with `rtrash empty -n`, keep recent items with `rtrash empty DAYS`). Trash
directories appear as one `trash` row and are never printed by --print0.

  -n, --top=N           list N directories and N files (default 20)
  -x, --one-file-system skip directories on other file systems
      --apparent        rank by apparent size (file length) instead of disk use
      --max-depth=N     list only entries at most N levels below PATH
      --all-dirs        list every directory level, not just the one that
                          holds the bulk of its parent
      --artefacts       list only regenerable build artefacts and caches
                          (cargo target dirs, node_modules, __pycache__,
                          virtualenvs, CACHEDIR.TAG directories, ...)
      --dirs            list directories only
      --files           list files only
      --older-than=DAYS only list entries whose newest modification is more
                          than DAYS days old (directories use their whole tree)
      --newer-than=DAYS only list entries modified within the last DAYS days
      --home-only       consider only the home trash for the trash summary
      --trash-dir=PATH  consider only this trash directory (repeatable)
      --no-git          do not ask git about listed paths
      --include-dirty   with --print0, also print paths whose git state is
                          dirty, unpushed or unknown
      --json            emit a JSON object (scripts)
      --print0          print the listed paths, NUL-separated, for
                          `xargs -0 rtrash put -rf --`
      --pick            open the put browser on the listed rows: mark rows
                          and trash them (needs a terminal)
      --help            display this help and exit
      --version         output version information and exit
";

/// Classify a directory as a regenerable build artefact or cache from its
/// name and the names of its direct children. Rules need a marker file where
/// the name alone is ambiguous (`target`, `build`, `venv`).
pub fn artefact_kind(name: &str, has_child: impl Fn(&str) -> bool) -> Option<&'static str> {
    if has_child(".rustc_info.json") || (name == "target" && has_child("CACHEDIR.TAG")) {
        return Some("cargo-target");
    }
    let by_name = match name {
        "node_modules" => Some("node-modules"),
        "__pycache__" | ".pytest_cache" | ".mypy_cache" | ".ruff_cache" | ".hypothesis" => {
            Some("python-cache")
        }
        ".tox" | ".nox" => Some("python-env"),
        ".gradle" => Some("gradle-cache"),
        ".next" | ".nuxt" | ".parcel-cache" | ".turbo" | ".svelte-kit" => Some("js-build-cache"),
        ".zig-cache" | "zig-cache" | "zig-out" => Some("zig-cache"),
        ".pixi" => Some("pixi-env"),
        ".direnv" => Some("direnv"),
        _ => None,
    };
    if by_name.is_some() {
        return by_name;
    }
    if has_child("pyvenv.cfg") {
        return Some("python-env");
    }
    if has_child("CMakeCache.txt") {
        return Some("cmake-build");
    }
    if has_child("CACHEDIR.TAG") {
        return Some("cache");
    }
    None
}

/// Which size orders the lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Disk,
    Apparent,
}

/// Scan settings shared by every worker.
#[derive(Debug, Clone)]
pub struct ScanOpts {
    pub top: usize,
    pub one_file_system: bool,
    pub sort: SortKey,
    pub max_depth: Option<usize>,
    pub all_dirs: bool,
    /// List only directories tagged as regenerable artefacts.
    pub artefacts_only: bool,
    /// Only list entries whose newest mtime is at or before this epoch.
    pub older_cutoff: Option<i64>,
    /// Only list entries whose newest mtime is after this epoch.
    pub newer_cutoff: Option<i64>,
    /// Trash roots: listed as one `trash` row each, contents never listed.
    pub trash_roots: Vec<PathBuf>,
}

impl Default for ScanOpts {
    fn default() -> Self {
        ScanOpts {
            top: 20,
            one_file_system: false,
            sort: SortKey::Disk,
            max_depth: None,
            all_dirs: false,
            artefacts_only: false,
            older_cutoff: None,
            newer_cutoff: None,
            trash_roots: Vec::new(),
        }
    }
}

/// One listed file or directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub path: PathBuf,
    pub is_dir: bool,
    /// Sum of file lengths (`st_size`).
    pub apparent: u64,
    /// Allocated bytes (`st_blocks * 512`).
    pub disk: u64,
    /// Newest modification time in the tree (seconds since the epoch).
    pub mtime: i64,
    /// Kind of regenerable artefact (`cargo-target`, `node-modules`, ...).
    pub artefact: Option<&'static str>,
    /// Git repositories at or below this directory (capped).
    pub repos: Vec<PathBuf>,
    /// Riskiest git state and the repo that decided it (after `annotate_git`).
    pub git: Option<(GitState, PathBuf)>,
}

impl Item {
    fn key(&self, sort: SortKey) -> u64 {
        match sort {
            SortKey::Disk => self.disk,
            SortKey::Apparent => self.apparent,
        }
    }
}

/// The N largest items seen so far, largest first.
#[derive(Debug, Clone)]
pub struct TopN {
    cap: usize,
    sort: SortKey,
    items: Vec<Item>,
}

impl TopN {
    pub fn new(cap: usize, sort: SortKey) -> Self {
        TopN {
            cap,
            sort,
            items: Vec::new(),
        }
    }

    pub fn push(&mut self, item: Item) {
        if self.cap == 0 {
            return;
        }
        let k = item.key(self.sort);
        if self.items.len() == self.cap {
            match self.items.last() {
                Some(last) if last.key(self.sort) >= k => return,
                _ => {}
            }
        }
        // Ties keep path order so output is deterministic across runs.
        let pos = self.items.partition_point(|x| {
            let xk = x.key(self.sort);
            xk > k || (xk == k && x.path < item.path)
        });
        self.items.insert(pos, item);
        self.items.truncate(self.cap);
    }

    pub fn merge(&mut self, other: TopN) {
        for it in other.items {
            self.push(it);
        }
    }

    pub fn into_vec(self) -> Vec<Item> {
        self.items
    }
}

/// Totals for one scanned PATH.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootTotals {
    pub path: PathBuf,
    pub apparent: u64,
    pub disk: u64,
    pub files: u64,
    pub dirs: u64,
}

/// Result of scanning one or more paths.
#[derive(Debug, Clone)]
pub struct Report {
    pub roots: Vec<RootTotals>,
    pub dirs: Vec<Item>,
    pub files: Vec<Item>,
    /// Entries that could not be read (permission denied, vanished).
    pub unreadable: u64,
}

/// Trash on the file systems that were scanned: emptying it is what
/// actually returns space there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrashSummary {
    pub roots: Vec<PathBuf>,
    pub items: u64,
    pub bytes: u64,
}

/// Summarize the trash dirs that live on the same device as any scan root.
pub fn trash_on_scanned_devices(dirs: &[TrashDir], report: &Report) -> TrashSummary {
    let devs: HashSet<u64> = report
        .roots
        .iter()
        .filter_map(|r| fs::symlink_metadata(&r.path).ok().map(|m| m.dev()))
        .collect();
    let same: Vec<TrashDir> = dirs
        .iter()
        .filter(|d| {
            fs::symlink_metadata(&d.root)
                .map(|m| devs.contains(&m.dev()))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    let (_, items, bytes) = status::summarize_dirs(&same, None, None);
    TrashSummary {
        roots: same.into_iter().map(|d| d.root).collect(),
        items,
        bytes,
    }
}

struct Ctx<'a> {
    opts: &'a ScanOpts,
    root_dev: u64,
    /// (dev, ino) of multiply-linked files already counted.
    seen: Mutex<HashSet<(u64, u64)>>,
    unreadable: AtomicU64,
    files: AtomicU64,
    dirs: AtomicU64,
}

impl Ctx<'_> {
    fn listable(&self, depth: usize, mtime: i64) -> bool {
        if let Some(max) = self.opts.max_depth {
            if depth > max {
                return false;
            }
        }
        if let Some(c) = self.opts.older_cutoff {
            if mtime > c {
                return false;
            }
        }
        if let Some(c) = self.opts.newer_cutoff {
            if mtime <= c {
                return false;
            }
        }
        true
    }

    /// Count a non-directory once even when it has several hard links.
    fn first_link(&self, meta: &fs::Metadata) -> bool {
        if meta.nlink() <= 1 {
            return true;
        }
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        seen.insert((meta.dev(), meta.ino()))
    }
}

struct Node {
    apparent: u64,
    disk: u64,
    mtime: i64,
    top_dirs: TopN,
    top_files: TopN,
    repos: Vec<PathBuf>,
}

/// At most this many repo roots are remembered per directory.
const MAX_REPOS: usize = 64;

fn sizes(meta: &fs::Metadata) -> (u64, u64) {
    (meta.len(), meta.blocks().saturating_mul(512))
}

fn walk_dir(ctx: &Ctx<'_>, path: &Path, meta: &fs::Metadata, depth: usize) -> Node {
    let opts = ctx.opts;
    // Like GNU du, a directory adds blocks to disk use but nothing to the
    // apparent size: its st_size is a filesystem detail, not data.
    let (_, disk) = sizes(meta);
    let mut node = Node {
        apparent: 0,
        disk,
        mtime: meta.mtime(),
        top_dirs: TopN::new(opts.top, opts.sort),
        top_files: TopN::new(opts.top, opts.sort),
        repos: Vec::new(),
    };
    ctx.dirs.fetch_add(1, Ordering::Relaxed);
    let rd = match fs::read_dir(path) {
        Ok(rd) => rd,
        Err(_) => {
            ctx.unreadable.fetch_add(1, Ordering::Relaxed);
            return node;
        }
    };
    let mut subdirs: Vec<(PathBuf, fs::Metadata)> = Vec::new();
    let mut markers: Vec<std::ffi::OsString> = Vec::new();
    for ent in rd {
        let Ok(ent) = ent else {
            ctx.unreadable.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let fname = ent.file_name();
        if fname == ".git" && node.repos.is_empty() {
            node.repos.push(path.to_path_buf());
        }
        if MARKERS.iter().any(|m| fname == *m) {
            markers.push(fname);
        }
        // DirEntry::metadata does not follow symlinks on Unix.
        let Ok(m) = ent.metadata() else {
            ctx.unreadable.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        if m.is_dir() {
            if opts.one_file_system && m.dev() != ctx.root_dev {
                continue;
            }
            subdirs.push((ent.path(), m));
            continue;
        }
        ctx.files.fetch_add(1, Ordering::Relaxed);
        if !ctx.first_link(&m) {
            continue;
        }
        let (a, d) = sizes(&m);
        node.apparent = node.apparent.saturating_add(a);
        node.disk = node.disk.saturating_add(d);
        node.mtime = node.mtime.max(m.mtime());
        if m.is_file() && !opts.artefacts_only && ctx.listable(depth + 1, m.mtime()) {
            node.top_files.push(Item {
                path: ent.path(),
                is_dir: false,
                apparent: a,
                disk: d,
                mtime: m.mtime(),
                artefact: None,
                repos: Vec::new(),
                git: None,
            });
        }
    }

    let children: Vec<Node> = subdirs
        .into_par_iter()
        .map(|(p, m)| walk_dir(ctx, &p, &m, depth + 1))
        .collect();

    let mut largest_child = 0u64;
    for child in children {
        let child_key = match opts.sort {
            SortKey::Disk => child.disk,
            SortKey::Apparent => child.apparent,
        };
        largest_child = largest_child.max(child_key);
        node.apparent = node.apparent.saturating_add(child.apparent);
        node.disk = node.disk.saturating_add(child.disk);
        node.mtime = node.mtime.max(child.mtime);
        node.top_files.merge(child.top_files);
        node.top_dirs.merge(child.top_dirs);
        for r in child.repos {
            if node.repos.len() < MAX_REPOS {
                node.repos.push(r);
            }
        }
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    if name == ".git" {
        // Repository internals are never offered for deletion.
        node.top_dirs = TopN::new(opts.top, opts.sort);
        node.top_files = TopN::new(opts.top, opts.sort);
        return node;
    }
    let artefact = if opts.trash_roots.iter().any(|r| r == path) {
        Some("trash")
    } else {
        artefact_kind(&name, |c| markers.iter().any(|m| m == c))
    };
    if artefact.is_some() {
        // The artefact is one row; nothing below it is listed separately.
        node.top_dirs = TopN::new(opts.top, opts.sort);
        node.top_files = TopN::new(opts.top, opts.sort);
    }
    node.top_dirs_offer_self(ctx, path, depth, largest_child, artefact);
    node
}

/// Child names that `artefact_kind` looks for.
const MARKERS: &[&str] = &[
    ".rustc_info.json",
    "CACHEDIR.TAG",
    "pyvenv.cfg",
    "CMakeCache.txt",
];

impl Node {
    /// Offer this directory to the list unless it is the scan root or one
    /// subdirectory already accounts for 90% or more of it.
    fn top_dirs_offer_self(
        &mut self,
        ctx: &Ctx<'_>,
        path: &Path,
        depth: usize,
        largest: u64,
        artefact: Option<&'static str>,
    ) {
        if depth == 0 || !ctx.listable(depth, self.mtime) {
            return;
        }
        if ctx.opts.artefacts_only && artefact.is_none() {
            return;
        }
        let own = match ctx.opts.sort {
            SortKey::Disk => self.disk,
            SortKey::Apparent => self.apparent,
        };
        let dominated = own > 0 && largest.saturating_mul(10) >= own.saturating_mul(9);
        if artefact.is_none() && !ctx.opts.all_dirs && dominated {
            return;
        }
        self.top_dirs.push(Item {
            path: path.to_path_buf(),
            is_dir: true,
            apparent: self.apparent,
            disk: self.disk,
            mtime: self.mtime,
            artefact,
            repos: self.repos.clone(),
            git: None,
        });
    }
}

/// Scan `paths` and return the merged report. Paths are made absolute first
/// (without resolving a final symlink) so listed paths work from any cwd.
pub fn scan(paths: &[PathBuf], opts: &ScanOpts) -> io::Result<Report> {
    let mut roots = Vec::new();
    let mut dirs = TopN::new(opts.top, opts.sort);
    let mut files = TopN::new(opts.top, opts.sort);
    let mut unreadable = 0u64;
    for p in paths {
        let abs = absolute(p)?;
        let meta = fs::symlink_metadata(&abs)?;
        let ctx = Ctx {
            opts,
            root_dev: meta.dev(),
            seen: Mutex::new(HashSet::new()),
            unreadable: AtomicU64::new(0),
            files: AtomicU64::new(0),
            dirs: AtomicU64::new(0),
        };
        let (apparent, disk) = if meta.is_dir() {
            let node = walk_dir(&ctx, &abs, &meta, 0);
            dirs.merge(node.top_dirs);
            files.merge(node.top_files);
            (node.apparent, node.disk)
        } else {
            ctx.files.fetch_add(1, Ordering::Relaxed);
            let (a, d) = sizes(&meta);
            if meta.is_file() && !opts.artefacts_only && ctx.listable(0, meta.mtime()) {
                files.push(Item {
                    path: abs.clone(),
                    is_dir: false,
                    apparent: a,
                    disk: d,
                    mtime: meta.mtime(),
                    artefact: None,
                    repos: Vec::new(),
                    git: None,
                });
            }
            (a, d)
        };
        unreadable += ctx.unreadable.load(Ordering::Relaxed);
        roots.push(RootTotals {
            path: abs,
            apparent,
            disk,
            files: ctx.files.load(Ordering::Relaxed),
            dirs: ctx.dirs.load(Ordering::Relaxed),
        });
    }
    Ok(Report {
        roots,
        dirs: dirs.into_vec(),
        files: files.into_vec(),
        unreadable,
    })
}

fn absolute(p: &Path) -> io::Result<PathBuf> {
    if p.file_name().is_none() {
        // `.`, `..`, `/`: no final component to keep unresolved.
        return fs::canonicalize(p);
    }
    trashdir::abs_nofollow(p)
}

/// Fill in `git` for every listed row. Returns false when `git` could not
/// be run at all.
pub fn annotate_git(report: &mut Report) -> bool {
    let mut probe = GitProbe::new();
    for it in report.dirs.iter_mut().chain(report.files.iter_mut()) {
        if du_git::inside_git_dir(&it.path) {
            continue;
        }
        it.git = probe.classify(&it.path, it.is_dir, &it.repos);
        if probe.unavailable {
            it.git = None;
            return false;
        }
    }
    true
}

/// Compact age such as `45s`, `3h`, `12d`, `2y` (time since `mtime`).
pub fn format_age(mtime: i64, now: i64) -> String {
    let s = (now - mtime).max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3_600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3_600)
    } else if s < 365 * 86_400 {
        format!("{}d", s / 86_400)
    } else {
        format!("{}y", s / (365 * 86_400))
    }
}

fn json_opt(v: Option<&str>) -> String {
    match v {
        Some(s) => format!("\"{}\"", json_escape(s)),
        None => "null".into(),
    }
}

fn item_json(it: &Item) -> String {
    let git_state = it.git.as_ref().map(|(s, _)| s.as_str());
    let git_repo = it
        .git
        .as_ref()
        .map(|(_, r)| r.to_string_lossy().into_owned());
    format!(
        "{{\"path\":\"{}\",\"type\":\"{}\",\"disk\":{},\"apparent\":{},\"mtime\":{},\"artefact\":{},\"git\":{},\"git_repo\":{}}}",
        json_escape(&it.path.to_string_lossy()),
        if it.is_dir { "dir" } else { "file" },
        it.disk,
        it.apparent,
        it.mtime,
        json_opt(it.artefact),
        json_opt(git_state),
        json_opt(git_repo.as_deref())
    )
}

/// Tags shown in the NOTE column.
fn notes(it: &Item) -> String {
    let mut v: Vec<String> = Vec::new();
    if let Some(a) = it.artefact {
        v.push(a.to_string());
    }
    if let Some((s, _)) = &it.git {
        v.push(format!("git:{}", s.as_str()));
    }
    if v.is_empty() {
        "-".into()
    } else {
        v.join(",")
    }
}

fn trash_json(t: &TrashSummary) -> String {
    let roots: Vec<String> = t
        .roots
        .iter()
        .map(|r| format!("\"{}\"", json_escape(&r.to_string_lossy())))
        .collect();
    format!(
        "{{\"roots\":[{}],\"items\":{},\"bytes\":{}}}",
        roots.join(","),
        t.items,
        t.bytes
    )
}

/// JSON form of a report (one object; arrays of roots, dirs and files).
pub fn report_json(r: &Report, trash: &TrashSummary) -> String {
    let roots: Vec<String> = r
        .roots
        .iter()
        .map(|t| {
            format!(
                "{{\"path\":\"{}\",\"disk\":{},\"apparent\":{},\"files\":{},\"dirs\":{}}}",
                json_escape(&t.path.to_string_lossy()),
                t.disk,
                t.apparent,
                t.files,
                t.dirs
            )
        })
        .collect();
    let dirs: Vec<String> = r.dirs.iter().map(item_json).collect();
    let files: Vec<String> = r.files.iter().map(item_json).collect();
    format!(
        "{{\"roots\":[{}],\"dirs\":[{}],\"files\":[{}],\"unreadable\":{},\"trash\":{}}}",
        roots.join(","),
        dirs.join(","),
        files.join(","),
        r.unreadable,
        trash_json(trash)
    )
}

fn print_table(title: &str, items: &[Item], now: i64) {
    if items.is_empty() {
        return;
    }
    println!("{title}");
    let width = items
        .iter()
        .map(|it| notes(it).len())
        .max()
        .unwrap_or(4)
        .max(4);
    println!(
        "{:>10}  {:>10}  {:>4}  {:<width$}  PATH",
        "DISK", "APPARENT", "AGE", "NOTE"
    );
    for it in items {
        let mut path = it.path.display().to_string();
        if it.is_dir {
            path.push('/');
        }
        println!(
            "{:>10}  {:>10}  {:>4}  {:<width$}  {}",
            format_bytes(it.disk),
            format_bytes(it.apparent),
            format_age(it.mtime, now),
            notes(it),
            path
        );
    }
    println!();
}

fn print_trash(t: &TrashSummary) {
    let noun = if t.items == 1 { "item" } else { "items" };
    println!(
        "Trash on these file systems: {} in {} {noun}",
        format_bytes(t.bytes),
        t.items
    );
    if t.items > 0 {
        println!(
            "  space returns only when the trash is emptied: preview with `rtrash empty -n`, free it with `rtrash empty`, or keep the last 30 days with `rtrash empty 30`"
        );
    }
}

fn pick_available() -> bool {
    cfg!(feature = "tui") && crate::util::stdin_is_tty()
}

/// Browser rows for `--pick`: directories then files, without trash rows.
/// Rows with git work at risk carry `warn` and show `!`.
#[cfg(feature = "tui")]
pub fn pick_rows(report: &Report) -> Vec<crate::put_tui::PathRow> {
    let items: Vec<&Item> = report
        .dirs
        .iter()
        .chain(report.files.iter())
        .filter(|it| it.artefact != Some("trash"))
        .collect();
    let width = items.iter().map(|it| notes(it).len()).max().unwrap_or(1);
    items
        .into_iter()
        .map(|it| crate::put_tui::PathRow {
            path: it.path.clone(),
            is_dir: it.is_dir,
            label: Some(format!(
                "{:>10}  {:<width$}  {}",
                format_bytes(it.disk),
                notes(it),
                it.path.display()
            )),
            warn: it.git.as_ref().is_some_and(|(s, _)| s.is_risky()),
        })
        .collect()
}

fn parse_days(s: &str) -> Result<i64, String> {
    match s.parse::<i64>() {
        Ok(d) if d >= 0 => Ok(d),
        Ok(d) => Err(format!("DAYS must be >= 0 (got {d})")),
        Err(_) => Err(format!("invalid DAYS '{s}'")),
    }
}

fn parse_count(s: &str, what: &str) -> Result<usize, String> {
    s.parse::<usize>()
        .map_err(|_| format!("invalid {what} '{s}'"))
}

/// Value of `--name=V` or of the next argument for `--name V` / `-n V`.
fn take_value<'a>(
    args: &'a [String],
    i: &mut usize,
    arg: &'a str,
    long: &str,
    short: Option<&str>,
) -> Option<Result<&'a str, String>> {
    if let Some(v) = arg.strip_prefix(&format!("{long}=")) {
        return Some(Ok(v));
    }
    if arg == long || Some(arg) == short {
        *i += 1;
        return Some(
            args.get(*i)
                .map(String::as_str)
                .ok_or_else(|| format!("{arg} requires a value")),
        );
    }
    None
}

pub fn run(prog: &str, args: &[String]) -> i32 {
    let mut opts = ScanOpts::default();
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut json = false;
    let mut print0 = false;
    let mut want_dirs = true;
    let mut want_files = true;
    let mut only_dirs = false;
    let mut only_files = false;
    let mut no_more_opts = false;
    let mut use_git = true;
    let mut home_only = false;
    let mut trash_pins: Vec<PathBuf> = Vec::new();
    let mut include_dirty = false;
    let mut pick = false;
    let now = info::now_epoch();
    let mut i = 0usize;
    while i < args.len() {
        let arg = args[i].as_str();
        if no_more_opts || arg == "-" || !arg.starts_with('-') {
            paths.push(PathBuf::from(arg));
            i += 1;
            continue;
        }
        macro_rules! bail {
            ($msg:expr) => {{
                eprintln!("{prog}: {}", $msg);
                eprintln!("Try '{prog} --help' for more information.");
                return 2;
            }};
        }
        if let Some(v) = take_value(args, &mut i, arg, "--top", Some("-n")) {
            match v.and_then(|v| parse_count(v, "N")) {
                Ok(n) => opts.top = n,
                Err(e) => bail!(e),
            }
        } else if let Some(v) = take_value(args, &mut i, arg, "--max-depth", None) {
            match v.and_then(|v| parse_count(v, "depth")) {
                Ok(n) => opts.max_depth = Some(n),
                Err(e) => bail!(e),
            }
        } else if let Some(v) = take_value(args, &mut i, arg, "--older-than", None) {
            match v.and_then(parse_days) {
                Ok(d) => opts.older_cutoff = Some(now - d.saturating_mul(86_400)),
                Err(e) => bail!(e),
            }
        } else if let Some(v) = take_value(args, &mut i, arg, "--newer-than", None) {
            match v.and_then(parse_days) {
                Ok(d) => opts.newer_cutoff = Some(now - d.saturating_mul(86_400)),
                Err(e) => bail!(e),
            }
        } else {
            match arg {
                "--" => no_more_opts = true,
                "-x" | "--one-file-system" => opts.one_file_system = true,
                "--apparent" => opts.sort = SortKey::Apparent,
                "--all-dirs" => opts.all_dirs = true,
                "--artefacts" => opts.artefacts_only = true,
                "--dirs" => only_dirs = true,
                "--files" => only_files = true,
                "--json" => json = true,
                "--print0" => print0 = true,
                "--no-git" => use_git = false,
                "--home-only" => home_only = true,
                a if a.starts_with("--trash-dir=") => {
                    trash_pins.push(PathBuf::from(&a["--trash-dir=".len()..]));
                }
                "--include-dirty" => include_dirty = true,
                "--pick" => pick = true,
                "--help" => {
                    print!("{}", HELP.replace("{prog}", prog));
                    return 0;
                }
                "--version" => {
                    println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                    return 0;
                }
                a => bail!(format!("unrecognized option '{a}'")),
            }
        }
        i += 1;
    }
    if only_dirs && !only_files {
        want_files = false;
    }
    if only_files && !only_dirs {
        want_dirs = false;
    }
    if [json, print0, pick].iter().filter(|b| **b).count() > 1 {
        eprintln!("{prog}: --json, --print0 and --pick are mutually exclusive");
        return 2;
    }
    if pick && !pick_available() {
        eprintln!("{prog}: --pick needs a terminal and the tui feature");
        return 2;
    }
    if paths.is_empty() {
        paths.push(PathBuf::from("."));
    }

    let trash_dirs = trashdir::resolve_dirs_opts(&trash_pins, home_only);
    if trash_dirs.is_empty() && !trash_pins.is_empty() {
        eprintln!("{prog}: no valid --trash-dir pins (need files/ and info/ directories)");
        return 2;
    }
    opts.trash_roots = trash_dirs
        .iter()
        .map(|d| fs::canonicalize(&d.root).unwrap_or_else(|_| d.root.clone()))
        .collect();
    let mut report = match scan(&paths, &opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{prog}: cannot scan: {e}");
            return 1;
        }
    };
    if !want_dirs {
        report.dirs.clear();
    }
    if !want_files {
        report.files.clear();
    }
    if use_git && !annotate_git(&mut report) {
        eprintln!("{prog}: warning: git not found; rows carry no git state");
    }

    let trash = trash_on_scanned_devices(&trash_dirs, &report);
    #[cfg(feature = "tui")]
    if pick {
        let title = report
            .roots
            .first()
            .map(|r| r.path.clone())
            .unwrap_or_default();
        return crate::put_tui::run_rows(prog, &title, pick_rows(&report));
    }
    if json {
        println!("{}", report_json(&report, &trash));
    } else if print0 {
        let mut out = io::stdout().lock();
        let mut held = 0usize;
        for it in report.dirs.iter().chain(report.files.iter()) {
            if it.artefact == Some("trash") {
                continue;
            }
            let risky = it.git.as_ref().is_some_and(|(s, _)| s.is_risky());
            if risky && !include_dirty {
                held += 1;
                continue;
            }
            let _ = out.write_all(it.path.as_os_str().as_encoded_bytes());
            let _ = out.write_all(b"\0");
        }
        if held > 0 {
            eprintln!(
                "{prog}: left out {held} path(s) with dirty, unpushed or unknown git state (--include-dirty prints them)"
            );
        }
    } else {
        let by = match opts.sort {
            SortKey::Disk => "disk use",
            SortKey::Apparent => "apparent size",
        };
        print_table(&format!("Largest directories (by {by})"), &report.dirs, now);
        print_table(&format!("Largest files (by {by})"), &report.files, now);
        for t in &report.roots {
            println!(
                "Total {}: {} on disk, {} apparent, {} files, {} directories",
                t.path.display(),
                format_bytes(t.disk),
                format_bytes(t.apparent),
                t.files,
                t.dirs
            );
        }
        print_trash(&trash);
    }
    if report.unreadable > 0 {
        eprintln!("{prog}: {} entries could not be read", report.unreadable);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(path: &str, disk: u64, apparent: u64) -> Item {
        Item {
            path: PathBuf::from(path),
            is_dir: false,
            apparent,
            disk,
            mtime: 0,
            artefact: None,
            repos: Vec::new(),
            git: None,
        }
    }

    #[test]
    fn topn_keeps_largest_in_order() {
        let mut t = TopN::new(2, SortKey::Disk);
        t.push(item("/a", 5, 50));
        t.push(item("/b", 9, 1));
        t.push(item("/c", 1, 99));
        t.push(item("/d", 7, 0));
        let v: Vec<_> = t.into_vec().into_iter().map(|i| i.disk).collect();
        assert_eq!(v, [9, 7]);
    }

    #[test]
    fn topn_apparent_and_zero_cap() {
        let mut t = TopN::new(1, SortKey::Apparent);
        t.push(item("/a", 5, 50));
        t.push(item("/c", 1, 99));
        assert_eq!(t.into_vec()[0].path, PathBuf::from("/c"));
        let mut z = TopN::new(0, SortKey::Disk);
        z.push(item("/a", 1, 1));
        assert!(z.into_vec().is_empty());
    }

    #[test]
    fn topn_ties_sort_by_path() {
        let mut t = TopN::new(3, SortKey::Disk);
        t.push(item("/z", 4, 4));
        t.push(item("/a", 4, 4));
        t.push(item("/m", 4, 4));
        let v: Vec<_> = t.into_vec().into_iter().map(|i| i.path).collect();
        assert_eq!(v, [PathBuf::from("/a"), "/m".into(), "/z".into()]);
    }

    #[test]
    fn age_units() {
        assert_eq!(format_age(100, 130), "30s");
        assert_eq!(format_age(0, 7_200), "2h");
        assert_eq!(format_age(0, 3 * 86_400), "3d");
        assert_eq!(format_age(0, 800 * 86_400), "2y");
        assert_eq!(format_age(10, 0), "0s");
    }

    #[test]
    fn artefact_rules() {
        let none = |_: &str| false;
        assert_eq!(artefact_kind("node_modules", none), Some("node-modules"));
        assert_eq!(artefact_kind("__pycache__", none), Some("python-cache"));
        assert_eq!(artefact_kind("target", none), None);
        assert_eq!(artefact_kind("build", none), None);
        assert_eq!(
            artefact_kind("target", |c| c == "CACHEDIR.TAG"),
            Some("cargo-target")
        );
        assert_eq!(
            artefact_kind("target-pub", |c| c == ".rustc_info.json"),
            Some("cargo-target")
        );
        assert_eq!(
            artefact_kind(".venv", |c| c == "pyvenv.cfg"),
            Some("python-env")
        );
        assert_eq!(
            artefact_kind("build", |c| c == "CMakeCache.txt"),
            Some("cmake-build")
        );
        assert_eq!(
            artefact_kind("anything", |c| c == "CACHEDIR.TAG"),
            Some("cache")
        );
    }

    #[cfg(feature = "tui")]
    #[test]
    fn pick_rows_skip_trash_and_flag_risky() {
        let mut risky = item("/w/local", 9, 9);
        risky.is_dir = true;
        risky.git = Some((GitState::Unpushed, PathBuf::from("/w/local")));
        let mut trash = item("/w/Trash", 8, 8);
        trash.is_dir = true;
        trash.artefact = Some("trash");
        let report = Report {
            roots: Vec::new(),
            dirs: vec![risky, trash],
            files: vec![item("/w/f.bin", 1, 1)],
            unreadable: 0,
        };
        let rows = pick_rows(&report);
        let paths: Vec<_> = rows.iter().map(|r| r.path.clone()).collect();
        assert_eq!(
            paths,
            [PathBuf::from("/w/local"), PathBuf::from("/w/f.bin")]
        );
        assert!(rows[0].warn && !rows[1].warn);
        assert!(rows[0].label.as_deref().unwrap().contains("git:unpushed"));
    }
}
