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
use std::sync::{Arc, Mutex};

use rayon::prelude::*;

use crate::du_filter::{self, Chain};
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
counts once, in the sizes and in the file count (as with du --inodes);
the total names the extra links. When one subdirectory accounts for 90% or more of a
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
      --sort=KEY        rank by KEY: disk (default), apparent, items (files
                          and directories below each directory; lists no
                          files) or mtime (newest modification first; a
                          directory gives way to the subdirectory that has
                          its newest change)
  -C, --count           add an ITEMS column (each hard-linked file once)
      --unique          add a UNIQUE column: the bytes in extents no other
                          file shares, roughly what deleting a row frees on
                          btrfs, XFS or APFS (reflinks, clones, snapshots);
                          slower, since it maps every file's extents
      --max-depth=N     list only entries at most N levels below PATH
      --threads=N       walk with N threads (default: one per CPU, or three
                          per CPU up to 64 when the first PATH looks uncached)
      --all-dirs        list every directory level, not just the one that
                          holds the bulk of its parent
      --artefacts       list only regenerable build artefacts and caches
                          (cargo target dirs, node_modules, __pycache__,
                          virtualenvs, CACHEDIR.TAG directories, ...)
      --exclude=PATTERN leave out entries that match PATTERN (repeatable;
                          gitignore syntax: a name at any depth, or a path
                          relative to PATH when it has a '/'; `dir/` matches
                          directories only)
  -X, --exclude-from=FILE  read patterns from FILE, one per line (# comments
                          and !negations as in .gitignore; - is stdin)
      --exclude-caches  leave out directories below PATH that hold a valid
                          CACHEDIR.TAG
      --gitignore       leave out what git ignores: .gitignore files,
                          .git/info/exclude and ~/.config/git/ignore, inside
                          git repositories
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
  -o, --output=FILE     write a snapshot of every directory total (to
                          --max-depth) to FILE (- for stdout) instead of
                          listing; compare snapshots with `du diff`
  -f, --file=FILE       list from a snapshot instead of scanning
      --stacks          print folded stacks of every directory's own size
                          for flame graph tools (flamegraph.pl, inferno,
                          speedscope) instead of listing; works with -f
      --pick            open the put browser on the listed rows: mark rows
                          and trash them (needs a terminal)
  -i, --interactive     explore PATH one directory at a time: open and leave
                          directories, trash rows (d), undo (u), rescan (r),
                          sort (s, n, C, M), switch disk use and apparent
                          size (S), show details (i) and filter by name (/);
                          rows show git state and artefact kind
      --no-delete       with -i, browse only: d and u do nothing
      --si              show sizes in powers of 1000 (kB, MB, GB)
      --help            display this help and exit
      --version         output version information and exit
";

/// Every artefact kind a row can carry, `trash` included.
pub const ARTEFACT_KINDS: &[&str] = &[
    "cargo-target",
    "node-modules",
    "python-cache",
    "python-env",
    "gradle-cache",
    "js-build-cache",
    "zig-cache",
    "pixi-env",
    "direnv",
    "cmake-build",
    "cache",
    "trash",
];

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

/// What orders the lists, largest (or newest) first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Disk,
    Apparent,
    /// Files and directories below a directory.
    Items,
    /// Newest modification below a directory.
    Mtime,
}

impl SortKey {
    pub fn parse(s: &str) -> Option<SortKey> {
        Some(match s {
            "disk" | "size" => SortKey::Disk,
            "apparent" => SortKey::Apparent,
            "items" | "count" => SortKey::Items,
            "mtime" | "time" | "age" => SortKey::Mtime,
            _ => return None,
        })
    }

    /// The table heading for a list of `what` ordered by this key.
    pub fn title(self, what: &str) -> String {
        match self {
            SortKey::Disk => format!("Largest {what} (by disk use)"),
            SortKey::Apparent => format!("Largest {what} (by apparent size)"),
            SortKey::Items => format!("{} with the most items", capitalize(what)),
            SortKey::Mtime => format!("Most recently modified {what}"),
        }
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// The ranking value of an entry under `sort`. Times map to `u64` in
/// order, so the same largest-first list keeps the newest.
pub fn sort_key(sort: SortKey, apparent: u64, disk: u64, items: u64, mtime: i64) -> u64 {
    match sort {
        SortKey::Disk => disk,
        SortKey::Apparent => apparent,
        SortKey::Items => items,
        SortKey::Mtime => (mtime as u64) ^ (1 << 63),
    }
}

/// Live counters of one scan, read by whoever runs it in the background
/// (the `du -i` explorer shows them while a directory is still scanning),
/// and a request to stop it early.
#[derive(Debug, Default)]
pub struct ScanControl {
    /// Files counted so far, each hard-linked file once.
    pub files: AtomicU64,
    /// Disk use of those files.
    pub disk: AtomicU64,
    /// Set to stop: directories not yet entered are skipped, and the
    /// report comes back marked partial.
    pub stop: std::sync::atomic::AtomicBool,
}

impl ScanControl {
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
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
    /// Collect no file rows (snapshots keep directories only).
    pub no_files: bool,
    /// `--exclude`, `--exclude-caches` and `--gitignore`.
    pub filter: Option<Arc<du_filter::Spec>>,
    /// `--unique`: map file extents to find the bytes no other file shares.
    pub unique: bool,
    /// Counters to update as the walk goes (one add per directory chunk).
    pub control: Option<Arc<ScanControl>>,
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
            no_files: false,
            filter: None,
            unique: false,
            control: None,
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
    /// Files and directories below a directory (a file has 1). Each
    /// hard-linked file counts once.
    pub items: u64,
    /// Kind of regenerable artefact (`cargo-target`, `node-modules`, ...).
    pub artefact: Option<&'static str>,
    /// Git repositories at or below this directory (capped).
    pub repos: Vec<PathBuf>,
    /// Riskiest git state and the repo that decided it (after `annotate_git`).
    pub git: Option<(GitState, PathBuf)>,
    /// Allocated bytes no other file shares (`--unique` scans only).
    pub unique: Option<u64>,
}

impl Item {
    pub fn key(&self, sort: SortKey) -> u64 {
        sort_key(sort, self.apparent, self.disk, self.items, self.mtime)
    }
}

/// Lists up to this long keep sorted order on every insert.
const SORTED_INSERT_CAP: usize = 1024;

/// The N largest items seen so far, largest first.
///
/// Above [`SORTED_INSERT_CAP`], new items go into a buffer that is sorted
/// and cut back to N once it holds twice that many, so a large N (a
/// snapshot keeps every directory) costs O(n log n) rather than one
/// insertion shift per item.
#[derive(Debug, Clone)]
pub struct TopN {
    cap: usize,
    sort: SortKey,
    items: Vec<Item>,
    /// Smallest kept item after the last cut, once N items survived it.
    floor: Option<(u64, PathBuf)>,
}

impl TopN {
    pub fn new(cap: usize, sort: SortKey) -> Self {
        TopN {
            cap,
            sort,
            items: Vec::new(),
            floor: None,
        }
    }

    /// Add `item` unless it ranks below the floor; returns whether it was
    /// taken. Ties go to the smaller path, whatever order the workers
    /// report in, so output stays deterministic.
    pub fn push(&mut self, item: Item) -> bool {
        if self.cap == 0 {
            return false;
        }
        let k = item.key(self.sort);
        if let Some((fk, fp)) = &self.floor {
            if (*fk, &item.path) >= (k, fp) {
                return false;
            }
        }
        if self.cap <= SORTED_INSERT_CAP {
            // Small lists stay sorted, so the floor is known as soon as
            // the list fills and workers prune from the start.
            let sort = self.sort;
            let pos = self.items.partition_point(|x| {
                let xk = x.key(sort);
                xk > k || (xk == k && x.path < item.path)
            });
            self.items.insert(pos, item);
            self.items.truncate(self.cap);
            if self.items.len() == self.cap {
                self.floor = self.items.last().map(|x| (x.key(sort), x.path.clone()));
            }
            return true;
        }
        self.items.push(item);
        if self.items.len() >= self.cap.saturating_mul(2) {
            self.cut();
        }
        true
    }

    /// Sort largest first (ties by path) and keep the best N.
    fn cut(&mut self) {
        let sort = self.sort;
        self.items.sort_unstable_by(|a, b| {
            b.key(sort)
                .cmp(&a.key(sort))
                .then_with(|| a.path.cmp(&b.path))
        });
        self.items.truncate(self.cap);
        if self.items.len() == self.cap {
            self.floor = self.items.last().map(|x| (x.key(sort), x.path.clone()));
        }
    }

    pub fn merge(&mut self, other: TopN) {
        self.merge_vec(other.items);
    }

    pub fn merge_vec(&mut self, items: Vec<Item>) {
        for it in items {
            self.push(it);
        }
    }

    /// Key of the smallest kept item once the list holds N (for a large N,
    /// as of the last cut). Items below it can never get in.
    pub fn floor(&self) -> Option<u64> {
        self.floor.as_ref().map(|f| f.0)
    }

    pub fn into_vec(mut self) -> Vec<Item> {
        self.cut();
        self.items
    }
}

/// Totals for one scanned PATH.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootTotals {
    pub path: PathBuf,
    pub apparent: u64,
    pub disk: u64,
    /// Files, each counted once however many hard links it has (as
    /// `du --inodes` counts them).
    pub files: u64,
    /// Names that pointed at a file already counted: extra hard links.
    pub links: u64,
    pub dirs: u64,
    /// Artefact kind of the PATH itself, when it is one.
    pub artefact: Option<&'static str>,
    /// Entries left out by the filters (not opened, not counted).
    pub excluded: u64,
    /// Bytes in extents no other file shares (`--unique` scans only).
    pub unique: Option<u64>,
}

/// Result of scanning one or more paths.
#[derive(Debug, Clone)]
pub struct Report {
    pub roots: Vec<RootTotals>,
    pub dirs: Vec<Item>,
    pub files: Vec<Item>,
    /// Entries that could not be read (permission denied, vanished).
    pub unreadable: u64,
    /// `--unique` scans: bytes of the shared extents the scanned files
    /// point at, each extent counted once, and with every reference.
    pub shared: Option<SharedTotals>,
    /// The scan was stopped early: sizes are lower bounds and some
    /// directories are missing.
    pub partial: bool,
}

/// What the shared extents of a `--unique` scan add up to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SharedTotals {
    /// Space they take, each extent counted once.
    pub distinct: u64,
    /// Bytes the files reference, with repeats.
    pub referenced: u64,
}

/// Trash on the file systems that were scanned: emptying it is what
/// actually returns space there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrashSummary {
    pub roots: Vec<PathBuf>,
    pub items: u64,
    pub bytes: u64,
    /// Windows: the Recycle Bin caps of those drives added up, when every
    /// one sets `MaxCapacity`.
    pub cap: Option<u64>,
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
        cap: None,
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
    let mut t = TrashSummary {
        cap: Some(0),
        ..TrashSummary::default()
    };
    for d in drives {
        if let Some((items, bytes)) = crate::win_recycle::query_bin(&d) {
            t.items += items;
            t.bytes += bytes;
            let q = crate::win_recycle::bin_quota(&d);
            t.cap = match (t.cap, q.max_bytes) {
                (Some(sum), Some(cap)) => Some(sum.saturating_add(cap)),
                _ => None,
            };
            t.roots.push(d);
        }
    }
    if t.roots.is_empty() {
        t.cap = None;
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
            // An empty list wants nothing, so workers never take the lock.
            floor: AtomicU64::new(if cap == 0 { u64::MAX } else { 0 }),
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
    /// Git probes to start as repositories turn up.
    prefetch: Option<&'a du_git::Prefetch>,
    root_dev: u64,
    /// Multiply-linked files already counted.
    seen: LinkSet,
    unreadable: AtomicU64,
    /// Files counted, each hard-linked file once.
    files: AtomicU64,
    /// Further names of files already counted.
    links: AtomicU64,
    dirs: AtomicU64,
    /// Filters for this root, when any are active.
    filter: Option<du_filter::Filter>,
    /// Entries the filters left out.
    excluded: AtomicU64,
    top_dirs: SharedTop,
    top_files: SharedTop,
    /// Shared extents met so far (`--unique`).
    shared: std::sync::Mutex<crate::du_extent::SharedSet>,
    /// A directory was skipped because the scan was asked to stop.
    cut: std::sync::atomic::AtomicBool,
}

impl Ctx<'_> {
    /// Bytes of a counted file that no other file shares; `disk` without
    /// `--unique` or an extent map. Shared extents go to `self.shared`.
    fn unique_of(&self, dir: &du_fs::Dir, name: &CStr, st: &du_fs::Stat, disk: u64) -> u64 {
        #[cfg(unix)]
        {
            if !st.is_file() || disk == 0 {
                return disk;
            }
            let Ok(f) = dir.open_file(name) else {
                return disk;
            };
            self.note_shares(st.dev, crate::du_extent::file_share(&f, disk))
        }
        #[cfg(not(unix))]
        {
            let _ = (dir, name, st);
            disk
        }
    }

    #[cfg(unix)]
    fn note_shares(&self, dev: u64, s: crate::du_extent::FileShare) -> u64 {
        if !s.shared.is_empty() {
            if let Ok(mut set) = self.shared.lock() {
                set.add(dev, &s.shared);
            }
        }
        s.unique
    }

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

    fn key(&self, apparent: u64, disk: u64, items: u64, mtime: i64) -> u64 {
        sort_key(self.opts.sort, apparent, disk, items, mtime)
    }
}

struct Node {
    apparent: u64,
    disk: u64,
    /// Bytes no other file shares (`--unique`), else 0.
    unique: u64,
    mtime: i64,
    /// Files and directories below this one.
    items: u64,
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
    unique: u64,
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
    chain: Option<&Arc<Chain>>,
) -> Option<Node> {
    // Stopped: skip what is not entered yet. The PATH itself still lists
    // its own files, so a stop always leaves something to show.
    if depth > 0 && ctx.opts.control.as_ref().is_some_and(|c| c.stopped()) {
        ctx.cut.store(true, Ordering::Relaxed);
        return None;
    }
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
            unique: st.sizes().1,
            mtime: st.mtime,
            items: 0,
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
    walk_dir(ctx, dir, path, &st, depth, listing, chain)
}

/// Directories with more entries than this stat them in parallel.
const WIDE_DIR: usize = 256;
/// Entries per parallel stat job in a wide directory.
const WIDE_CHUNK: usize = 64;

/// What the stats of some entries of one directory add up to.
struct Stats {
    apparent: u64,
    disk: u64,
    unique: u64,
    mtime: i64,
    /// Files counted (each hard-linked file once).
    files: u64,
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
    (path, chain): (&Path, Option<&Chain>),
) -> Stats {
    let opts = ctx.opts;
    let mut out = Stats {
        apparent: 0,
        disk: 0,
        unique: 0,
        mtime: i64::MIN,
        files: 0,
        subdirs: Vec::new(),
        candidates: Vec::new(),
    };
    let filter = ctx.filter.as_ref();
    let excluded = |name: &CStr, is_dir: bool| {
        let out = filter.is_some_and(|f| f.excluded(chain, path, name.to_bytes(), is_dir));
        if out {
            ctx.excluded.fetch_add(1, Ordering::Relaxed);
        }
        out
    };
    for i in range {
        let (name, kind) = entries.get(i);
        if filter.is_some()
            && kind != du_fs::Kind::Unknown
            && excluded(name, kind == du_fs::Kind::Dir)
        {
            continue;
        }
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
        if filter.is_some() && kind == du_fs::Kind::Unknown && excluded(name, m.is_dir()) {
            continue;
        }
        if m.is_dir() {
            if opts.one_file_system && m.dev != ctx.root_dev {
                continue;
            }
            out.subdirs.push((i, Some(m)));
            continue;
        }
        if !ctx.first_link(&m) {
            ctx.links.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        ctx.files.fetch_add(1, Ordering::Relaxed);
        out.files += 1;
        let (a, d) = m.sizes();
        out.apparent = out.apparent.saturating_add(a);
        out.disk = out.disk.saturating_add(d);
        let u = if opts.unique {
            ctx.unique_of(dir, name, &m, d)
        } else {
            0
        };
        out.unique = out.unique.saturating_add(u);
        out.mtime = out.mtime.max(m.mtime);
        if list_files
            && m.is_file()
            && ctx.top_files.wants(ctx.key(a, d, 1, m.mtime))
            && ctx.listable(depth + 1, m.mtime)
        {
            out.candidates.push(FileCandidate {
                entry: i,
                apparent: a,
                disk: d,
                unique: u,
                mtime: m.mtime,
            });
        }
    }
    if let Some(c) = &opts.control {
        c.files.fetch_add(out.files, Ordering::Relaxed);
        c.disk.fetch_add(out.disk, Ordering::Relaxed);
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
    chain: Option<&Arc<Chain>>,
) -> Option<Node> {
    let opts = ctx.opts;
    // Like GNU du, a directory adds blocks to disk use but nothing to the
    // apparent size: its st_size is a filesystem detail, not data.
    let mut node = Node {
        apparent: 0,
        disk: st.sizes().1,
        // A directory's own blocks belong to it alone.
        unique: if opts.unique { st.sizes().1 } else { 0 },
        mtime: st.mtime,
        items: 0,
        repos: Vec::new(),
        listable: false,
        artefact: None,
    };
    let (entries, failed) = dir.entries();
    let list_files = listing && !opts.artefacts_only && opts.top > 0;
    let mut markers: Vec<&'static str> = Vec::new();
    let mut has_dot_git = false;
    let mut bare_parts = 0u8;
    let mut has_gitignore = false;
    for i in 0..entries.len() {
        let (name, kind) = entries.get(i);
        let bytes = name.to_bytes();
        if bytes == b".git" {
            has_dot_git = true;
        }
        has_gitignore |= bytes == b".gitignore";
        if let Some(m) = MARKERS.iter().find(|m| m.as_bytes() == bytes) {
            markers.push(m);
        }
        bare_parts |= bare_part(bytes, kind);
    }
    let mut chain = chain.cloned();
    if let Some(f) = &ctx.filter {
        // A tagged cache below PATH is left out whole, as with
        // `tar --exclude-caches-all`.
        if f.exclude_caches
            && depth > 0
            && markers.contains(&"CACHEDIR.TAG")
            && du_filter::has_cachedir_tag(path)
        {
            ctx.excluded.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        chain = f.chain_for(chain.as_ref(), path, has_dot_git, has_gitignore);
    }
    ctx.dirs.fetch_add(1, Ordering::Relaxed);
    if failed {
        ctx.unreadable.fetch_add(1, Ordering::Relaxed);
    }
    let bare = !has_dot_git && bare_parts == BARE_ALL && is_bare_repo(path);
    if has_dot_git || bare {
        node.repos.push(path.to_path_buf());
        if let Some(p) = ctx.prefetch {
            p.repo(path);
        }
    }
    let stat_chunk = |range: std::ops::Range<usize>| {
        stat_entries(
            ctx,
            &dir,
            &entries,
            range,
            depth,
            list_files,
            (path, chain.as_deref()),
        )
    };
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
        node.unique = node.unique.saturating_add(p.unique);
        node.mtime = node.mtime.max(p.mtime);
        node.items += p.files;
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
            ctx.key(y.apparent, y.disk, 1, y.mtime)
                .cmp(&ctx.key(x.apparent, x.disk, 1, x.mtime))
                .then_with(|| name_of(x).cmp(name_of(y)))
        };
        let top = opts.top;
        if candidates.len() > top {
            candidates.select_nth_unstable_by(top - 1, best_first);
            candidates.truncate(top);
        }
        candidates.sort_unstable_by(best_first);
        for c in candidates {
            let key = ctx.key(c.apparent, c.disk, 1, c.mtime);
            let kept = ctx.top_files.offer(key, || Item {
                path: path.join(du_fs::os(entries.get(c.entry).0)),
                is_dir: false,
                apparent: c.apparent,
                disk: c.disk,
                mtime: c.mtime,
                items: 1,
                artefact: None,
                repos: Vec::new(),
                git: None,
                unique: opts.unique.then_some(c.unique),
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
            enter(
                ctx,
                rel,
                &child,
                known,
                depth + 1,
                inner_listing,
                chain.as_ref(),
            )
        })
        .collect();

    let mut largest_child = 0u64;
    for child in children.into_iter().flatten() {
        if child.listable {
            let k = ctx.key(child.apparent, child.disk, child.items, child.mtime);
            largest_child = largest_child.max(k);
        }
        node.items += 1 + child.items;
        node.apparent = node.apparent.saturating_add(child.apparent);
        node.disk = node.disk.saturating_add(child.disk);
        node.unique = node.unique.saturating_add(child.unique);
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
    Some(node)
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
pub(crate) fn is_bare_repo(path: &Path) -> bool {
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
        let own = ctx.key(self.apparent, self.disk, self.items, self.mtime);
        let dominated = match ctx.opts.sort {
            // The newest change lies in a subdirectory: list that instead.
            SortKey::Mtime => largest == own,
            _ => own > 0 && largest.saturating_mul(10) >= own.saturating_mul(9),
        };
        if artefact.is_none() && !ctx.opts.all_dirs && dominated {
            return;
        }
        ctx.top_dirs.offer(own, || Item {
            path: path.to_path_buf(),
            is_dir: true,
            apparent: self.apparent,
            disk: self.disk,
            mtime: self.mtime,
            items: self.items,
            artefact,
            repos: self.repos.clone(),
            git: None,
            unique: ctx.opts.unique.then_some(self.unique),
        });
    }
}

/// Scan `paths` and return the merged report. Paths are made absolute first
/// (without resolving a final symlink) so listed paths work from any cwd.
pub fn scan(paths: &[PathBuf], opts: &ScanOpts) -> io::Result<Report> {
    scan_with(paths, opts, None)
}

/// [`scan`], starting git probes on `prefetch` for every repository the
/// walk meets and for the one enclosing each PATH.
pub fn scan_with(
    paths: &[PathBuf],
    opts: &ScanOpts,
    prefetch: Option<&du_git::Prefetch>,
) -> io::Result<Report> {
    du_fs::raise_nofile_limit();
    let mut roots = Vec::new();
    let mut dirs = TopN::new(opts.top, opts.sort);
    // Every file holds one item, so a count ranks no files.
    let file_cap = if opts.no_files || opts.sort == SortKey::Items {
        0
    } else {
        opts.top
    };
    let mut files = TopN::new(file_cap, opts.sort);
    let mut unreadable = 0u64;
    let mut shared = crate::du_extent::SharedSet::default();
    let mut partial = false;
    for p in paths {
        let abs = absolute(p)?;
        let meta = du_fs::lstat(&abs)?;
        if let Some(p) = prefetch {
            if let Some(repo) = du_git::enclosing_repo(&abs) {
                p.repo(&repo);
            }
        }
        let ctx = Ctx {
            opts,
            prefetch,
            root_dev: meta.dev,
            seen: LinkSet::new(),
            unreadable: AtomicU64::new(0),
            files: AtomicU64::new(0),
            links: AtomicU64::new(0),
            dirs: AtomicU64::new(0),
            filter: opts
                .filter
                .as_deref()
                .filter(|s| s.is_active())
                .map(|s| du_filter::Filter::new(s, &abs)),
            excluded: AtomicU64::new(0),
            top_dirs: SharedTop::new(opts.top, opts.sort),
            top_files: SharedTop::new(file_cap, opts.sort),
            shared: Default::default(),
            cut: Default::default(),
        };
        let (apparent, disk, unique, artefact) = if meta.is_dir() {
            enter(
                &ctx,
                None,
                &abs,
                Some(meta),
                0,
                true,
                ctx.filter.as_ref().and_then(|f| f.root_chain.as_ref()),
            )
            .map(|n| (n.apparent, n.disk, n.unique, n.artefact))
            .unwrap_or((0, 0, 0, None))
        } else {
            ctx.files.fetch_add(1, Ordering::Relaxed);
            let (a, d) = meta.sizes();
            let u = if opts.unique {
                root_file_unique(&ctx, &abs, &meta, d)
            } else {
                0
            };
            if meta.is_file() && !opts.artefacts_only && ctx.listable(0, meta.mtime) {
                ctx.top_files.offer(ctx.key(a, d, 1, meta.mtime), || Item {
                    path: abs.clone(),
                    is_dir: false,
                    apparent: a,
                    disk: d,
                    mtime: meta.mtime,
                    items: 1,
                    artefact: None,
                    repos: Vec::new(),
                    git: None,
                    unique: opts.unique.then_some(u),
                });
            }
            (a, d, u, None)
        };
        unreadable += ctx.unreadable.load(Ordering::Relaxed);
        partial |= ctx.cut.load(Ordering::Relaxed);
        roots.push(RootTotals {
            path: abs,
            apparent,
            disk,
            files: ctx.files.load(Ordering::Relaxed),
            links: ctx.links.load(Ordering::Relaxed),
            dirs: ctx.dirs.load(Ordering::Relaxed),
            artefact,
            excluded: ctx.excluded.load(Ordering::Relaxed),
            unique: opts.unique.then_some(unique),
        });
        dirs.merge_vec(ctx.top_dirs.into_vec());
        files.merge_vec(ctx.top_files.into_vec());
        if let Ok(s) = ctx.shared.into_inner() {
            shared.merge(s);
        }
    }
    Ok(Report {
        roots,
        dirs: dirs.into_vec(),
        files: files.into_vec(),
        unreadable,
        shared: opts.unique.then(|| SharedTotals {
            distinct: shared.distinct(),
            referenced: shared.referenced,
        }),
        partial,
    })
}

/// Unique bytes of a PATH that is itself a file.
fn root_file_unique(ctx: &Ctx<'_>, path: &Path, st: &du_fs::Stat, disk: u64) -> u64 {
    #[cfg(unix)]
    {
        if !st.is_file() || disk == 0 {
            return disk;
        }
        match fs::File::open(path) {
            Ok(f) => ctx.note_shares(st.dev, crate::du_extent::file_share(&f, disk)),
            Err(_) => disk,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (ctx, path, st);
        disk
    }
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

/// Entries the cold-cache probe stats, and subdirectories it lists.
const PROBE_STATS: usize = 48;
const PROBE_SUBDIRS: usize = 4;
/// A probe whose stats and opens average this long means the metadata is
/// coming off the disk. Measured on a loaded 8 CPU box, without the
/// slowest call: at most 2.8 us cached (240 runs over four trees), at
/// least 16 us right after dropping the caches.
const PROBE_COLD_PER_CALL: std::time::Duration = std::time::Duration::from_micros(10);

/// Whether the metadata under `path` looks uncached: time a listing of
/// `path` and a few of its subdirectories and the stat of some entries.
/// Walks that wait on the disk go faster with more threads in flight
/// than there are CPUs; cached walks go slower, so this decides.
fn looks_cold(path: &Path) -> bool {
    use std::time::{Duration, Instant};
    let Ok(dir) = du_fs::Dir::open(path) else {
        return false;
    };
    // Only stats and opens are timed: listing a huge directory is slow
    // even when cached, and says little about the cache per entry.
    let mut spent = Duration::ZERO;
    let mut slowest = Duration::ZERO;
    let mut calls = 0u32;
    let mut timed = |f: &mut dyn FnMut()| {
        let t = Instant::now();
        f();
        let took = t.elapsed();
        spent += took;
        slowest = slowest.max(took);
        calls += 1;
    };
    let (entries, _) = dir.entries();
    let mut subdirs = 0;
    for i in 0..entries.len().min(PROBE_STATS) {
        let (name, kind) = entries.get(i);
        let mut st = entries.stat(i);
        if st.is_none() {
            timed(&mut || st = dir.stat_at(name).ok());
        }
        let is_dir = kind == du_fs::Kind::Dir || st.is_some_and(|m| m.is_dir());
        if !is_dir || subdirs == PROBE_SUBDIRS {
            continue;
        }
        subdirs += 1;
        let mut child = None;
        timed(&mut || child = dir.open_child(name).ok());
        let Some(child) = child else { continue };
        let (inner, _) = child.entries();
        for j in 0..inner.len().min(PROBE_STATS / PROBE_SUBDIRS) {
            if inner.stat(j).is_none() {
                timed(&mut || {
                    let _ = child.stat_at(inner.get(j).0);
                });
            }
        }
    }
    // The slowest call is left out: on a busy machine one call that loses
    // its CPU would otherwise decide for a cached tree.
    if calls < 2 {
        return false;
    }
    (spent - slowest) / (calls - 1) >= PROBE_COLD_PER_CALL
}

/// Threads for a walk that waits on the disk: three per CPU, as diskus
/// found best, up to 64.
fn cold_threads() -> usize {
    let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
    (cpus * 3).min(64)
}

/// Fill in `git` for every listed row. Returns false when `git` could not
/// be run at all.
pub fn annotate_git(report: &mut Report) -> bool {
    annotate_git_with(report, None)
}

/// [`annotate_git`], reusing what a [`du_git::Prefetch`] found.
pub fn annotate_git_with(report: &mut Report, prefetched: Option<&du_git::Prefetched>) -> bool {
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
        du_git::classify_all_with(&queries, prefetched)
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

/// `,"unique":N` on `--unique` scans; nothing otherwise, so other output
/// keeps its shape.
fn unique_json(u: Option<u64>) -> String {
    u.map(|u| format!(",\"unique\":{u}")).unwrap_or_default()
}

fn item_json(it: &Item) -> String {
    let git_state = it.git.as_ref().map(|(s, _)| s.as_str());
    let git_repo = it
        .git
        .as_ref()
        .map(|(_, r)| r.to_string_lossy().into_owned());
    format!(
        "{{\"path\":\"{}\",\"type\":\"{}\",\"disk\":{},\"apparent\":{}{},\"mtime\":{},\"items\":{},\"artefact\":{},\"git\":{},\"git_repo\":{}}}",
        json_escape(&it.path.to_string_lossy()),
        if it.is_dir { "dir" } else { "file" },
        it.disk,
        it.apparent,
        unique_json(it.unique),
        it.mtime,
        it.items,
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
    // The cap key appears only where a cap is known (Windows), so other
    // output keeps its shape.
    let cap = t.cap.map(|c| format!(",\"cap\":{c}")).unwrap_or_default();
    format!(
        "{{\"roots\":[{}],\"items\":{},\"bytes\":{}{cap}}}",
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
                "{{\"path\":\"{}\",\"disk\":{},\"apparent\":{}{},\"files\":{},\"links\":{},\"dirs\":{},\"excluded\":{},\"artefact\":{}}}",
                json_escape(&t.path.to_string_lossy()),
                t.disk,
                t.apparent,
                unique_json(t.unique),
                t.files,
                t.links,
                t.dirs,
                t.excluded,
                json_opt(t.artefact)
            )
        })
        .collect();
    let dirs: Vec<String> = r.dirs.iter().map(item_json).collect();
    let files: Vec<String> = r.files.iter().map(item_json).collect();
    let shared = r
        .shared
        .map(|s| {
            format!(
                ",\"shared\":{{\"distinct\":{},\"referenced\":{}}}",
                s.distinct, s.referenced
            )
        })
        .unwrap_or_default();
    // Only a stopped scan says so; complete reports keep their old shape.
    let partial = if r.partial { ",\"partial\":true" } else { "" };
    format!(
        "{{\"roots\":[{}],\"dirs\":[{}],\"files\":[{}],\"unreadable\":{}{shared}{partial},\"trash\":{}}}",
        roots.join(","),
        dirs.join(","),
        files.join(","),
        r.unreadable,
        trash_json(trash)
    )
}

fn print_table(title: &str, items: &[Item], now: i64, count: bool) {
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
    let counts = |v: &str| {
        if count {
            format!("{v:>9}  ")
        } else {
            String::new()
        }
    };
    // The UNIQUE column shows on `--unique` scans only.
    let with_unique = items.iter().any(|it| it.unique.is_some());
    let uniques = |v: &str| {
        if with_unique {
            format!("{v:>10}  ")
        } else {
            String::new()
        }
    };
    println!(
        "{:>10}  {:>10}  {}{}{:>4}  {:<width$}  PATH",
        "DISK",
        "APPARENT",
        uniques("UNIQUE"),
        counts("ITEMS"),
        "AGE",
        "NOTE"
    );
    for it in items {
        let mut path = crate::util::term_path(&it.path);
        if it.is_dir {
            path.push(std::path::MAIN_SEPARATOR);
        }
        println!(
            "{:>10}  {:>10}  {}{}{:>4}  {:<width$}  {}",
            format_bytes(it.disk),
            format_bytes(it.apparent),
            uniques(&it.unique.map(format_bytes).unwrap_or_default()),
            counts(&it.items.to_string()),
            format_age(it.mtime, now),
            notes(it),
            path
        );
    }
    println!();
}

/// The `--unique` summary of shared extents.
fn shared_line(s: SharedTotals) -> String {
    if s.referenced == 0 {
        return "Shared extents: none (every byte belongs to one file)".into();
    }
    format!(
        "Shared extents: {} on disk, referenced as {} (reflinks, clones or snapshots)",
        format_bytes(s.distinct),
        format_bytes(s.referenced)
    )
}

fn print_trash(t: &TrashSummary) {
    println!(
        "Trash on these file systems: {} in {}{}",
        format_bytes(t.bytes),
        plural(t.items, "item", "items"),
        cap_suffix(t)
    );
    if t.items > 0 {
        println!(
            "  space returns only when the trash is emptied: preview with `rtrash empty -n`, free it with `rtrash empty`, or keep the last 30 days with `rtrash empty 30`"
        );
    }
}

/// `, Recycle Bin cap 4.00 GiB (25% used)` when the cap is known.
pub fn cap_suffix(t: &TrashSummary) -> String {
    match t.cap {
        Some(cap) if cap > 0 => format!(
            ", Recycle Bin cap {} ({}% used)",
            format_bytes(cap),
            (u128::from(t.bytes) * 100 / u128::from(cap)).min(999)
        ),
        _ => String::new(),
    }
}

fn pick_available() -> bool {
    cfg!(all(unix, feature = "tui")) && crate::util::stdin_is_tty()
}

/// The `--interactive` browser runs on Windows too; `--pick` reuses the
/// Unix put browser and does not.
fn interactive_available() -> bool {
    cfg!(feature = "tui") && crate::util::stdin_is_tty()
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
    // `-n5`: a short option with its value attached.
    if let Some(v) = short
        .and_then(|s| arg.strip_prefix(s))
        .filter(|v| !v.is_empty())
    {
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

/// Print folded stacks to stdout; a closed pipe (`| head`) ends quietly.
fn stacks_out(prog: &str, roots: &[RootTotals], dirs: &[Item], apparent: bool) -> i32 {
    let out = io::BufWriter::new(io::stdout().lock());
    match crate::du_snap::write_stacks(roots, dirs, apparent, out) {
        Ok(()) => 0,
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => 0,
        Err(e) => {
            eprintln!("{prog}: cannot write stacks: {e}");
            1
        }
    }
}

pub fn run(prog: &str, args: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some("diff") {
        return crate::du_snap::run_diff(prog, &args[1..]);
    }
    // Ctrl-C stops the scan and keeps what it counted. Installed before
    // the options are read, so one pressed while `-X -` waits on stdin
    // counts too.
    let stop = Arc::new(ScanControl::default());
    crate::du_interrupt::install(stop.clone());
    let mut opts = ScanOpts::default();
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut json = false;
    let mut print0 = false;
    let mut stacks = false;
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
    let mut interactive = false;
    let mut no_delete = false;
    let mut output: Option<PathBuf> = None;
    let mut from_file: Option<PathBuf> = None;
    let mut threads: Option<usize> = None;
    let mut filter = du_filter::Spec::default();
    let mut count = false;
    let now = crate::util::now_epoch();
    // Defaults from the config file; the options below override them.
    let cfg = crate::config::get();
    let warn = |key: &str, want: &str| {
        eprintln!("{prog}: warning: {}", cfg.bad_value("du", key, want));
    };
    match cfg.int("du", "top").map(usize::try_from) {
        Some(Ok(n)) => opts.top = n,
        Some(Err(_)) => warn("top", "a count of 0 or more"),
        None => {}
    }
    if let Some(s) = cfg.str("du", "sort") {
        match SortKey::parse(s) {
            Some(k) => opts.sort = k,
            None => warn("sort", "disk, apparent, items or mtime"),
        }
    }
    match cfg.int("du", "threads") {
        Some(n) if n >= 1 => threads = usize::try_from(n).ok(),
        Some(_) => warn("threads", "1 or more"),
        None => {}
    }
    count = cfg.bool("du", "count").unwrap_or(count);
    opts.one_file_system = cfg.bool("du", "one_file_system").unwrap_or(false);
    use_git = cfg.bool("du", "git").unwrap_or(use_git);
    let mut si = cfg.bool("du", "si").unwrap_or(false);
    // The config key only shapes -i; the flag alone is an error below.
    let mut read_only = cfg.bool("du", "no_delete").unwrap_or(false);
    filter.exclude_caches = cfg.bool("du", "exclude_caches").unwrap_or(false);
    filter.gitignore = cfg.bool("du", "gitignore").unwrap_or(false);
    for p in cfg.list("du", "exclude") {
        if du_filter::Pattern::parse(p).is_some() {
            filter.patterns.push(p.clone());
        }
    }
    let mut i = 0usize;
    while i < args.len() {
        let arg = args[i].as_str();
        if no_more_opts || arg == "-" || !arg.starts_with('-') {
            paths.push(crate::util::arg_path(arg));
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
        } else if let Some(v) = take_value(args, &mut i, arg, "--output", Some("-o")) {
            match v {
                Ok(v) => output = Some(crate::util::arg_path(v)),
                Err(e) => bail!(e),
            }
        } else if let Some(v) = take_value(args, &mut i, arg, "--file", Some("-f")) {
            match v {
                Ok(v) => from_file = Some(crate::util::arg_path(v)),
                Err(e) => bail!(e),
            }
        } else if let Some(v) = take_value(args, &mut i, arg, "--sort", None) {
            match v.map(|v| (v, SortKey::parse(v))) {
                Ok((_, Some(k))) => opts.sort = k,
                Ok((v, None)) => bail!(format!(
                    "invalid --sort '{v}' (use disk, apparent, items or mtime)"
                )),
                Err(e) => bail!(e),
            }
        } else if let Some(v) = take_value(args, &mut i, arg, "--exclude", None) {
            match v {
                Ok(v) if du_filter::Pattern::parse(v).is_some() => filter.patterns.push(v.into()),
                Ok(v) => bail!(format!("--exclude: empty pattern '{v}'")),
                Err(e) => bail!(e),
            }
        } else if let Some(v) = take_value(args, &mut i, arg, "--exclude-from", Some("-X")) {
            match v.map(|v| (v, du_filter::read_pattern_file(&crate::util::arg_path(v)))) {
                Ok((_, Ok(lines))) => filter.patterns.extend(lines),
                Ok((v, Err(e))) => {
                    eprintln!("{prog}: cannot read pattern file '{v}': {e}");
                    return 1;
                }
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
                "-C" | "--count" => count = true,
                "--all-dirs" => opts.all_dirs = true,
                "--artefacts" => opts.artefacts_only = true,
                "--exclude-caches" => filter.exclude_caches = true,
                "--gitignore" => filter.gitignore = true,
                "--dirs" => only_dirs = true,
                "--files" => only_files = true,
                "--json" => json = true,
                "--print0" => print0 = true,
                "--stacks" => stacks = true,
                "--unique" => opts.unique = true,
                "--no-git" => use_git = false,
                "--home-only" => home_only = true,
                a if a.starts_with("--trash-dir=") => {
                    trash_pins.push(crate::util::arg_path(&a["--trash-dir=".len()..]));
                }
                "--include-dirty" => include_dirty = true,
                "--pick" => pick = true,
                "-i" | "--interactive" => interactive = true,
                "--no-delete" => no_delete = true,
                "--si" => si = true,
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
    if filter.is_active() {
        if interactive {
            // The explorer rescans one directory at a time; anchored
            // patterns stay relative to the PATH it was opened on.
            let first = paths.first().map_or(Path::new("."), PathBuf::as_path);
            filter.base = absolute(first).ok();
        }
        opts.filter = Some(Arc::new(filter));
    }
    if only_dirs && !only_files {
        want_files = false;
    }
    if only_files && !only_dirs {
        want_dirs = false;
    }
    if [json, print0, pick, interactive]
        .iter()
        .filter(|b| **b)
        .count()
        > 1
    {
        eprintln!("{prog}: --json, --print0, --pick and --interactive are mutually exclusive");
        return 2;
    }
    if pick && !pick_available() {
        eprintln!("{prog}: --pick needs a terminal and the tui feature");
        return 2;
    }
    if interactive && !interactive_available() {
        eprintln!("{prog}: --interactive needs a terminal and the tui feature");
        return 2;
    }
    if paths.is_empty() {
        paths.push(PathBuf::from("."));
    }
    // Text sizes everywhere in this run, the explorer included; JSON,
    // snapshots and stacks stay in bytes.
    crate::util::set_si_units(si);
    if no_delete && !interactive {
        eprintln!("{prog}: --no-delete applies to --interactive; other modes never delete");
        return 2;
    }
    read_only |= no_delete;
    #[cfg(not(feature = "tui"))]
    let _ = read_only;
    if interactive && paths.len() > 1 {
        eprintln!("{prog}: --interactive takes one PATH");
        return 2;
    }
    if output.is_some() && (from_file.is_some() || json || print0 || pick || interactive) {
        eprintln!("{prog}: --output writes a snapshot; it takes no other output mode");
        return 2;
    }
    if stacks && (output.is_some() || json || print0 || pick || interactive) {
        eprintln!("{prog}: --stacks prints folded stacks; it takes no other output mode");
        return 2;
    }
    if opts.unique && (from_file.is_some() || output.is_some() || stacks || interactive) {
        eprintln!(
            "{prog}: --unique adds a column to a live listing; it takes no -f, -o, --stacks or -i"
        );
        return 2;
    }
    if stacks && !matches!(opts.sort, SortKey::Disk | SortKey::Apparent) {
        eprintln!("{prog}: --stacks weighs by disk use or --apparent size");
        return 2;
    }
    if let Some(file) = &from_file {
        if print0 || pick || interactive {
            eprintln!(
                "{prog}: a snapshot can be stale; --file lists it but cannot drive --print0, --pick or --interactive"
            );
            return 2;
        }
        let snap = match crate::du_snap::read(file) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{prog}: cannot read snapshot '{}': {e}", file.display());
                return 1;
            }
        };
        if stacks {
            let apparent = opts.sort == SortKey::Apparent;
            return stacks_out(prog, &snap.roots, &snap.dirs, apparent);
        }
        if opts.sort == SortKey::Items {
            eprintln!("{prog}: snapshots do not record item counts; --sort=items needs a scan");
            return 2;
        }
        let report = crate::du_snap::to_report(snap, opts.sort, opts.top);
        let trash = TrashSummary::default();
        if json {
            println!("{}", report_json(&report, &trash));
        } else {
            print_table(
                &format!("{} in {}", opts.sort.title("directories"), file.display()),
                &report.dirs,
                now,
                false,
            );
            for t in &report.roots {
                println!(
                    "Total {}: {} on disk, {} apparent",
                    crate::util::term_path(&t.path),
                    format_bytes(t.disk),
                    format_bytes(t.apparent)
                );
            }
        }
        return 0;
    }
    if output.is_some() || stacks {
        opts = crate::du_snap::snapshot_opts(opts);
        // Snapshots record sizes only.
        use_git = false;
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
    if interactive {
        // The explorer reads Ctrl-C as a key.
        crate::du_interrupt::uninstall();
        if crate::du_interrupt::pressed() {
            return 130;
        }
    }
    #[cfg(feature = "tui")]
    if interactive {
        return crate::du_tui::run(prog, &paths[0], opts, use_git, read_only);
    }
    opts.control = Some(stop);
    // Git probes run beside the walk. Under --no-git, or without git on
    // PATH, there is nothing to start.
    let prefetch = if use_git {
        du_git::Prefetch::start()
    } else {
        None
    };
    let scan = |paths: &[PathBuf], opts: &ScanOpts| scan_with(paths, opts, prefetch.as_ref());
    // Without --threads or RAYON_NUM_THREADS, an uncached tree gets more
    // threads than CPUs.
    let threads = threads.or_else(|| {
        if std::env::var_os("RAYON_NUM_THREADS").is_some() {
            return None;
        }
        let first = paths.first()?;
        let first = absolute(first).ok()?;
        looks_cold(&first).then(cold_threads)
    });
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
    // From here a Ctrl-C ends the process again.
    crate::du_interrupt::uninstall();
    let mut report = match scanned {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{prog}: cannot scan: {e}");
            return 1;
        }
    };
    if report.partial && (stacks || output.is_some() || print0 || pick) {
        // A partial list would pass for a whole one: trashing from it or
        // diffing against it misleads.
        eprintln!(
            "{prog}: interrupted; --print0, --pick, --output and --stacks print nothing for a partial scan"
        );
        return 130;
    }
    if stacks {
        let apparent = opts.sort == SortKey::Apparent;
        return stacks_out(prog, &report.roots, &report.dirs, apparent);
    }
    if let Some(out) = &output {
        let written = if out.as_os_str() == "-" {
            crate::du_snap::write(&report, &opts, now, io::stdout().lock())
        } else {
            fs::File::create(out)
                .and_then(|f| crate::du_snap::write(&report, &opts, now, io::BufWriter::new(f)))
        };
        if let Err(e) = written {
            eprintln!("{prog}: cannot write snapshot '{}': {e}", out.display());
            return 1;
        }
        if out.as_os_str() != "-" {
            eprintln!(
                "{prog}: wrote {} to '{}'",
                plural(report.dirs.len() as u64, "directory", "directories"),
                out.display()
            );
        }
        return 0;
    }
    if !want_dirs {
        report.dirs.clear();
    }
    if !want_files {
        report.files.clear();
    }
    let prefetched = prefetch.map(du_git::Prefetch::finish);
    if use_git && !annotate_git_with(&mut report, prefetched.as_ref()) {
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
        let count = count || opts.sort == SortKey::Items;
        print_table(&opts.sort.title("directories"), &report.dirs, now, count);
        print_table(&opts.sort.title("files"), &report.files, now, count);
        for t in &report.roots {
            let kind = t.artefact.map(|k| format!(" ({k})")).unwrap_or_default();
            let links = if t.links > 0 {
                format!(" (+{})", plural(t.links, "hard link", "hard links"))
            } else {
                String::new()
            };
            let excluded = if t.excluded > 0 {
                format!(", {} excluded", t.excluded)
            } else {
                String::new()
            };
            let unique = t
                .unique
                .map(|u| format!(", {} unique", format_bytes(u)))
                .unwrap_or_default();
            println!(
                "Total {}{kind}: {} on disk{unique}, {} apparent, {}{links}, {}{excluded}",
                crate::util::term_path(&t.path),
                format_bytes(t.disk),
                format_bytes(t.apparent),
                plural(t.files, "file", "files"),
                plural(t.dirs, "directory", "directories")
            );
        }
        if let Some(s) = report.shared {
            println!("{}", shared_line(s));
        }
        if report.partial {
            println!("Partial scan: stopped by Ctrl-C, so sizes are lower bounds");
        }
        print_trash(&trash);
    }
    if report.unreadable > 0 {
        eprintln!(
            "{prog}: {} could not be read",
            plural(report.unreadable, "entry", "entries")
        );
    }
    if report.partial {
        eprintln!("{prog}: interrupted; the listing covers what was scanned before Ctrl-C");
        return 130;
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
            items: 1,
            artefact: None,
            repos: Vec::new(),
            git: None,
            unique: None,
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
    fn cold_walks_get_three_threads_per_cpu_up_to_64() {
        let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
        assert_eq!(cold_threads(), (cpus * 3).min(64));
        assert!(!looks_cold(Path::new("/nonexistent-rtrash-probe")));
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
    fn topn_large_cap_matches_sorted_insert() {
        let items: Vec<Item> = (0..5000u64)
            .map(|i| item(&format!("/p{:05}", (i * 7919) % 5000), (i * 31) % 97, 0))
            .collect();
        for cap in [3usize, 1500, 4000, usize::MAX] {
            let mut big = TopN::new(cap, SortKey::Disk);
            big.merge_vec(items.clone());
            let mut want = items.clone();
            want.sort_by(|a, b| b.disk.cmp(&a.disk).then_with(|| a.path.cmp(&b.path)));
            want.truncate(cap);
            assert_eq!(big.into_vec(), want, "cap {cap}");
        }
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

    #[test]
    fn stopped_scan_keeps_the_root_files_and_says_partial() {
        let root = std::env::temp_dir().join(format!("rtrash-du-stop-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/deep"), vec![1u8; 300_000]).unwrap();
        fs::write(root.join("top"), vec![2u8; 5000]).unwrap();
        let whole = scan(std::slice::from_ref(&root), &ScanOpts::default()).unwrap();
        assert!(!whole.partial);
        assert_eq!(whole.roots[0].apparent, 305_000);
        assert!(!report_json(&whole, &TrashSummary::default()).contains("partial"));
        let control = Arc::new(ScanControl::default());
        control.stop.store(true, Ordering::Relaxed);
        let opts = ScanOpts {
            control: Some(control.clone()),
            ..ScanOpts::default()
        };
        let r = scan(std::slice::from_ref(&root), &opts).unwrap();
        assert!(r.partial);
        assert_eq!(r.roots[0].apparent, 5000);
        assert_eq!(r.roots[0].files, 1);
        assert_eq!(control.files.load(Ordering::Relaxed), 1);
        assert!(r.files.iter().any(|f| f.path.ends_with("top")));
        assert!(report_json(&r, &TrashSummary::default()).contains(",\"partial\":true,"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unique_scan_sums_up_and_shows_in_json() {
        let root = std::env::temp_dir().join(format!("rtrash-du-unique-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a")).unwrap();
        fs::write(root.join("a/big"), vec![1u8; 300_000]).unwrap();
        fs::write(root.join("small"), vec![2u8; 5000]).unwrap();
        let plain = scan(std::slice::from_ref(&root), &ScanOpts::default()).unwrap();
        assert!(plain.shared.is_none());
        assert!(plain.files.iter().all(|f| f.unique.is_none()));
        assert!(!report_json(&plain, &TrashSummary::default()).contains("\"unique\":"));
        let opts = ScanOpts {
            unique: true,
            ..ScanOpts::default()
        };
        let r = scan(std::slice::from_ref(&root), &opts).unwrap();
        let t = &r.roots[0];
        // Fresh files share nothing, so unique equals disk use, whether the
        // file system maps extents or not.
        // APFS reports a private size of its own, which may round
        // differently from the allocated blocks; elsewhere they match.
        let fits = |u: Option<u64>, disk: u64| match u {
            Some(u) if cfg!(target_os = "macos") => u <= disk,
            Some(u) => u == disk,
            None => false,
        };
        assert!(fits(t.unique, t.disk), "{:?} of {}", t.unique, t.disk);
        for f in &r.files {
            assert!(fits(f.unique, f.disk), "{}", f.path.display());
        }
        assert_eq!(r.shared, Some(SharedTotals::default()));
        let j = report_json(&r, &TrashSummary::default());
        assert!(
            j.contains(&format!("\"unique\":{}", t.unique.unwrap())),
            "{j}"
        );
        assert!(
            j.contains("\"shared\":{\"distinct\":0,\"referenced\":0}"),
            "{j}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn shared_line_names_both_totals() {
        assert!(shared_line(SharedTotals::default()).contains("none"));
        let s = shared_line(SharedTotals {
            distinct: 1 << 20,
            referenced: 3 << 20,
        });
        assert!(
            s.contains(&format_bytes(1 << 20)) && s.contains(&format_bytes(3 << 20)),
            "{s}"
        );
    }

    #[test]
    fn trash_summary_shows_the_cap_only_when_known() {
        let mut t = TrashSummary {
            roots: vec![PathBuf::from("C:\\")],
            items: 3,
            bytes: 1 << 30,
            cap: None,
        };
        assert_eq!(cap_suffix(&t), "");
        assert!(!trash_json(&t).contains("cap"));
        t.cap = Some(4 << 30);
        assert_eq!(
            cap_suffix(&t),
            format!(", Recycle Bin cap {} (25% used)", format_bytes(4 << 30))
        );
        assert!(trash_json(&t).ends_with(&format!(",\"cap\":{}}}", 4u64 << 30)));
        t.cap = Some(0);
        assert_eq!(cap_suffix(&t), "");
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
            shared: None,
            partial: false,
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
