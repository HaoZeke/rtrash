//! `rtrash du`: find what to delete. Walks one or more paths in parallel and
//! reports the largest directories and files, with on-disk and apparent sizes.

use std::collections::HashSet;
use std::ffi::CStr;
use std::fs;
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use rayon::prelude::*;

use crate::du_fs;
use crate::du_git::{self, GitState};
#[cfg(unix)]
use crate::status;
#[cfg(unix)]
use crate::trashdir::{self, TrashDir};
use crate::util::{format_bytes, json_escape, plural};

const HELP: &str = "\
Usage: {prog} [OPTION]... [PATH]...
Find what to delete: walk each PATH (default .) in parallel and list the
largest directories and files, by on-disk size unless --apparent is given.

Symlinks are never followed, and a file with more than one hard link
counts once. When one subdirectory accounts for 90% or more of a
directory's size, only the subdirectory is listed (--all-dirs lists both).
Regenerable artefacts are tagged and listed as one row each; their
contents never appear on their own, unless the artefact is the PATH
being scanned.

Each row inside or containing a git repository gets a git state: ignored,
clean, dirty (uncommitted changes, untracked files or a stash), unknown, or
unpushed (local commits no remote has). --print0 leaves out dirty,
unknown and unpushed rows unless --include-dirty is given. Paths inside
.git or a bare repository are never listed; a bare repository is one row,
and it reads unpushed when no remote has its commits.

The closing summary shows the trash on the scanned file systems: trashed
files still take space there until `rtrash empty` removes them (preview
with `rtrash empty -n`, keep recent items with `rtrash empty DAYS`). Trash
directories appear as one `trash` row and are never printed by --print0.

  -n, --top=N           list N directories and N files (default 20)
  -x, --one-file-system skip directories on other file systems
      --apparent        rank by apparent size (file length) instead of disk use
      --max-depth=N     list only entries at most N levels below PATH
      --threads=N       walk with N threads (default: one per CPU)
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

    /// Add `item` if it ranks among the kept ones; returns whether it was
    /// kept.
    pub fn push(&mut self, item: Item) -> bool {
        if self.cap == 0 {
            return false;
        }
        let k = item.key(self.sort);
        if self.items.len() == self.cap {
            // A full list keeps the smaller path on a tie, whatever order
            // the workers report in.
            match self.items.last() {
                Some(last) if (last.key(self.sort), &item.path) >= (k, &last.path) => return false,
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
        true
    }

    pub fn merge(&mut self, other: TopN) {
        self.merge_vec(other.items);
    }

    pub fn merge_vec(&mut self, items: Vec<Item>) {
        for it in items {
            self.push(it);
        }
    }

    /// Key of the smallest kept item, once the list is full.
    pub fn floor(&self) -> Option<u64> {
        if self.items.len() < self.cap {
            return None;
        }
        self.items.last().map(|x| x.key(self.sort))
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
    /// Artefact kind of the PATH itself, when it is one.
    pub artefact: Option<&'static str>,
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
#[cfg(unix)]
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

/// Summarize the Recycle Bin of each drive that holds a scan root.
#[cfg(windows)]
pub fn recycle_bin_on_scanned_drives(report: &Report) -> TrashSummary {
    let mut drives: Vec<PathBuf> = Vec::new();
    for r in &report.roots {
        let mut c = r.path.components();
        if let (Some(std::path::Component::Prefix(p)), Some(root)) = (c.next(), c.next()) {
            let d = PathBuf::from(p.as_os_str()).join(root.as_os_str());
            if !drives.contains(&d) {
                drives.push(d);
            }
        }
    }
    let mut t = TrashSummary::default();
    for d in drives {
        if let Some((items, bytes)) = crate::win_recycle::query_bin(&d) {
            t.items += items;
            t.bytes += bytes;
            t.roots.push(d);
        }
    }
    t
}

/// One list shared by every worker, with the key of its smallest kept
/// item mirrored in an atomic so most candidates are dropped without a
/// lock or an allocation.
struct SharedTop {
    list: Mutex<TopN>,
    /// Smallest kept key once the list is full, 0 before.
    floor: AtomicU64,
}

impl SharedTop {
    fn new(cap: usize, sort: SortKey) -> Self {
        SharedTop {
            list: Mutex::new(TopN::new(cap, sort)),
            floor: AtomicU64::new(0),
        }
    }

    /// Whether an item with this key could still enter the list. A key
    /// equal to the floor may win its tie on path, so it passes.
    fn wants(&self, key: u64) -> bool {
        key >= self.floor.load(Ordering::Relaxed)
    }

    /// Offer an item with this key, built only if it may get in. Returns
    /// whether it was kept. The list only ever gets harder to enter, so an
    /// item that ranks below a rejected one would be rejected as well.
    fn offer(&self, key: u64, make: impl FnOnce() -> Item) -> bool {
        if !self.wants(key) {
            return false;
        }
        let mut list = self.list.lock().unwrap_or_else(|e| e.into_inner());
        if list.cap == 0 {
            return false;
        }
        let kept = list.push(make());
        if let Some(f) = list.floor() {
            self.floor.store(f, Ordering::Relaxed);
        }
        kept
    }

    fn into_vec(self) -> Vec<Item> {
        self.list
            .into_inner()
            .unwrap_or_else(|e| e.into_inner())
            .into_vec()
    }
}

/// Hard links seen so far, as `(dev, ino)`, split over several locks so
/// workers on a tree full of links (pnpm, nix, ccache) rarely wait on
/// each other.
struct LinkSet {
    shards: Vec<Mutex<HashSet<(u64, u64)>>>,
}

impl LinkSet {
    /// Whether `st` is the first sighting of its file. See
    /// [`Ctx::first_link`] for what link count 0 and file ID 0 mean.
    fn first_link(&self, st: &du_fs::Stat) -> bool {
        st.nlink == 1 || st.ino == 0 || self.insert(st.dev, st.ino)
    }

    const SHARDS: usize = 16;

    fn new() -> Self {
        LinkSet {
            shards: (0..Self::SHARDS).map(|_| Mutex::default()).collect(),
        }
    }

    /// Record the file; returns whether it was new.
    fn insert(&self, dev: u64, ino: u64) -> bool {
        // Fibonacci hashing: inode numbers often share their low bits.
        let h = (ino ^ dev.rotate_left(32)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let shard = &self.shards[(h >> 60) as usize % Self::SHARDS];
        shard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((dev, ino))
    }
}

struct Ctx<'a> {
    opts: &'a ScanOpts,
    root_dev: u64,
    /// Multiply-linked files already counted.
    seen: LinkSet,
    unreadable: AtomicU64,
    files: AtomicU64,
    dirs: AtomicU64,
    top_dirs: SharedTop,
    top_files: SharedTop,
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
    /// A link count of 0 means the platform did not report one, so the
    /// file is tracked like a linked one. A file ID of 0 means there is no
    /// ID to compare (some file systems and Windows layers report none), so
    /// the file counts on its own rather than as a copy of every other.
    fn first_link(&self, st: &du_fs::Stat) -> bool {
        self.seen.first_link(st)
    }

    fn key(&self, apparent: u64, disk: u64) -> u64 {
        match self.opts.sort {
            SortKey::Disk => disk,
            SortKey::Apparent => apparent,
        }
    }
}

struct Node {
    apparent: u64,
    disk: u64,
    mtime: i64,
    repos: Vec<PathBuf>,
    /// The filters would let this directory onto the list. A parent only
    /// hands its row to a dominant child that can show up in its place.
    listable: bool,
    /// Artefact kind (`cargo-target`, `trash`, ...), if any.
    artefact: Option<&'static str>,
}

/// At most this many repo roots are remembered per directory.
const MAX_REPOS: usize = 64;

/// Below this depth a directory stays open while its subdirectories are
/// read, so they open relative to it. Deeper directories close first and
/// their children open by full path, which bounds open descriptors.
const FD_HOLD_DEPTH: usize = 64;

/// A file that looked big enough for the list while its directory was
/// being read; it is offered once the directory is known to be listable.
struct FileCandidate {
    entry: usize,
    apparent: u64,
    disk: u64,
    mtime: i64,
}

/// Open and walk one directory. `parent` and `name` locate it relative to
/// an open parent; without them it opens by `path`. `known` is its stat
/// when the caller already has one. Returns `None` for a directory on
/// another file system under `-x`, or one that cannot be stat-ed.
fn enter(
    ctx: &Ctx<'_>,
    parent: Option<(&du_fs::Dir, &CStr)>,
    path: &Path,
    known: Option<du_fs::Stat>,
    depth: usize,
    listing: bool,
) -> Option<Node> {
    let opened = match parent {
        Some((dir, name)) => dir.open_child(name),
        None => du_fs::Dir::open(path),
    };
    let stat_again = || match parent {
        Some((dir, name)) => dir.stat_at(name),
        None => du_fs::lstat(path),
    };
    let unreadable = || {
        ctx.unreadable.fetch_add(1, Ordering::Relaxed);
        None
    };
    let Ok(dir) = opened else {
        // Unreadable directory: its own blocks still count, as with du(1).
        let Some(st) = known.or_else(|| stat_again().ok()) else {
            return unreadable();
        };
        if !st.is_dir() || (ctx.opts.one_file_system && st.dev != ctx.root_dev) {
            return unreadable();
        }
        ctx.dirs.fetch_add(1, Ordering::Relaxed);
        ctx.unreadable.fetch_add(1, Ordering::Relaxed);
        return Some(Node {
            apparent: 0,
            disk: st.sizes().1,
            mtime: st.mtime,
            repos: Vec::new(),
            listable: false,
            artefact: None,
        });
    };
    let st = match known {
        Some(st) => st,
        None => match dir.stat() {
            Ok(st) => st,
            Err(_) => return unreadable(),
        },
    };
    if ctx.opts.one_file_system && st.dev != ctx.root_dev {
        return None;
    }
    Some(walk_dir(ctx, dir, path, &st, depth, listing))
}

/// Directories with more entries than this stat them in parallel.
const WIDE_DIR: usize = 256;
/// Entries per parallel stat job in a wide directory.
const WIDE_CHUNK: usize = 64;

/// What the stats of some entries of one directory add up to.
struct Stats {
    apparent: u64,
    disk: u64,
    mtime: i64,
    /// Subdirectories, with their stat when it was taken here (for -x or
    /// an entry of unknown type).
    subdirs: Vec<(usize, Option<du_fs::Stat>)>,
    candidates: Vec<FileCandidate>,
}

/// Stat entries `range` of `dir`: sum the files and collect the
/// subdirectories and the files big enough for the list.
fn stat_entries(
    ctx: &Ctx<'_>,
    dir: &du_fs::Dir,
    entries: &du_fs::Entries,
    range: std::ops::Range<usize>,
    depth: usize,
    list_files: bool,
) -> Stats {
    let opts = ctx.opts;
    let mut out = Stats {
        apparent: 0,
        disk: 0,
        mtime: i64::MIN,
        subdirs: Vec::new(),
        candidates: Vec::new(),
    };
    for i in range {
        let (name, kind) = entries.get(i);
        // A directory gets its stat from its own descriptor once open.
        // Under -x it is stat-ed first so a mount point is never opened.
        if kind == du_fs::Kind::Dir && !opts.one_file_system {
            out.subdirs.push((i, None));
            continue;
        }
        let Some(m) = entries.stat(i).or_else(|| dir.stat_at(name).ok()) else {
            ctx.unreadable.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        if m.is_dir() {
            if opts.one_file_system && m.dev != ctx.root_dev {
                continue;
            }
            out.subdirs.push((i, Some(m)));
            continue;
        }
        ctx.files.fetch_add(1, Ordering::Relaxed);
        if !ctx.first_link(&m) {
            continue;
        }
        let (a, d) = m.sizes();
        out.apparent = out.apparent.saturating_add(a);
        out.disk = out.disk.saturating_add(d);
        out.mtime = out.mtime.max(m.mtime);
        if list_files
            && m.is_file()
            && ctx.top_files.wants(ctx.key(a, d))
            && ctx.listable(depth + 1, m.mtime)
        {
            out.candidates.push(FileCandidate {
                entry: i,
                apparent: a,
                disk: d,
                mtime: m.mtime,
            });
        }
    }
    out
}

/// Walk one open directory. `listing` is false below `.git`, artefacts
/// and trash roots: their sizes count, but nothing inside them is listed.
fn walk_dir(
    ctx: &Ctx<'_>,
    dir: du_fs::Dir,
    path: &Path,
    st: &du_fs::Stat,
    depth: usize,
    listing: bool,
) -> Node {
    let opts = ctx.opts;
    // Like GNU du, a directory adds blocks to disk use but nothing to the
    // apparent size: its st_size is a filesystem detail, not data.
    let mut node = Node {
        apparent: 0,
        disk: st.sizes().1,
        mtime: st.mtime,
        repos: Vec::new(),
        listable: false,
        artefact: None,
    };
    ctx.dirs.fetch_add(1, Ordering::Relaxed);
    let (entries, failed) = dir.entries();
    if failed {
        ctx.unreadable.fetch_add(1, Ordering::Relaxed);
    }
    let list_files = listing && !opts.artefacts_only && opts.top > 0;
    let mut markers: Vec<&'static str> = Vec::new();
    let mut has_dot_git = false;
    let mut bare_parts = 0u8;
    for i in 0..entries.len() {
        let (name, kind) = entries.get(i);
        let bytes = name.to_bytes();
        if bytes == b".git" {
            has_dot_git = true;
        }
        if let Some(m) = MARKERS.iter().find(|m| m.as_bytes() == bytes) {
            markers.push(m);
        }
        bare_parts |= bare_part(bytes, kind);
    }
    let bare = !has_dot_git && bare_parts == BARE_ALL && is_bare_repo(path);
    if has_dot_git || bare {
        node.repos.push(path.to_path_buf());
    }
    let stat_chunk =
        |range: std::ops::Range<usize>| stat_entries(ctx, &dir, &entries, range, depth, list_files);
    let n = entries.len();
    // One thread stats a directory's entries unless there are many: then
    // chunks of them go to the pool, so a huge flat directory does not
    // serialise the scan.
    let parts: Vec<Stats> = if n > WIDE_DIR {
        (0..n.div_ceil(WIDE_CHUNK))
            .into_par_iter()
            .map(|c| stat_chunk(c * WIDE_CHUNK..n.min((c + 1) * WIDE_CHUNK)))
            .collect()
    } else {
        vec![stat_chunk(0..n)]
    };
    let mut subdirs: Vec<(usize, Option<du_fs::Stat>)> = Vec::new();
    let mut candidates: Vec<FileCandidate> = Vec::new();
    for p in parts {
        node.apparent = node.apparent.saturating_add(p.apparent);
        node.disk = node.disk.saturating_add(p.disk);
        node.mtime = node.mtime.max(p.mtime);
        subdirs.extend(p.subdirs);
        candidates.extend(p.candidates);
    }

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    // A bare repository is all git internals, like a `.git` directory.
    let is_git_dir = name == ".git";
    let closed = is_git_dir || bare;
    let artefact = if is_git_dir {
        None
    } else if opts
        .trash_roots
        .iter()
        .any(|r| r.as_os_str() == path.as_os_str())
    {
        Some("trash")
    } else {
        artefact_kind(&name, |c| markers.contains(&c))
    };
    // Repository internals are never offered for deletion, and an artefact
    // is one row: nothing below either is listed on its own. A PATH that is
    // itself an artefact is the exception: it was named to be looked into,
    // so its contents are listed. A trash root stays closed even then.
    let opens_as_root = depth == 0 && artefact != Some("trash");
    let inner_listing = listing && !closed && (artefact.is_none() || opens_as_root);
    if inner_listing {
        // Best first: the list's own order, by key and then by name since
        // the parent matches. No more than `top` of them can get in, and
        // once one is turned away the rest would be too. This matters when
        // thousands of files tie on size.
        let name_of = |c: &FileCandidate| entries.get(c.entry).0.to_bytes();
        let best_first = |x: &FileCandidate, y: &FileCandidate| {
            ctx.key(y.apparent, y.disk)
                .cmp(&ctx.key(x.apparent, x.disk))
                .then_with(|| name_of(x).cmp(name_of(y)))
        };
        let top = opts.top;
        if candidates.len() > top {
            candidates.select_nth_unstable_by(top - 1, best_first);
            candidates.truncate(top);
        }
        candidates.sort_unstable_by(best_first);
        for c in candidates {
            let kept = ctx.top_files.offer(ctx.key(c.apparent, c.disk), || Item {
                path: path.join(du_fs::os(entries.get(c.entry).0)),
                is_dir: false,
                apparent: c.apparent,
                disk: c.disk,
                mtime: c.mtime,
                artefact: None,
                repos: Vec::new(),
                git: None,
            });
            if !kept {
                break;
            }
        }
    }

    let parent = if depth < FD_HOLD_DEPTH {
        Some(dir)
    } else {
        drop(dir);
        None
    };
    let children: Vec<Option<Node>> = subdirs
        .into_par_iter()
        .map(|(i, known)| {
            let cname = entries.get(i).0;
            let child = path.join(du_fs::os(cname));
            let rel = parent.as_ref().map(|d| (d, cname));
            enter(ctx, rel, &child, known, depth + 1, inner_listing)
        })
        .collect();

    let mut largest_child = 0u64;
    for child in children.into_iter().flatten() {
        if child.listable {
            largest_child = largest_child.max(ctx.key(child.apparent, child.disk));
        }
        node.apparent = node.apparent.saturating_add(child.apparent);
        node.disk = node.disk.saturating_add(child.disk);
        node.mtime = node.mtime.max(child.mtime);
        for r in child.repos {
            if node.repos.len() < MAX_REPOS {
                node.repos.push(r);
            }
        }
    }
    node.listable = listing && !is_git_dir && depth > 0 && ctx.listable(depth, node.mtime);
    node.artefact = artefact;
    if listing && !is_git_dir {
        node.offer_self(ctx, path, depth, largest_child, artefact);
    }
    node
}

/// `HEAD`, `objects/` and `refs/`: the entries every git directory has.
const BARE_ALL: u8 = 0b111;

/// Which of the git directory entries this one is, as a bit of
/// [`BARE_ALL`]. An entry of unknown type passes; [`is_bare_repo`]
/// checks the real thing.
fn bare_part(name: &[u8], kind: du_fs::Kind) -> u8 {
    let dirish = matches!(kind, du_fs::Kind::Dir | du_fs::Kind::Unknown);
    match name {
        b"HEAD" if kind != du_fs::Kind::Dir => 0b001,
        b"objects" if dirish => 0b010,
        b"refs" if dirish => 0b100,
        _ => 0,
    }
}

/// Whether `path` holds a bare repository: git's own test, a `HEAD` that
/// names a ref or a commit next to `objects/` and `refs/`. Directories
/// inside a `.git` (submodules, worktrees) belong to that repository.
fn is_bare_repo(path: &Path) -> bool {
    if du_git::inside_git_dir(path) {
        return false;
    }
    let mut head = [0u8; 80];
    let n = fs::File::open(path.join("HEAD"))
        .and_then(|mut f| io::Read::read(&mut f, &mut head))
        .unwrap_or(0);
    let head = &head[..n];
    let hash_len = head.iter().take_while(|b| b.is_ascii_hexdigit()).count();
    let names_ref = head.starts_with(b"ref: refs/");
    let names_commit = (hash_len == 40 || hash_len == 64) && head.get(hash_len) == Some(&b'\n');
    (names_ref || names_commit) && path.join("objects").is_dir() && path.join("refs").is_dir()
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
    /// listable subdirectory already accounts for 90% or more of it.
    fn offer_self(
        &self,
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
        let own = ctx.key(self.apparent, self.disk);
        let dominated = own > 0 && largest.saturating_mul(10) >= own.saturating_mul(9);
        if artefact.is_none() && !ctx.opts.all_dirs && dominated {
            return;
        }
        ctx.top_dirs.offer(own, || Item {
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
    du_fs::raise_nofile_limit();
    let mut roots = Vec::new();
    let mut dirs = TopN::new(opts.top, opts.sort);
    let mut files = TopN::new(opts.top, opts.sort);
    let mut unreadable = 0u64;
    for p in paths {
        let abs = absolute(p)?;
        let meta = du_fs::lstat(&abs)?;
        let ctx = Ctx {
            opts,
            root_dev: meta.dev,
            seen: LinkSet::new(),
            unreadable: AtomicU64::new(0),
            files: AtomicU64::new(0),
            dirs: AtomicU64::new(0),
            top_dirs: SharedTop::new(opts.top, opts.sort),
            top_files: SharedTop::new(opts.top, opts.sort),
        };
        let (apparent, disk, artefact) = if meta.is_dir() {
            enter(&ctx, None, &abs, Some(meta), 0, true)
                .map(|n| (n.apparent, n.disk, n.artefact))
                .unwrap_or((0, 0, None))
        } else {
            ctx.files.fetch_add(1, Ordering::Relaxed);
            let (a, d) = meta.sizes();
            if meta.is_file() && !opts.artefacts_only && ctx.listable(0, meta.mtime) {
                ctx.top_files.offer(ctx.key(a, d), || Item {
                    path: abs.clone(),
                    is_dir: false,
                    apparent: a,
                    disk: d,
                    mtime: meta.mtime,
                    artefact: None,
                    repos: Vec::new(),
                    git: None,
                });
            }
            (a, d, None)
        };
        unreadable += ctx.unreadable.load(Ordering::Relaxed);
        roots.push(RootTotals {
            path: abs,
            apparent,
            disk,
            files: ctx.files.load(Ordering::Relaxed),
            dirs: ctx.dirs.load(Ordering::Relaxed),
            artefact,
        });
        dirs.merge_vec(ctx.top_dirs.into_vec());
        files.merge_vec(ctx.top_files.into_vec());
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
        return canonical(p);
    }
    let parent = match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => canonical(d)?,
        _ => std::env::current_dir()?,
    };
    Ok(parent.join(p.file_name().unwrap_or_default()))
}

#[cfg(unix)]
fn canonical(p: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(p)
}

/// Canonical path without the verbatim `\\?\` prefix that
/// `fs::canonicalize` adds on Windows, so rows print as users type them.
#[cfg(windows)]
fn canonical(p: &Path) -> io::Result<PathBuf> {
    let c = fs::canonicalize(p)?;
    Ok(strip_verbatim(&c))
}

#[cfg(any(windows, test))]
fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match s.strip_prefix(r"\\?\") {
        // Only a drive path has a plain form: `C:\...`.
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => p.to_path_buf(),
    }
}

/// Fill in `git` for every listed row. Returns false when `git` could not
/// be run at all.
pub fn annotate_git(report: &mut Report) -> bool {
    let states = {
        let queries: Vec<du_git::RowQuery<'_>> = report
            .dirs
            .iter()
            .chain(report.files.iter())
            .filter(|it| !du_git::inside_git_dir(&it.path))
            .map(|it| du_git::RowQuery {
                path: &it.path,
                is_dir: it.is_dir,
                repos: &it.repos,
            })
            .collect();
        du_git::classify_all(&queries)
    };
    let Some(states) = states else {
        return false;
    };
    let rows = report
        .dirs
        .iter_mut()
        .chain(report.files.iter_mut())
        .filter(|it| !du_git::inside_git_dir(&it.path));
    for (it, s) in rows.zip(states) {
        it.git = s;
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
                "{{\"path\":\"{}\",\"disk\":{},\"apparent\":{},\"files\":{},\"dirs\":{},\"artefact\":{}}}",
                json_escape(&t.path.to_string_lossy()),
                t.disk,
                t.apparent,
                t.files,
                t.dirs,
                json_opt(t.artefact)
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
            path.push(std::path::MAIN_SEPARATOR);
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
    println!(
        "Trash on these file systems: {} in {}",
        format_bytes(t.bytes),
        plural(t.items, "item", "items")
    );
    if t.items > 0 {
        println!(
            "  space returns only when the trash is emptied: preview with `rtrash empty -n`, free it with `rtrash empty`, or keep the last 30 days with `rtrash empty 30`"
        );
    }
}

fn pick_available() -> bool {
    cfg!(all(unix, feature = "tui")) && crate::util::stdin_is_tty()
}

/// Browser rows for `--pick`: directories then files, without trash rows.
/// Rows with git work at risk carry `warn` and show `!`.
#[cfg(all(unix, feature = "tui"))]
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
    let mut threads: Option<usize> = None;
    let now = crate::util::now_epoch();
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
        } else if let Some(v) = take_value(args, &mut i, arg, "--threads", None) {
            match v.and_then(|v| parse_count(v, "N")) {
                Ok(0) => bail!("--threads must be at least 1"),
                Ok(n) => threads = Some(n),
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

    #[cfg(windows)]
    {
        // The Recycle Bin has no FreeDesktop layout to pin.
        let _ = (home_only, &trash_pins);
    }
    #[cfg(unix)]
    let trash_dirs = trashdir::resolve_dirs_opts(&trash_pins, home_only);
    #[cfg(unix)]
    if trash_dirs.is_empty() && !trash_pins.is_empty() {
        eprintln!("{prog}: no valid --trash-dir pins (need files/ and info/ directories)");
        return 2;
    }
    #[cfg(unix)]
    {
        opts.trash_roots = trash_dirs
            .iter()
            .map(|d| fs::canonicalize(&d.root).unwrap_or_else(|_| d.root.clone()))
            .collect();
    }
    let scanned = match threads {
        Some(n) => match rayon::ThreadPoolBuilder::new().num_threads(n).build() {
            Ok(pool) => pool.install(|| scan(&paths, &opts)),
            Err(e) => {
                eprintln!("{prog}: cannot start {n} threads: {e}");
                return 1;
            }
        },
        None => scan(&paths, &opts),
    };
    let mut report = match scanned {
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

    #[cfg(unix)]
    let trash = trash_on_scanned_devices(&trash_dirs, &report);
    #[cfg(windows)]
    let trash = recycle_bin_on_scanned_drives(&report);
    #[cfg(all(unix, feature = "tui"))]
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
            let kind = t.artefact.map(|k| format!(" ({k})")).unwrap_or_default();
            println!(
                "Total {}{kind}: {} on disk, {} apparent, {}, {}",
                t.path.display(),
                format_bytes(t.disk),
                format_bytes(t.apparent),
                plural(t.files, "file", "files"),
                plural(t.dirs, "directory", "directories")
            );
        }
        print_trash(&trash);
    }
    if report.unreadable > 0 {
        eprintln!(
            "{prog}: {} could not be read",
            plural(report.unreadable, "entry", "entries")
        );
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
    fn verbatim_prefix_is_dropped_only_where_a_plain_form_exists() {
        let p = |s: &str| strip_verbatim(Path::new(s));
        assert_eq!(p(r"\\?\C:\Users\me"), PathBuf::from(r"C:\Users\me"));
        assert_eq!(p(r"\\?\UNC\srv\share\x"), PathBuf::from(r"\\srv\share\x"));
        assert_eq!(p(r"\\?\Volume{1}\x"), PathBuf::from(r"\\?\Volume{1}\x"));
        assert_eq!(p("/home/me"), PathBuf::from("/home/me"));
    }

    #[test]
    fn link_set_counts_each_inode_once_across_shards() {
        let set = LinkSet::new();
        let mut used = HashSet::new();
        for ino in 0..4096u64 {
            assert!(set.insert(7, ino));
            assert!(!set.insert(7, ino), "second link of {ino}");
            // The same inode number on another device is another file.
            assert!(set.insert(8, ino));
        }
        for (i, s) in set.shards.iter().enumerate() {
            if !s.lock().unwrap().is_empty() {
                used.insert(i);
            }
        }
        assert_eq!(used.len(), LinkSet::SHARDS, "sequential inodes spread out");
    }

    #[test]
    fn file_id_zero_never_merges_files() {
        let set = LinkSet::new();
        let st = |nlink, ino| du_fs::Stat {
            mode: 0o100_000, // regular file
            size: 10,
            blocks: 1,
            nlink,
            ino,
            dev: 3,
            mtime: 0,
        };
        // No link count and no file ID, as on a volume without IDs: every
        // file counts.
        for _ in 0..3 {
            assert!(set.first_link(&st(0, 0)));
        }
        // No link count but a real ID: the second sighting is a hard link.
        assert!(set.first_link(&st(0, 42)));
        assert!(!set.first_link(&st(0, 42)));
        assert!(set.first_link(&st(2, 43)));
        assert!(!set.first_link(&st(2, 43)));
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
    fn topn_full_list_ties_keep_smaller_path_in_any_order() {
        let mut t = TopN::new(2, SortKey::Disk);
        t.push(item("/z", 4, 4));
        t.push(item("/y", 4, 4));
        t.push(item("/a", 4, 4));
        t.push(item("/zz", 4, 4));
        let v: Vec<_> = t.into_vec().into_iter().map(|i| i.path).collect();
        assert_eq!(v, [PathBuf::from("/a"), "/y".into()]);
        assert_eq!(
            {
                let mut f = TopN::new(1, SortKey::Disk);
                f.push(item("/b", 4, 4));
                f.floor()
            },
            Some(4)
        );
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

    #[cfg(all(unix, feature = "tui"))]
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
