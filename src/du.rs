//! `rtrash du`: find what to delete. Walks one or more paths in parallel and
//! reports the largest directories and files, with on-disk and apparent sizes.

use std::collections::{HashMap, HashSet};
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

Symlinks are not followed unless -L is given, and a file with more than one hard link
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
  -L, --follow-symlinks follow symlinks: each counts as what it points to,
                          a directory met again (a loop, or a second link)
                          is walked once and a file counts once; the
                          total names the repeats as repeated names
      --apparent        rank by apparent size (file length) instead of disk use
      --sort=KEY        rank by KEY: disk (default), apparent, items (files
                          and directories below each directory; lists no
                          files) or mtime (newest modification first; a
                          directory gives way to the subdirectory that has
                          its newest change)
  -C, --count           add an ITEMS column (each hard-linked file once)
      --show-mtime      show the local modification time (YYYY-MM-DD HH:MM)
                          in place of the AGE column; --json keeps mtime
                          as epoch seconds
      --show-relative-size  add a SHARE column: each row's percent of its
                          PATH total (apparent size with --apparent, else
                          disk use); the JSON rows keep the byte counts
  -s, --summarize       print only the totals, no rows (GNU du -s, gdu)
      --skip-total      print the rows without the Total lines (dust)
      --reverse         print each table in reverse, the top row last
                          (dust -r); JSON and --print0 keep the ranking
      --inodes          rank by inodes, as --sort=items: files and
                          directories, each hard-linked file once; the
                          total gives the inodes each PATH uses
      --types           list file types instead of directories and files:
                          disk use, apparent size and file count per
                          extension (case ignored; names with no extension
                          make one row), first --top of them
      --unique          add a UNIQUE column: the bytes in extents no other
                          file shares, roughly what deleting a row frees on
                          btrfs, XFS or APFS (reflinks, clones, snapshots);
                          slower, since it maps every file's extents
      --shared          count each extent that reflinks, clones or snapshots
                          share once per row and total, as btrfs and XFS
                          flag them on Linux; slower, since it maps every
                          file's extents (APFS and other file systems
                          count every file in full)
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
      --files-from=FILE  also scan the paths in FILE, one per line (- is
                          stdin), as dust does
      --files0-from=FILE  the same with NUL-separated paths (find -print0)
      --exclude-caches  leave out directories below PATH that hold a valid
                          CACHEDIR.TAG
      --type=EXTS       count only files ending in one of EXTS, such as
                          yaml,json (directories are still walked)
      --exclude-type=EXTS  leave out files ending in one of EXTS
      --include-kernfs  also walk kernel pseudo file systems such as /proc,
                          /sys and cgroup mounts met below PATH (skipped by
                          default on Linux; their sizes are not disk use)
      --no-hidden       leave out hidden entries, those named with a
                          leading dot (same as --exclude='.*')
      --gitignore       leave out what git ignores: .gitignore files,
                          .git/info/exclude and ~/.config/git/ignore, inside
                          git repositories
      --dirs            list directories only
      --files           list files only
      --older-than=DAYS only list entries whose newest modification is more
                          than DAYS days old (directories use their whole tree)
      --newer-than=DAYS only list entries modified within the last DAYS days
      --since=DATE      only list entries modified at or after DATE
                          (YYYY-MM-DD, or YYYY-MM-DDTHH:MM[:SS]; local time)
      --until=DATE      only list entries whose newest modification is
                          before DATE
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
      --dupes           list groups of files with the same contents (size,
                          then edge hash, then full hash); hard links count
                          once, reflinks free nothing; the oldest file stays
      --trash-dupes     trash every copy but the kept one in each group
      --clone-dupes     replace every copy but the kept one with a reflink
                          clone of it, keeping its mode, owner and times
                          (btrfs, XFS, APFS, ReFS)
      --link-dupes      replace every copy but the kept one with a hard
                          link to it (same device only)
      --min-size=SIZE   with --dupes, skip files smaller than SIZE
      --devices         list every mounted file system with its size, use
                          and free space (as df does), instead of scanning
      --ncdu=FILE       write an ncdu export of PATH to FILE (- for
                          stdout) for `ncdu -f FILE` or `du -f FILE`
      --ncdu-binary=FILE  the same in ncdu 2's binary format (ncdu -O),
                          for ncdu 2.6 or later
  -f, --file=FILE       list from a snapshot, or an ncdu export (ncdu -o,
                          ncdu -O, gdu -o), instead of scanning
      --stacks          print folded stacks of every directory's own size
                          for flame graph tools (flamegraph.pl, inferno,
                          speedscope) instead of listing; works with -f
      --pick            open the put browser on the listed rows: mark rows
                          and trash them (needs a terminal)
  -i, --interactive     explore PATH one directory at a time (several PATHs
                          get a view of their own above them): open and leave
                          directories, trash rows (d), undo (u), rescan (r),
                          sort (s, n, C, M), switch disk use and apparent
                          size (S), show details (i), filter by name (/),
                          hide dot names (.), print paths and quit (p) or
                          copy them (y), open a shell there (b), page a
                          file (v) or open a row with its program (o); rows
                          show git state and artefact kind
      --no-delete       with -i, browse only: d, u and b do nothing
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
            "items" | "count" | "inodes" => SortKey::Items,
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
    /// `-L`: follow symlinks; each directory and file counts once.
    pub follow: bool,
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
    /// `--shared`: count each shared extent once per row and total.
    pub shared_once: bool,
    /// Counters to update as the walk goes (one add per directory chunk).
    pub control: Option<Arc<ScanControl>>,
    /// `--types`: total the files of each file type.
    pub types: bool,
    /// `--include-kernfs`: also walk kernel pseudo file systems (proc,
    /// sysfs, cgroup, ...) met below a scan root; skipped by default.
    pub include_kernfs: bool,
}

impl Default for ScanOpts {
    fn default() -> Self {
        ScanOpts {
            top: 20,
            one_file_system: false,
            follow: false,
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
            shared_once: false,
            control: None,
            types: false,
            include_kernfs: false,
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
    /// `--shared` scans: disk use counting every reference to a shared
    /// extent, where `disk` counts each once.
    pub disk_all: Option<u64>,
}

impl RootTotals {
    /// Inodes the PATH uses: its files, each hard-linked file once, and
    /// its directories, the PATH itself included.
    pub fn inodes(&self) -> u64 {
        self.files.saturating_add(self.dirs)
    }
}

/// Result of scanning one or more paths.
#[derive(Debug, Clone)]
pub struct Report {
    pub roots: Vec<RootTotals>,
    pub dirs: Vec<Item>,
    pub files: Vec<Item>,
    /// Entries that could not be read (permission denied, vanished).
    pub unreadable: u64,
    /// The first of them by path, up to 100, each with the reason.
    pub unread: Vec<(PathBuf, String)>,
    /// Directories nested too deep to enter: counted, contents unread.
    pub too_deep: u64,
    /// Levels the walk entered at most: [`MAX_DEPTH`], or fewer when the
    /// system refused the walk threads their full stacks.
    pub depth_limit: usize,
    /// `--unique` scans: bytes of the shared extents the scanned files
    /// point at, each extent counted once, and with every reference.
    pub shared: Option<SharedTotals>,
    /// The scan was stopped early: sizes are lower bounds and some
    /// directories are missing.
    pub partial: bool,
    /// `--types` scans: every file type, in the order of the sort key.
    pub types: Vec<TypeRow>,
}

/// The files of one type in a `--types` scan: names ending in `.EXT`
/// (`ext` lowercase), or with no extension (`ext` empty).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TypeRow {
    pub ext: String,
    pub disk: u64,
    pub apparent: u64,
    pub files: u64,
}

/// Disk use, apparent size and file count per type key.
type TypeMap = HashMap<Vec<u8>, (u64, u64, u64)>;

/// Extensions longer than this read as none, so hashes and version tails
/// after a dot do not each make a type.
const TYPE_MAX: usize = 12;

/// The type key of a file name: the lowercase text after its last dot, or
/// empty when there is none. A leading dot (`.bashrc`) starts no
/// extension.
pub fn type_key(name: &[u8]) -> Vec<u8> {
    match name.iter().rposition(|&b| b == b'.') {
        Some(i) if i > 0 && i + 1 < name.len() && name.len() - i - 1 <= TYPE_MAX => {
            name[i + 1..].to_ascii_lowercase()
        }
        _ => Vec::new(),
    }
}

fn add_type(map: &mut TypeMap, name: &[u8], apparent: u64, disk: u64) {
    let e = map.entry(type_key(name)).or_default();
    e.0 = e.0.saturating_add(disk);
    e.1 = e.1.saturating_add(apparent);
    e.2 += 1;
}

/// The rows of `map`, by `sort` (disk use unless apparent or items, which
/// counts files), then by type.
fn type_rows(map: TypeMap, sort: SortKey) -> Vec<TypeRow> {
    let mut rows: Vec<TypeRow> = map
        .into_iter()
        .map(|(k, (disk, apparent, files))| TypeRow {
            ext: String::from_utf8_lossy(&k).into_owned(),
            disk,
            apparent,
            files,
        })
        .collect();
    let key = |r: &TypeRow| match sort {
        SortKey::Apparent => r.apparent,
        SortKey::Items => r.files,
        _ => r.disk,
    };
    rows.sort_by(|a, b| key(b).cmp(&key(a)).then_with(|| a.ext.cmp(&b.ext)));
    rows
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
    let (_, items, bytes) = status::summarize_dirs(&same, None, None, Default::default());
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
    /// `-L`: directories entered so far, so a symlink loop or a second
    /// link to a directory walks it once.
    dirs_seen: LinkSet,
    unreadable: AtomicU64,
    /// The first [`UNREAD_KEPT`] unreadable entries, with the reason.
    unread: Mutex<Vec<(PathBuf, String)>>,
    /// Levels the walk enters: [`MAX_DEPTH`], or fewer on small stacks.
    depth_limit: usize,
    /// Directories past `depth_limit` or [`MAX_PATH_BYTES`], not entered.
    too_deep: AtomicU64,
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
    /// Which files are worth an extent map.
    #[cfg_attr(not(unix), allow(dead_code))]
    probe: crate::du_extent::Probe,
    /// A directory was skipped because the scan was asked to stop.
    cut: std::sync::atomic::AtomicBool,
    /// File type totals (`--types`), merged once per directory chunk.
    types: Mutex<TypeMap>,
}

/// Unreadable entries a scan remembers by name; the count covers the rest.
const UNREAD_KEPT: usize = 100;
/// Of those, how many stderr names before it sums up the rest.
const UNREAD_SHOWN: usize = 10;

impl Ctx<'_> {
    /// Count an entry that could not be read, and remember its path and
    /// the reason while fewer than [`UNREAD_KEPT`] are kept.
    fn unread(&self, path: impl FnOnce() -> PathBuf, why: impl std::fmt::Display) {
        self.unreadable.fetch_add(1, Ordering::Relaxed);
        let mut kept = self.unread.lock().unwrap_or_else(|e| e.into_inner());
        if kept.len() < UNREAD_KEPT {
            kept.push((path(), why.to_string()));
        }
    }

    /// The extents of a counted file: its bytes no other file shares and
    /// its shared extents. Without an extent map the whole file counts
    /// as unique. On `--unique` scans the shared extents also go to
    /// `self.shared`.
    fn share_of(
        &self,
        dir: &du_fs::Dir,
        name: &CStr,
        st: &du_fs::Stat,
        disk: u64,
    ) -> crate::du_extent::FileShare {
        let whole = crate::du_extent::FileShare {
            unique: disk,
            shared: Vec::new(),
        };
        #[cfg(unix)]
        {
            if !st.is_file() || disk == 0 {
                return whole;
            }
            let Some(f) = self.probe.wants(st.dev, disk, || dir.open_file(name).ok()) else {
                return whole;
            };
            self.note_shares(st.dev, crate::du_extent::file_share(&f, disk))
        }
        #[cfg(not(unix))]
        {
            let _ = (dir, name, st);
            whole
        }
    }

    #[cfg(unix)]
    fn note_shares(&self, dev: u64, s: crate::du_extent::FileShare) -> crate::du_extent::FileShare {
        if self.opts.unique && !s.shared.is_empty() {
            if let Ok(mut set) = self.shared.lock() {
                set.add(dev, &s.shared);
            }
        }
        s
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
    /// `--shared`: bytes outside shared extents, and the shared extents
    /// as merged `(device, start, end)` ranges; `disk_all` counts every
    /// reference. `disk` is then `base` plus what the ranges cover.
    base: u64,
    ranges: Vec<(u64, u64, u64)>,
    disk_all: u64,
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
/// their children open by full path, which bounds open descriptors,
/// unless the path is too long for that ([`children_open_by_path`]).
const FD_HOLD_DEPTH: usize = 64;

/// Whether the subdirectories of the directory at `path` can open by full
/// path: theirs, one name longer, must stay under `PATH_MAX`.
#[cfg(unix)]
fn children_open_by_path(path: &Path) -> bool {
    path.as_os_str().len() < libc::PATH_MAX as usize - 256
}

/// Windows opens every directory by path, so holding one changes nothing.
#[cfg(windows)]
fn children_open_by_path(_: &Path) -> bool {
    true
}

/// The deepest level, and the longest path, the walk enters. A directory
/// past either counts with its own blocks, is reported, and its contents
/// stay unread: the walk recurses once per level and keeps the path of
/// each, so a hostile tree could otherwise exhaust the stack or memory.
/// Unit tests on macOS use 1000 levels: the tests build chains past this
/// limit, and there each step down a chain costs more the deeper it lies.
pub(crate) const MAX_DEPTH: usize = if cfg!(all(test, target_os = "macos")) {
    1000
} else {
    4096
};
pub(crate) const MAX_PATH_BYTES: usize = 32 * 1024;

/// Stack of each walk thread. Rayon's default of 2 MiB ran out near 2000
/// levels (near 600 in a debug build); this leaves room for [`MAX_DEPTH`]
/// in a debug build, also when a waiting thread runs a second deep chain
/// on top of its own.
const WALK_STACK: usize = 256 * 1024 * 1024;

/// Stacks to try for the walk threads, largest first. A cap on the
/// address space (`ulimit -v`) or strict overcommit may refuse the
/// threads [`WALK_STACK`] each; each smaller try walks fewer levels.
const WALK_STACKS: [usize; 4] = [WALK_STACK, WALK_STACK / 4, WALK_STACK / 16, 2 * 1024 * 1024];

/// Levels a walk thread with `stack` bytes of stack may enter: the share
/// of [`MAX_DEPTH`] that [`WALK_STACK`] gives it, so each level keeps the
/// room it has at full size.
fn depth_for_stack(stack: usize) -> usize {
    MAX_DEPTH * (stack / 1024) / (WALK_STACK / 1024)
}

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
    // A directory known only by its stat: its own blocks count, as with
    // du(1), and nothing below it.
    let shut = |st: du_fs::Stat| {
        ctx.dirs.fetch_add(1, Ordering::Relaxed);
        Node {
            apparent: 0,
            disk: st.sizes().1,
            unique: st.sizes().1,
            base: st.sizes().1,
            ranges: Vec::new(),
            disk_all: st.sizes().1,
            mtime: st.mtime,
            items: 0,
            repos: Vec::new(),
            listable: false,
            artefact: None,
        }
    };
    if depth > ctx.depth_limit || path.as_os_str().len() > MAX_PATH_BYTES {
        let st = match known {
            Some(st) => st,
            None => match parent {
                Some((dir, name)) => dir.stat_at(name).ok()?,
                None => du_fs::lstat(path).ok()?,
            },
        };
        if !st.is_dir() || (ctx.opts.one_file_system && st.dev != ctx.root_dev) {
            return None;
        }
        ctx.too_deep.fetch_add(1, Ordering::Relaxed);
        return Some(shut(st));
    }
    let follow = ctx.opts.follow;
    let opened = match parent {
        Some((dir, name)) if follow => dir.open_child_follow(name),
        Some((dir, name)) => dir.open_child(name),
        None if follow => du_fs::Dir::open_follow(path),
        None => du_fs::Dir::open(path),
    };
    let stat_again = || match parent {
        Some((dir, name)) if follow => dir.stat_follow(name),
        Some((dir, name)) => dir.stat_at(name),
        None if follow => du_fs::stat_follow(path),
        None => du_fs::lstat(path),
    };
    let unreadable = |e: io::Error| {
        ctx.unread(|| path.to_path_buf(), e);
        None
    };
    let dir = match opened {
        Ok(dir) => dir,
        Err(e) => {
            let st = match known {
                Some(st) => st,
                None => match stat_again() {
                    Ok(st) => st,
                    Err(e) => return unreadable(e),
                },
            };
            if !st.is_dir() || (ctx.opts.one_file_system && st.dev != ctx.root_dev) {
                return unreadable(e);
            }
            ctx.unread(|| path.to_path_buf(), e);
            return Some(shut(st));
        }
    };
    let st = match known {
        Some(st) => st,
        None => match dir.stat() {
            Ok(st) => st,
            Err(e) => return unreadable(e),
        },
    };
    if ctx.opts.one_file_system && st.dev != ctx.root_dev {
        return None;
    }
    // A mount of proc, sysfs and the like below the root reports sizes
    // that are not disk use (/proc/kcore alone claims the address space),
    // so it stays out unless asked for. A root named on purpose is walked.
    if !ctx.opts.include_kernfs && st.dev != ctx.root_dev && dir.is_kernfs() {
        return None;
    }
    // -L: a directory met again, through a loop or a second link, is
    // walked once.
    if follow && st.ino != 0 && !ctx.dirs_seen.insert(st.dev, st.ino) {
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
    /// `--shared`: as in [`Node`], for these entries.
    base: u64,
    ranges: Vec<(u64, u64, u64)>,
    mtime: i64,
    /// Files counted (each hard-linked file once).
    files: u64,
    /// Subdirectories, with their stat when it was taken here (for -x or
    /// an entry of unknown type).
    subdirs: Vec<(usize, Option<du_fs::Stat>)>,
    candidates: Vec<FileCandidate>,
}

/// `--shared`: add a file of `raw` allocated bytes with `shared`
/// extents on `dev` to `base` and `ranges`, and return its own size with
/// each shared byte counted once. The parts never add up to more than
/// `raw`: an extent map can give compressed data its uncompressed length.
fn count_once(
    dev: u64,
    raw: u64,
    shared: &[crate::du_extent::Extent],
    base: &mut u64,
    ranges: &mut Vec<(u64, u64, u64)>,
) -> u64 {
    if shared.is_empty() {
        *base = base.saturating_add(raw);
        return raw;
    }
    let referenced: u64 = shared.iter().map(|e| e.length).sum();
    let own = raw.saturating_sub(referenced);
    *base = base.saturating_add(own);
    let mut mine: Vec<(u64, u64, u64)> = shared
        .iter()
        .map(|e| (dev, e.physical, e.physical.saturating_add(e.length)))
        .collect();
    let once = crate::du_extent::coalesce(&mut mine);
    ranges.extend(mine);
    raw.min(own.saturating_add(once))
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
        base: 0,
        ranges: Vec::new(),
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
    let mut types = TypeMap::new();
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
        let mut m = match entries.stat(i) {
            Some(m) => m,
            None => match dir.stat_at(name) {
                Ok(m) => m,
                Err(e) => {
                    ctx.unread(|| path.join(du_fs::os(name)), e);
                    continue;
                }
            },
        };
        // -L: a symlink counts as what it points to; a dangling one counts
        // as itself. A file reached through links counts once.
        if opts.follow && m.is_symlink() {
            if let Ok(t) = dir.stat_follow(name) {
                m = t;
            }
        }
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
        let first = if opts.follow {
            m.ino == 0 || ctx.seen.insert(m.dev, m.ino)
        } else {
            ctx.first_link(&m)
        };
        if !first {
            ctx.links.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        ctx.files.fetch_add(1, Ordering::Relaxed);
        out.files += 1;
        let (a, raw) = m.sizes();
        out.apparent = out.apparent.saturating_add(a);
        out.disk = out.disk.saturating_add(raw);
        let (u, d) = if opts.unique || opts.shared_once {
            let s = ctx.share_of(dir, name, &m, raw);
            let d = if opts.shared_once {
                count_once(m.dev, raw, &s.shared, &mut out.base, &mut out.ranges)
            } else {
                raw
            };
            (if opts.unique { s.unique } else { 0 }, d)
        } else {
            (0, raw)
        };
        out.unique = out.unique.saturating_add(u);
        out.mtime = out.mtime.max(m.mtime);
        if opts.types {
            add_type(&mut types, name.to_bytes(), a, d);
        }
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
    if !types.is_empty() {
        merge_types(
            &mut ctx.types.lock().unwrap_or_else(|e| e.into_inner()),
            types,
        );
    }
    out
}

fn merge_types(into: &mut TypeMap, from: TypeMap) {
    for (k, (d, a, n)) in from {
        let e = into.entry(k).or_default();
        e.0 = e.0.saturating_add(d);
        e.1 = e.1.saturating_add(a);
        e.2 += n;
    }
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
        base: st.sizes().1,
        ranges: Vec::new(),
        disk_all: st.sizes().1,
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
        ctx.unread(|| path.to_path_buf(), "the listing stopped early");
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
        node.disk_all = node.disk_all.saturating_add(p.disk);
        node.base = node.base.saturating_add(p.base);
        node.ranges.extend(p.ranges);
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

    let parent = if depth < FD_HOLD_DEPTH || !children_open_by_path(path) {
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
        node.disk_all = node.disk_all.saturating_add(child.disk_all);
        node.base = node.base.saturating_add(child.base);
        node.ranges.extend(child.ranges);
        node.unique = node.unique.saturating_add(child.unique);
        node.mtime = node.mtime.max(child.mtime);
        for r in child.repos {
            if node.repos.len() < MAX_REPOS {
                node.repos.push(r);
            }
        }
    }
    if opts.shared_once {
        let once = crate::du_extent::coalesce(&mut node.ranges);
        node.disk = node.disk_all.min(node.base.saturating_add(once));
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
    scan_on(paths, opts, prefetch, None)
}

/// [`scan_with`] on `threads` walk threads, or rayon's default count
/// (`RAYON_NUM_THREADS`, else one per CPU), each with [`WALK_STACK`].
pub fn scan_on(
    paths: &[PathBuf],
    opts: &ScanOpts,
    prefetch: Option<&du_git::Prefetch>,
    threads: Option<usize>,
) -> io::Result<Report> {
    scan_on_stacks(paths, opts, prefetch, threads, &WALK_STACKS)
}

/// [`scan_on`] with the first of `stacks` the system grants the walk
/// threads. The walk recurses once per level, so the depth it enters
/// follows the stack it got (see [`depth_for_stack`]): falling back to a
/// small stack with the full depth overflowed it on a deep tree.
fn scan_on_stacks(
    paths: &[PathBuf],
    opts: &ScanOpts,
    prefetch: Option<&du_git::Prefetch>,
    threads: Option<usize>,
    stacks: &[usize],
) -> io::Result<Report> {
    let n = threads.unwrap_or_else(|| {
        std::env::var("RAYON_NUM_THREADS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
    });
    // Under a cap on the address space, start with stacks that fit in a
    // quarter of it: the threads of a refused try may still hold theirs
    // while the next try starts.
    let budget = address_space_cap().map_or(usize::MAX, |cap| cap / 4);
    let fits = stacks
        .iter()
        .position(|&s| s.saturating_mul(n) <= budget)
        .unwrap_or(stacks.len().saturating_sub(1));
    let mut last = None;
    for &stack in &stacks[fits..] {
        let built = rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .stack_size(stack)
            .build();
        match built {
            Ok(pool) => {
                let depth = depth_for_stack(stack);
                return pool.install(|| scan_pooled(paths, opts, prefetch, depth));
            }
            Err(e) => last = Some(e),
        }
    }
    let why = last.map_or_else(|| "no stack size to try".to_string(), |e| e.to_string());
    Err(io::Error::other(format!("cannot start {n} threads: {why}")))
}

/// The soft cap on this process's address space, if there is one.
fn address_space_cap() -> Option<usize> {
    #[cfg(unix)]
    {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit only fills the struct handed to it.
        if unsafe { libc::getrlimit(libc::RLIMIT_AS, &mut lim) } == 0
            && lim.rlim_cur != libc::RLIM_INFINITY
        {
            return usize::try_from(lim.rlim_cur).ok();
        }
    }
    None
}

fn scan_pooled(
    paths: &[PathBuf],
    opts: &ScanOpts,
    prefetch: Option<&du_git::Prefetch>,
    depth_limit: usize,
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
    let mut unread = Vec::new();
    let mut too_deep = 0u64;
    let mut shared = crate::du_extent::SharedSet::default();
    let mut partial = false;
    let mut types = TypeMap::new();
    for p in paths {
        let abs = absolute(p)?;
        let meta = if opts.follow {
            du_fs::stat_follow(&abs)?
        } else {
            du_fs::lstat(&abs)?
        };
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
            dirs_seen: LinkSet::new(),
            unreadable: AtomicU64::new(0),
            unread: Mutex::new(Vec::new()),
            depth_limit,
            too_deep: AtomicU64::new(0),
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
            probe: Default::default(),
            cut: Default::default(),
            types: Default::default(),
        };
        let (apparent, disk, disk_all, unique, artefact) = if meta.is_dir() {
            enter(
                &ctx,
                None,
                &abs,
                Some(meta),
                0,
                true,
                ctx.filter.as_ref().and_then(|f| f.root_chain.as_ref()),
            )
            .map(|n| (n.apparent, n.disk, n.disk_all, n.unique, n.artefact))
            .unwrap_or((0, 0, 0, 0, None))
        } else {
            ctx.files.fetch_add(1, Ordering::Relaxed);
            let (a, raw) = meta.sizes();
            if opts.types {
                let name = abs.file_name().unwrap_or_default();
                let mut m = ctx.types.lock().unwrap_or_else(|e| e.into_inner());
                add_type(&mut m, name.as_encoded_bytes(), a, raw);
            }
            let (u, d) = if opts.unique || opts.shared_once {
                let s = root_file_share(&ctx, &abs, &meta, raw);
                let d = if opts.shared_once {
                    count_once(meta.dev, raw, &s.shared, &mut 0, &mut Vec::new())
                } else {
                    raw
                };
                (s.unique, d)
            } else {
                (0, raw)
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
            (a, d, raw, u, None)
        };
        unreadable += ctx.unreadable.load(Ordering::Relaxed);
        unread.extend(ctx.unread.into_inner().unwrap_or_else(|e| e.into_inner()));
        too_deep += ctx.too_deep.load(Ordering::Relaxed);
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
            disk_all: opts.shared_once.then_some(disk_all),
        });
        dirs.merge_vec(ctx.top_dirs.into_vec());
        files.merge_vec(ctx.top_files.into_vec());
        if let Ok(s) = ctx.shared.into_inner() {
            shared.merge(s);
        }
        merge_types(
            &mut types,
            ctx.types.into_inner().unwrap_or_else(|e| e.into_inner()),
        );
    }
    Ok(Report {
        roots,
        dirs: dirs.into_vec(),
        files: files.into_vec(),
        unreadable,
        unread: {
            // Threads finish in any order: sort, so runs agree.
            unread.sort();
            unread.truncate(UNREAD_KEPT);
            unread
        },
        too_deep,
        depth_limit,
        shared: opts.unique.then(|| SharedTotals {
            distinct: shared.distinct(),
            referenced: shared.referenced,
        }),
        partial,
        types: type_rows(types, opts.sort),
    })
}

/// [`Ctx::share_of`] for a PATH that is itself a file.
fn root_file_share(
    ctx: &Ctx<'_>,
    path: &Path,
    st: &du_fs::Stat,
    disk: u64,
) -> crate::du_extent::FileShare {
    let whole = crate::du_extent::FileShare {
        unique: disk,
        shared: Vec::new(),
    };
    #[cfg(unix)]
    {
        if !st.is_file() || disk == 0 {
            return whole;
        }
        match ctx.probe.wants(st.dev, disk, || fs::File::open(path).ok()) {
            Some(f) => ctx.note_shares(st.dev, crate::du_extent::file_share(&f, disk)),
            None => whole,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (ctx, path, st);
        whole
    }
}

/// `du --devices`: the mounted file systems, as text or JSON.
fn devices_out(prog: &str, paths: &[PathBuf], json: bool, si: bool, other_mode: bool) -> i32 {
    if !paths.is_empty() || other_mode {
        eprintln!("{prog}: --devices lists every mounted file system; it takes no PATH and no other mode but --json");
        return 2;
    }
    crate::util::set_si_units(si);
    match crate::du_dev::listed() {
        Ok(devs) if json => println!("{}", crate::du_dev::json(&devs)),
        Ok(devs) => print!("{}", crate::du_dev::table(&devs)),
        Err(e) => {
            eprintln!("{prog}: cannot list devices: {e}");
            return 1;
        }
    }
    0
}

/// `du --ncdu=FILE` (JSON) or `--ncdu-binary=FILE`: write `path` as an
/// ncdu export.
/// `du --dupes`: find the duplicate groups and report them, or list or
/// trash every copy but the one each group keeps.
/// What `--dupes` does with the extra copies.
#[derive(Clone, Copy, PartialEq)]
enum DupesAct {
    List,
    Trash,
    Link,
    Clone,
}

fn dupes_out(
    prog: &str,
    paths: &[PathBuf],
    opts: &ScanOpts,
    min_size: u64,
    json: bool,
    print0: bool,
    act: DupesAct,
) -> i32 {
    use std::io::Write;
    let files = crate::du_dupes::walk(
        paths,
        crate::du_dupes::Opts {
            min_size,
            one_file_system: opts.one_file_system,
            include_kernfs: opts.include_kernfs,
        },
    );
    let groups = crate::du_dupes::find(files);
    if json {
        println!("{}", crate::du_dupes::to_json(&groups));
        return 0;
    }
    let extras: Vec<&Path> = groups
        .iter()
        .flat_map(|g| g.extras.iter())
        .flat_map(|f| std::iter::once(f.path.as_path()).chain(f.links.iter().map(PathBuf::as_path)))
        .collect();
    if print0 {
        let mut out = std::io::stdout().lock();
        for p in &extras {
            let _ = out.write_all(p.as_os_str().as_encoded_bytes());
            let _ = out.write_all(b"\0");
        }
        return 0;
    }
    crate::du_dupes::print_text(&groups, opts.top);
    if matches!(act, DupesAct::Link | DupesAct::Clone) {
        use crate::du_dupes::Replace;
        let (how, verb) = if act == DupesAct::Link {
            (Replace::HardLink, "linked")
        } else {
            (Replace::Clone, "cloned")
        };
        let done = crate::du_dupes::replace_extras(&groups, how);
        for s in &done.skipped {
            eprintln!("{prog}: not {verb}: {s}");
        }
        println!(
            "{verb} {} name(s) to the kept copies, freeing {}",
            done.names,
            crate::util::format_bytes(done.freed)
        );
        return i32::from(!done.skipped.is_empty());
    }
    if act == DupesAct::Trash && !extras.is_empty() {
        let mut args: Vec<String> = vec!["--".into()];
        args.extend(extras.iter().map(|p| p.to_string_lossy().into_owned()));
        return crate::put::run(prog, &args);
    }
    0
}

fn ncdu_out(prog: &str, path: &Path, out: &Path, binary: bool, opts: &ScanOpts, now: i64) -> i32 {
    let spec = opts.filter.as_deref();
    let mounts = crate::du_ncdu::Mounts {
        one_file_system: opts.one_file_system,
        include_kernfs: opts.include_kernfs,
    };
    let write = |w: &mut dyn Write| {
        let mut w = io::BufWriter::new(w);
        if binary {
            crate::du_ncdu::export_binary_with(path, mounts, spec, &mut w)
        } else {
            crate::du_ncdu::export_with(path, mounts, spec, now, &mut w)
        }
    };
    let written = if out.as_os_str() == "-" {
        write(&mut io::stdout().lock())
    } else {
        fs::File::create(out).and_then(|mut f| write(&mut f))
    };
    let done = match written {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{prog}: cannot write ncdu export '{}': {e}", out.display());
            return 1;
        }
    };
    if done.unreadable > 0 {
        eprintln!(
            "{prog}: {} entries could not be read (marked read_error)",
            done.unreadable
        );
    }
    if done.too_deep > 0 {
        eprintln!(
            "{prog}: {} directories past {MAX_DEPTH} levels or {} KiB of path written without their contents",
            done.too_deep,
            MAX_PATH_BYTES / 1024
        );
    }
    0
}

pub(crate) fn absolute(p: &Path) -> io::Result<PathBuf> {
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
    // A bounded read: listing all of a huge directory here would cost
    // as much as the walk's own listing of it, for 48 entries.
    let (entries, _) = dir.entries_upto(PROBE_STATS);
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
        let (inner, _) = child.entries_upto(PROBE_STATS / PROBE_SUBDIRS);
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

/// An `Item::mtime` that is not known (an ncdu export written without
/// `-e` records none).
pub const MTIME_UNKNOWN: i64 = i64::MIN;

/// Compact age such as `45s`, `3h`, `12d`, `2y` (time since `mtime`), or
/// `-` for [`MTIME_UNKNOWN`].
pub fn format_age(mtime: i64, now: i64) -> String {
    if mtime == MTIME_UNKNOWN {
        return "-".into();
    }
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

/// Local modification time as `YYYY-MM-DD HH:MM` (gdu --show-mtime), or
/// `-` for [`MTIME_UNKNOWN`].
pub fn format_mtime(mtime: i64) -> String {
    if mtime == MTIME_UNKNOWN {
        return "-".into();
    }
    #[cfg(unix)]
    let iso = crate::info::local_string(mtime);
    #[cfg(windows)]
    let iso = crate::platform::epoch_to_local(mtime);
    #[cfg(not(any(unix, windows)))]
    let iso = crate::platform::epoch_to_iso_utc(mtime);
    iso.get(..16).unwrap_or(&iso).replacen('T', " ", 1)
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
        if it.mtime == MTIME_UNKNOWN {
            "null".to_string()
        } else {
            it.mtime.to_string()
        },
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
                "{{\"path\":\"{}\",\"disk\":{}{},\"apparent\":{}{},\"files\":{},\"links\":{},\"dirs\":{},\"excluded\":{},\"inodes\":{},\"artefact\":{}}}",
                json_escape(&t.path.to_string_lossy()),
                t.disk,
                t.disk_all
                    .map(|d| format!(",\"disk_all\":{d}"))
                    .unwrap_or_default(),
                t.apparent,
                unique_json(t.unique),
                t.files,
                t.links,
                t.dirs,
                t.excluded,
                t.inodes(),
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
    // Only a --types scan has the key.
    let types = if r.types.is_empty() {
        String::new()
    } else {
        let rows: Vec<String> = r
            .types
            .iter()
            .map(|t| {
                format!(
                    "{{\"type\":\"{}\",\"disk\":{},\"apparent\":{},\"files\":{}}}",
                    json_escape(&t.ext),
                    t.disk,
                    t.apparent,
                    t.files
                )
            })
            .collect();
        format!(",\"types\":[{}]", rows.join(","))
    };
    let unread = if r.unread.is_empty() {
        String::new()
    } else {
        let rows: Vec<String> = r
            .unread
            .iter()
            .map(|(p, why)| {
                format!(
                    "{{\"path\":\"{}\",\"error\":\"{}\"}}",
                    json_escape(&p.to_string_lossy()),
                    json_escape(why)
                )
            })
            .collect();
        format!(",\"unreadable_paths\":[{}]", rows.join(","))
    };
    let too_deep = if r.too_deep > 0 {
        format!(",\"too_deep\":{}", r.too_deep)
    } else {
        String::new()
    };
    format!(
        "{{\"roots\":[{}],\"dirs\":[{}],\"files\":[{}],\"unreadable\":{}{unread}{too_deep}{shared}{partial}{types},\"trash\":{}}}",
        roots.join(","),
        dirs.join(","),
        files.join(","),
        r.unreadable,
        trash_json(trash)
    )
}

/// The `--types` table: the first `top` rows, then one line for the rest.
fn print_types(title: &str, rows: &[TypeRow], top: usize) {
    if rows.is_empty() {
        return;
    }
    println!("{title}");
    println!("{:>10}  {:>10}  {:>9}  TYPE", "DISK", "APPARENT", "FILES");
    let line = |disk: u64, apparent: u64, files: u64, name: &str| {
        println!(
            "{:>10}  {:>10}  {:>9}  {name}",
            format_bytes(disk),
            format_bytes(apparent),
            files
        );
    };
    for t in rows.iter().take(top) {
        let name = if t.ext.is_empty() {
            "(no extension)".to_string()
        } else {
            format!(".{}", crate::util::term_text(&t.ext))
        };
        line(t.disk, t.apparent, t.files, &name);
    }
    let rest = &rows[top.min(rows.len())..];
    if !rest.is_empty() {
        let sum = |f: fn(&TypeRow) -> u64| rest.iter().map(f).fold(0u64, u64::saturating_add);
        line(
            sum(|t| t.disk),
            sum(|t| t.apparent),
            sum(|t| t.files),
            &format!(
                "({} more {})",
                rest.len(),
                if rest.len() == 1 { "type" } else { "types" }
            ),
        );
    }
}

/// Optional columns of the plain du rows.
#[derive(Clone, Copy)]
struct Columns<'a> {
    /// `-C`: the ITEMS column.
    count: bool,
    /// `--show-mtime`: MTIME in place of AGE.
    show_mtime: bool,
    /// `--show-relative-size`: each row's share of its scan root, by
    /// apparent size when `apparent`, else by disk use.
    share: Option<(&'a [RootTotals], bool)>,
    /// `--reverse`: the smallest (or oldest) row first, so the largest
    /// ends at the bottom next to the prompt, as dust -r.
    reverse: bool,
}

/// `42.5%`: the part of the scan root that holds `it`, or `-` when no
/// root holds it or the root is empty.
fn share_of(it: &Item, roots: &[RootTotals], apparent: bool) -> String {
    let root = roots
        .iter()
        .filter(|r| it.path.starts_with(&r.path))
        .max_by_key(|r| r.path.as_os_str().len());
    let pick = |a: u64, d: u64| if apparent { a } else { d };
    match root.map(|r| pick(r.apparent, r.disk)) {
        Some(total) if total > 0 => {
            let part = pick(it.apparent, it.disk) as f64 * 100.0 / total as f64;
            format!("{:.1}%", part.min(100.0))
        }
        _ => "-".into(),
    }
}

fn print_table(title: &str, items: &[Item], now: i64, cols: Columns) {
    let Columns {
        count,
        show_mtime,
        share,
        reverse,
    } = cols;
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
    // --show-mtime swaps the AGE column for the local modification time.
    let (when, when_width) = if show_mtime {
        ("MTIME", 16)
    } else {
        ("AGE", 4)
    };
    let shares = |v: &str| match share {
        Some(_) => format!("{v:>6}  "),
        None => String::new(),
    };
    let when_of = |it: &Item| {
        if show_mtime {
            format_mtime(it.mtime)
        } else {
            format_age(it.mtime, now)
        }
    };
    println!(
        "{:>10}  {:>10}  {}{}{}{:>when_width$}  {:<width$}  PATH",
        "DISK",
        "APPARENT",
        shares("SHARE"),
        uniques("UNIQUE"),
        counts("ITEMS"),
        when,
        "NOTE"
    );
    let rows: Box<dyn Iterator<Item = &Item>> = if reverse {
        Box::new(items.iter().rev())
    } else {
        Box::new(items.iter())
    };
    for it in rows {
        let mut path = crate::util::term_path(&it.path);
        if it.is_dir {
            path.push(std::path::MAIN_SEPARATOR);
        }
        println!(
            "{:>10}  {:>10}  {}{}{}{:>when_width$}  {:<width$}  {}",
            format_bytes(it.disk),
            format_bytes(it.apparent),
            shares(&share.map_or(String::new(), |(r, a)| share_of(it, r, a))),
            uniques(&it.unique.map(format_bytes).unwrap_or_default()),
            counts(&it.items.to_string()),
            when_of(it),
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

/// Keep the stricter of two age cutoffs, so the age options combine
/// in any order.
fn tighten(cut: &mut Option<i64>, new: i64, pick: fn(i64, i64) -> i64) {
    *cut = Some(cut.map_or(new, |c| pick(c, new)));
}

/// `YYYY-MM-DD` (local midnight) or `YYYY-MM-DDTHH:MM[:SS]` in local
/// time, with a space allowed for the `T`, as a unix time.
fn parse_date(s: &str) -> Result<i64, String> {
    let bad = || format!("invalid DATE '{s}' (want YYYY-MM-DD or YYYY-MM-DDTHH:MM[:SS])");
    let mut b = s.as_bytes().to_vec();
    match b.len() {
        10 => b.extend_from_slice(b"T00:00:00"),
        16 => b.extend_from_slice(b":00"),
        19 => {}
        _ => return Err(bad()),
    }
    if b[10] == b' ' {
        b[10] = b'T';
    }
    let num = |r: std::ops::Range<usize>| -> Option<u32> {
        let d = &b[r];
        if d.iter().all(u8::is_ascii_digit) {
            std::str::from_utf8(d).ok()?.parse().ok()
        } else {
            None
        }
    };
    let (Some(year), Some(mon), Some(day), Some(hour), Some(min), Some(sec)) = (
        num(0..4),
        num(5..7),
        num(8..10),
        num(11..13),
        num(14..16),
        num(17..19),
    ) else {
        return Err(bad());
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match mon {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1..=12 => 31,
        _ => 0,
    };
    if !(1..=days).contains(&day) || hour > 23 || min > 59 || sec > 59 {
        return Err(bad());
    }
    local_to_epoch(&b, [year, mon, day, hour, min, sec]).ok_or_else(bad)
}

/// Unix time of a local wall-clock time, through mktime.
#[cfg(unix)]
fn local_to_epoch(text: &[u8], _: [u32; 6]) -> Option<i64> {
    crate::info::parse_local_to_epoch(std::str::from_utf8(text).ok()?)
}

/// Unix time of a local wall-clock time, through the current time zone.
#[cfg(windows)]
fn local_to_epoch(_: &[u8], [y, mo, d, h, mi, s]: [u32; 6]) -> Option<i64> {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::Time::TzSpecificLocalTimeToSystemTime;
    let field = |v: u32| u16::try_from(v).ok();
    let local = SYSTEMTIME {
        wYear: field(y)?,
        wMonth: field(mo)?,
        wDayOfWeek: 0,
        wDay: field(d)?,
        wHour: field(h)?,
        wMinute: field(mi)?,
        wSecond: field(s)?,
        wMilliseconds: 0,
    };
    // SAFETY: SYSTEMTIME is plain data, so all zeroes is a valid value.
    let mut utc: SYSTEMTIME = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers name live SYSTEMTIME values; a null zone
    // means the current one.
    if unsafe { TzSpecificLocalTimeToSystemTime(std::ptr::null(), &local, &mut utc) } == 0 {
        return None;
    }
    let days = days_from_civil(utc.wYear.into(), utc.wMonth.into(), utc.wDay.into());
    Some(
        days * 86_400
            + i64::from(utc.wHour) * 3600
            + i64::from(utc.wMinute) * 60
            + i64::from(utc.wSecond),
    )
}

/// Days from 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// days_from_civil).
#[cfg(windows)]
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn parse_count(s: &str, what: &str) -> Result<usize, String> {
    s.parse::<usize>()
        .map_err(|_| format!("invalid {what} '{s}'"))
}

/// `--files-from=FILE` or `--files0-from=FILE` (true for the NUL form).
type PathListArg<'a> = Option<(Result<&'a str, String>, bool)>;

fn path_list_arg<'a>(args: &'a [String], i: &mut usize, arg: &'a str) -> PathListArg<'a> {
    if let Some(v) = take_value(args, i, arg, "--files-from", None) {
        return Some((v, false));
    }
    take_value(args, i, arg, "--files0-from", None).map(|v| (v, true))
}

/// The paths of a `--files-from` (one per line) or `--files0-from` (NUL
/// separated) list, read from FILE or, for `-`, stdin. Blank entries are
/// skipped; a line's trailing CR goes, so CRLF lists work.
fn read_path_list(src: &str, nul: bool) -> io::Result<Vec<PathBuf>> {
    let mut bytes = Vec::new();
    if src == "-" {
        io::Read::read_to_end(&mut io::stdin(), &mut bytes)?;
    } else {
        bytes = fs::read(crate::util::arg_path(src))?;
    }
    let sep = if nul { b'\0' } else { b'\n' };
    let mut out = Vec::new();
    for mut part in bytes.split(|&b| b == sep) {
        if !nul {
            part = part.strip_suffix(b"\r").unwrap_or(part);
        }
        if part.is_empty() {
            continue;
        }
        #[cfg(unix)]
        let p = {
            use std::os::unix::ffi::OsStrExt;
            PathBuf::from(std::ffi::OsStr::from_bytes(part))
        };
        #[cfg(not(unix))]
        let p = PathBuf::from(String::from_utf8_lossy(part).into_owned());
        out.push(p);
    }
    Ok(out)
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
    // --files-from / --files0-from: an empty list scans nothing, not ".".
    let mut listed = false;
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
    let mut ncdu: Option<PathBuf> = None;
    let mut ncdu_binary = false;
    let mut devices = false;
    let mut dupes = false;
    let mut trash_dupes = false;
    let mut link_dupes = false;
    let mut clone_dupes = false;
    let mut min_size: u64 = 1;
    let mut from_file: Option<PathBuf> = None;
    let mut threads: Option<usize> = None;
    let mut filter = du_filter::Spec::default();
    // --type extensions: only files with one of these count.
    let mut keep_types: Vec<String> = Vec::new();
    let mut count = false;
    let mut show_mtime = false;
    let mut relative = false;
    let mut reverse = false;
    let mut summarize = false;
    let mut skip_total = false;
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
        } else if let Some(v) = take_value(args, &mut i, arg, "--ncdu", None) {
            match v {
                Ok(_) if ncdu.is_some() => bail!("--ncdu and --ncdu-binary write one export"),
                Ok(v) => ncdu = Some(crate::util::arg_path(v)),
                Err(e) => bail!(e),
            }
        } else if let Some(v) = take_value(args, &mut i, arg, "--ncdu-binary", None) {
            match v {
                Ok(_) if ncdu.is_some() => bail!("--ncdu and --ncdu-binary write one export"),
                Ok(v) => {
                    ncdu = Some(crate::util::arg_path(v));
                    ncdu_binary = true;
                }
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
        } else if let Some(v) = take_value(args, &mut i, arg, "--type", None) {
            match v.and_then(|v| du_filter::extensions(v, "--type")) {
                Ok(exts) => keep_types.extend(exts),
                Err(e) => bail!(e),
            }
        } else if let Some(v) = take_value(args, &mut i, arg, "--exclude-type", None) {
            match v.and_then(|v| du_filter::extensions(v, "--exclude-type")) {
                Ok(exts) => filter
                    .patterns
                    .extend(exts.iter().map(|e| format!("*.{e}"))),
                Err(e) => bail!(e),
            }
        } else if let Some(v) = take_value(args, &mut i, arg, "--exclude", None) {
            match v {
                Ok(v) if du_filter::Pattern::parse(v).is_some() => filter.patterns.push(v.into()),
                Ok(v) => bail!(format!("--exclude: empty pattern '{v}'")),
                Err(e) => bail!(e),
            }
        } else if let Some((v, nul)) = path_list_arg(args, &mut i, arg) {
            let v = match v {
                Ok(v) => v,
                Err(e) => bail!(e),
            };
            match read_path_list(v, nul) {
                Ok(list) => paths.extend(list),
                Err(e) => {
                    eprintln!("{prog}: cannot read path list '{v}': {e}");
                    return 1;
                }
            }
            listed = true;
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
                Ok(d) => tighten(
                    &mut opts.older_cutoff,
                    now - d.saturating_mul(86_400),
                    i64::min,
                ),
                Err(e) => bail!(e),
            }
        } else if let Some(v) = take_value(args, &mut i, arg, "--newer-than", None) {
            match v.and_then(parse_days) {
                Ok(d) => tighten(
                    &mut opts.newer_cutoff,
                    now - d.saturating_mul(86_400),
                    i64::max,
                ),
                Err(e) => bail!(e),
            }
        } else if let Some(v) = take_value(args, &mut i, arg, "--since", None) {
            // At or after DATE: newer than the second before it.
            match v.and_then(parse_date) {
                Ok(t) => tighten(&mut opts.newer_cutoff, t - 1, i64::max),
                Err(e) => bail!(e),
            }
        } else if let Some(v) = take_value(args, &mut i, arg, "--until", None) {
            // Before DATE: at or before the second before it.
            match v.and_then(parse_date) {
                Ok(t) => tighten(&mut opts.older_cutoff, t - 1, i64::min),
                Err(e) => bail!(e),
            }
        } else {
            match arg {
                "--" => no_more_opts = true,
                "-x" | "--one-file-system" => opts.one_file_system = true,
                "--include-kernfs" => opts.include_kernfs = true,
                "-L" | "--follow-symlinks" => opts.follow = true,
                "--apparent" => opts.sort = SortKey::Apparent,
                "-C" | "--count" => count = true,
                "--show-mtime" => show_mtime = true,
                "--show-relative-size" => relative = true,
                "--reverse" => reverse = true,
                // GNU du -s and gdu --summarize: the totals, no rows.
                "-s" | "--summarize" => summarize = true,
                "--skip-total" => skip_total = true,
                "--inodes" => opts.sort = SortKey::Items,
                "--all-dirs" => opts.all_dirs = true,
                "--artefacts" => opts.artefacts_only = true,
                "--exclude-caches" => filter.exclude_caches = true,
                // gdu's --no-hidden: a dot name at any depth, in order
                // with --exclude so a later !pattern can bring one back.
                "--no-hidden" => filter.patterns.push(".*".to_string()),
                "--gitignore" => filter.gitignore = true,
                "--dirs" => only_dirs = true,
                "--files" => only_files = true,
                "--devices" => devices = true,
                "--json" => json = true,
                "--print0" => print0 = true,
                "--dupes" => dupes = true,
                "--trash-dupes" => trash_dupes = true,
                "--link-dupes" => link_dupes = true,
                "--clone-dupes" => clone_dupes = true,
                a if a.starts_with("--min-size=") => {
                    match crate::filters::parse_size(&a["--min-size=".len()..]) {
                        Some(n) => min_size = n,
                        None => {
                            eprintln!(
                                "{prog}: invalid --min-size '{}' (a size such as 1M)",
                                &a["--min-size=".len()..]
                            );
                            return 2;
                        }
                    }
                }
                "--stacks" => stacks = true,
                "--unique" => opts.unique = true,
                "--shared" => opts.shared_once = true,
                "--types" => opts.types = true,
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
    if summarize {
        // After the loop, so a later -n does not bring rows back.
        opts.top = 0;
    }
    if summarize && skip_total {
        eprintln!("{prog}: give --summarize or --skip-total, not both");
        return 2;
    }
    if !keep_types.is_empty() {
        // First, so every --exclude and --exclude-type still applies.
        let mut keep = du_filter::keep_only(&keep_types);
        keep.append(&mut filter.patterns);
        filter.patterns = keep;
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
    if opts.follow {
        if cfg!(windows) {
            eprintln!("{prog}: --follow-symlinks is not supported on Windows");
            return 2;
        }
        // A followed link lists the target's size under the link's name;
        // trashing that name would remove only the link.
        if print0 || pick || interactive || ncdu.is_some() {
            eprintln!(
                "{prog}: --follow-symlinks only reports; it does not combine with \
                 --print0, --pick, --interactive or --ncdu"
            );
            return 2;
        }
    }
    if devices {
        return devices_out(
            prog,
            &paths,
            json,
            si,
            ncdu.is_some()
                || output.is_some()
                || from_file.is_some()
                || stacks
                || print0
                || pick
                || interactive
                || opts.unique
                || opts.shared_once
                || opts.types,
        );
    }
    if paths.is_empty() && listed {
        eprintln!("{prog}: the path list names nothing to scan");
        return 2;
    }
    if paths.is_empty() {
        paths.push(PathBuf::from("."));
    }
    if (trash_dupes || link_dupes || clone_dupes || min_size != 1) && !dupes {
        eprintln!(
            "{prog}: --trash-dupes, --link-dupes, --clone-dupes and --min-size go with --dupes"
        );
        return 2;
    }
    if dupes {
        if ncdu.is_some()
            || output.is_some()
            || from_file.is_some()
            || stacks
            || pick
            || interactive
            || opts.types
            || [json || print0, trash_dupes, link_dupes, clone_dupes]
                .iter()
                .filter(|&&b| b)
                .count()
                > 1
        {
            eprintln!(
                "{prog}: --dupes takes one of --json, --print0, --trash-dupes, --link-dupes or --clone-dupes, with -x, -n and --min-size"
            );
            return 2;
        }
        crate::util::set_si_units(si);
        let act = if trash_dupes {
            DupesAct::Trash
        } else if link_dupes {
            DupesAct::Link
        } else if clone_dupes {
            DupesAct::Clone
        } else {
            DupesAct::List
        };
        return dupes_out(prog, &paths, &opts, min_size, json, print0, act);
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
    if output.is_some() && (from_file.is_some() || json || print0 || pick || interactive) {
        eprintln!("{prog}: --output writes a snapshot; it takes no other output mode");
        return 2;
    }
    if let Some(out) = &ncdu {
        let flag = if ncdu_binary {
            "--ncdu-binary"
        } else {
            "--ncdu"
        };
        if output.is_some()
            || from_file.is_some()
            || stacks
            || json
            || print0
            || pick
            || interactive
            || opts.unique
            || opts.shared_once
            || opts.types
        {
            eprintln!("{prog}: {flag} writes an ncdu export; it takes no other output mode");
            return 2;
        }
        if paths.len() > 1 {
            eprintln!("{prog}: {flag} exports one PATH");
            return 2;
        }
        return ncdu_out(prog, &paths[0], out, ncdu_binary, &opts, now);
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
    if opts.shared_once
        && (from_file.is_some() || output.is_some() || stacks || interactive || opts.types)
    {
        eprintln!(
            "{prog}: --shared changes the sizes of a live listing; it takes no -f, -o, --stacks, -i or --types"
        );
        return 2;
    }
    if opts.types
        && (from_file.is_some() || output.is_some() || stacks || print0 || pick || interactive)
    {
        eprintln!(
            "{prog}: --types lists file types from a live scan; it takes no -f, -o, --stacks, --print0, --pick or -i"
        );
        return 2;
    }
    if opts.types && opts.sort == SortKey::Mtime {
        eprintln!("{prog}: --types ranks by disk use, --apparent size or --sort=items");
        return 2;
    }
    if stacks && !matches!(opts.sort, SortKey::Disk | SortKey::Apparent) {
        eprintln!("{prog}: --stacks weighs by disk use or --apparent size");
        return 2;
    }
    // --show-relative-size weighs rows as the ranking does.
    let apparent_share = opts.sort == SortKey::Apparent;
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
                Columns {
                    count: false,
                    show_mtime,
                    share: relative.then_some((&report.roots[..], apparent_share)),
                    reverse,
                },
            );
            for t in report.roots.iter().filter(|_| !skip_total) {
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
        return crate::du_tui::run(prog, &paths, opts, use_git, read_only);
    }
    opts.control = Some(stop);
    // Git probes run beside the walk. Under --no-git, or without git on
    // PATH, there is nothing to start.
    let prefetch = if use_git {
        du_git::Prefetch::start()
    } else {
        None
    };
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
    let scanned = scan_on(&paths, &opts, prefetch.as_ref(), threads);
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
        if opts.types {
            print_types(&opts.sort.title("file types"), &report.types, opts.top);
        } else {
            let cols = Columns {
                count,
                show_mtime,
                share: relative.then_some((&report.roots[..], apparent_share)),
                reverse,
            };
            print_table(&opts.sort.title("directories"), &report.dirs, now, cols);
            print_table(&opts.sort.title("files"), &report.files, now, cols);
        }
        // --skip-total: the rows alone, as dust --skip-total.
        for t in report.roots.iter().filter(|_| !skip_total) {
            let kind = t.artefact.map(|k| format!(" ({k})")).unwrap_or_default();
            // Under -L a repeat may come through a symlink as well as a
            // hard link, so the total names both as repeated names.
            let links = if t.links > 0 && opts.follow {
                format!(" (+{})", plural(t.links, "repeated name", "repeated names"))
            } else if t.links > 0 {
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
            let once = t
                .disk_all
                .map(|d| {
                    format!(
                        " with shared extents once ({} counting each reference)",
                        format_bytes(d)
                    )
                })
                .unwrap_or_default();
            println!(
                "Total {}{kind}: {} on disk{once}{unique}, {} apparent, {}{links}, {}, {}{excluded}",
                crate::util::term_path(&t.path),
                format_bytes(t.disk),
                format_bytes(t.apparent),
                plural(t.files, "file", "files"),
                plural(t.dirs, "directory", "directories"),
                plural(t.inodes(), "inode", "inodes")
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
        for (p, why) in report.unread.iter().take(UNREAD_SHOWN) {
            eprintln!("{prog}: cannot read '{}': {why}", crate::util::err_path(p));
        }
        let named = report.unread.len().min(UNREAD_SHOWN) as u64;
        let more = report.unreadable - named;
        if more > 0 && named > 0 {
            eprintln!(
                "{prog}: and {more} more could not be read (--json lists up to {UNREAD_KEPT})"
            );
        } else if named == 0 {
            eprintln!(
                "{prog}: {} could not be read",
                plural(report.unreadable, "entry", "entries")
            );
        }
    }
    if report.too_deep > 0 {
        eprintln!(
            "{prog}: {} past {} levels or {} KiB of path counted without their contents",
            plural(report.too_deep, "directory", "directories"),
            report.depth_limit,
            MAX_PATH_BYTES / 1024
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

    #[test]
    fn type_keys_take_the_last_extension_in_lowercase() {
        assert_eq!(type_key(b"a.tar.GZ"), b"gz");
        assert_eq!(type_key(b".bashrc"), b"");
        assert_eq!(type_key(b"Makefile"), b"");
        assert_eq!(type_key(b"trailing."), b"");
        assert_eq!(type_key(b"x.0123456789abcdef"), b"");
        assert_eq!(type_key(b"x.0123456789ab"), b"0123456789ab");
    }

    #[test]
    fn type_rows_rank_by_the_sort_key() {
        let mut m = TypeMap::new();
        add_type(&mut m, b"a.rs", 10, 4096);
        add_type(&mut m, b"b.rs", 10, 4096);
        add_type(&mut m, b"c.iso", 9000, 8192);
        let exts = |rows: Vec<TypeRow>| rows.into_iter().map(|r| r.ext).collect::<Vec<_>>();
        assert_eq!(exts(type_rows(m.clone(), SortKey::Disk)), ["iso", "rs"]);
        assert_eq!(exts(type_rows(m.clone(), SortKey::Items)), ["rs", "iso"]);
        let rows = type_rows(m, SortKey::Apparent);
        assert_eq!(rows[1].files, 2);
        assert_eq!(rows[1].disk, 8192);
    }

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
        assert_eq!(format_age(MTIME_UNKNOWN, 0), "-");
        assert_eq!(format_mtime(MTIME_UNKNOWN), "-");
        let m = format_mtime(1_700_000_000);
        assert_eq!(m.len(), 16, "{m}");
        assert_eq!((&m[4..5], &m[10..11], &m[13..14]), ("-", " ", ":"), "{m}");
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
    fn count_once_counts_each_shared_byte_of_a_file_once() {
        use crate::du_extent::Extent;
        let sh = |physical, length| Extent {
            physical,
            length,
            shared: true,
        };
        let (mut base, mut ranges) = (0, Vec::new());
        // No shared extents: all of it is the file's own.
        assert_eq!(count_once(1, 4096, &[], &mut base, &mut ranges), 4096);
        assert_eq!((base, ranges.len()), (4096, 0));
        // 8 KiB of its own, and two references that overlap by 4 KiB.
        let d = count_once(
            1,
            32768,
            &[sh(0, 12288), sh(8192, 12288)],
            &mut base,
            &mut ranges,
        );
        assert_eq!(d, 8192 + 20480);
        assert_eq!(base, 4096 + 8192);
        assert_eq!(ranges, vec![(1, 0, 20480)]);
        // Compressed data can map longer than its blocks: never more than
        // the file's allocated size.
        let mut b2 = 0;
        assert_eq!(
            count_once(1, 4096, &[sh(0, 131072)], &mut b2, &mut Vec::new()),
            4096
        );
        assert_eq!(b2, 0);
    }

    /// Clone `from` to a new file `to` with `FICLONE`; false where the
    /// file system cannot.
    #[cfg(target_os = "linux")]
    fn reflink(from: &Path, to: &Path) -> bool {
        const FICLONE: libc::c_ulong = 0x4004_9409;
        let src = fs::File::open(from).unwrap();
        let dst = fs::File::create(to).unwrap();
        use std::os::fd::AsRawFd;
        // SAFETY: both fds are open; FICLONE takes the source fd.
        let r = unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE as _, src.as_raw_fd()) };
        r == 0
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shared_scan_counts_reflinked_copies_once() {
        let root = std::env::temp_dir().join(format!("rtrash-du-shared-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("x")).unwrap();
        let a = root.join("a");
        fs::write(&a, vec![3u8; 1 << 20]).unwrap();
        fs::File::open(&a).unwrap().sync_all().unwrap();
        fs::write(root.join("x/own"), vec![4u8; 100_000]).unwrap();
        let cloned = reflink(&a, &root.join("x/b"));
        let opts = ScanOpts {
            shared_once: true,
            ..ScanOpts::default()
        };
        let r = scan(std::slice::from_ref(&root), &opts).unwrap();
        let t = &r.roots[0];
        let all = t.disk_all.unwrap();
        let blocks = fs::symlink_metadata(&a).unwrap().blocks() * 512;
        if cloned {
            // b counts in full in x, but only once in the whole tree.
            assert_eq!(t.disk + blocks, all, "{t:?}");
            let x = r.dirs.iter().find(|d| d.path.ends_with("x")).unwrap();
            assert!(x.disk >= blocks, "{x:?}");
        } else {
            // Nothing shared here (no reflinks on this file system).
            assert_eq!(t.disk, all, "{t:?}");
        }
        let plain = scan(std::slice::from_ref(&root), &ScanOpts::default()).unwrap();
        assert_eq!(plain.roots[0].disk, all);
        assert!(plain.roots[0].disk_all.is_none());
        let j = report_json(&r, &TrashSummary::default());
        assert!(j.contains(&format!("\"disk_all\":{all}")), "{j}");
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
            unread: Vec::new(),
            too_deep: 0,
            depth_limit: MAX_DEPTH,
            shared: None,
            partial: false,
            types: Vec::new(),
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

    #[test]
    fn dates_parse_in_local_time_and_reject_the_impossible() {
        let day = parse_date("2024-02-29").unwrap();
        assert_eq!(parse_date("2024-02-29T00:00:00"), Ok(day));
        assert_eq!(
            parse_date("2024-02-29 12:30"),
            Ok(day + 12 * 3600 + 30 * 60)
        );
        for bad in [
            "2023-02-29",
            "2024-13-01",
            "2024-1-01",
            "2024-01-01T24:00",
            "yesterday",
            "",
        ] {
            assert!(parse_date(bad).is_err(), "{bad}");
        }
        #[cfg(windows)]
        {
            assert_eq!(days_from_civil(1970, 1, 1), 0);
            assert_eq!(days_from_civil(2020, 6, 15), 18_428);
            assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        }
        let mut cut = None;
        tighten(&mut cut, 10, i64::max);
        tighten(&mut cut, 5, i64::max);
        assert_eq!(cut, Some(10));
    }

    /// A chain of `levels` directories named `d` under `root`, made with
    /// paths relative to each level, since the whole path passes
    /// `PATH_MAX`; `file_at` gets a 5000 byte file `f`.
    #[cfg(unix)]
    fn deep_chain(root: &Path, levels: usize, file_at: usize) {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(root.as_os_str().as_bytes()).unwrap();
        // SAFETY: plain descriptor calls on NUL-terminated names; each
        // descriptor is closed once the next one is open.
        unsafe {
            let mut fd = libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
            assert!(fd >= 0);
            for i in 0..levels {
                assert_eq!(libc::mkdirat(fd, c"d".as_ptr(), 0o755), 0);
                let next = libc::openat(fd, c"d".as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
                assert!(next >= 0);
                libc::close(fd);
                fd = next;
                if i == file_at {
                    let f = libc::openat(fd, c"f".as_ptr(), libc::O_WRONLY | libc::O_CREAT, 0o644);
                    assert!(f >= 0);
                    assert_eq!(libc::write(f, [7u8; 5000].as_ptr().cast(), 5000), 5000);
                    libc::close(f);
                }
            }
            libc::close(fd);
        }
    }

    #[test]
    fn smaller_walk_stacks_enter_fewer_levels() {
        assert_eq!(depth_for_stack(WALK_STACK), MAX_DEPTH);
        assert_eq!(depth_for_stack(WALK_STACK / 4), MAX_DEPTH / 4);
        // Rayon's default stack: the size the old fallback walked with.
        assert_eq!(depth_for_stack(2 * 1024 * 1024), MAX_DEPTH / 128);
        assert!(WALK_STACKS.windows(2).all(|w| w[0] > w[1]));
        assert_eq!(WALK_STACKS[0], WALK_STACK);
    }

    #[cfg(unix)]
    #[test]
    fn deep_trees_count_past_path_max_and_stop_at_max_depth() {
        let root = std::env::temp_dir().join(format!("rtrash-du-deep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        // The file's level lies past PATH_MAX, so it opens relative to a
        // held directory. Levels past MAX_DEPTH are not entered.
        const FILE_AT: usize = libc::PATH_MAX as usize * 5 / 8;
        const _: () = assert!(FILE_AT * 2 > libc::PATH_MAX as usize && FILE_AT < MAX_DEPTH);
        deep_chain(&root, MAX_DEPTH + 3, FILE_AT);
        let opts = ScanOpts {
            top: 0,
            ..ScanOpts::default()
        };
        // One thread: the whole chain runs on a single walk stack.
        let r = scan_on(std::slice::from_ref(&root), &opts, None, Some(1)).unwrap();
        assert_eq!(r.unreadable, 0);
        assert_eq!(r.too_deep, 1);
        assert_eq!(r.roots[0].files, 1);
        assert_eq!(r.roots[0].apparent, 5000);
        // The root, MAX_DEPTH levels walked, and the one counted shut.
        assert_eq!(r.roots[0].dirs as usize, MAX_DEPTH + 2);
        let j = report_json(&r, &TrashSummary::default());
        assert!(j.contains("\"too_deep\":1,"), "{j}");
        fs::remove_dir_all(&root).unwrap();
    }
}
