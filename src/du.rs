//! `rtrash du`: find what to delete. Walks one or more paths in parallel and
//! reports the largest directories and files, with on-disk and apparent sizes.

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, OsStr};
use std::fs;
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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
A PATH given twice counts once. A PATH inside another is refused, since
it would count twice (under -x, one on another file system stays).

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

Filters such as --exclude, --gitignore and --type leave entries out of
the sizes, but trashing a directory takes all of it. So --print0, --pick
and the explorer never offer a directory that holds entries the filters
left out, unless --allow-filtered is given. Age and size filters only
choose rows; every size stays whole.

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
      --show-symlink-target  with -L, show each row that is itself a
                          symlink as PATH -> TARGET, the target as the link
                          stores it (gdu --show-symlink-target); in the
                          table and --tree, and --json adds \"target\"
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
      --ratio           add a RATIO column: disk use as a percent of the
                          apparent size, below 100% where compression or
                          holes save space; --json gives each row and
                          root a ratio key, disk over apparent
      --bars            add a BAR column drawing that share in ten cells,
                          as dust's bars
      --graph-style=STYLE  draw the bars with hash (#, the default),
                          half-block or eighth-block characters, as ncdu
                          --graph-style; implies --bars
      --tree[=DEPTH]    print each PATH as a tree: the rows DEPTH levels
                          below it (default 2) nested under their folders,
                          the --top largest of each folder first, as dust
                          draws them; plain table only
      --dirs-first      in --tree, list each folder's subfolders before its
                          files (ncdu --group-directories-first)
      --no-unicode      draw --tree with ASCII lines (|--, `--), for
                          consoles without UTF-8 (gdu --no-unicode)
  -s, --summarize       print only the totals, no rows (GNU du -s, gdu)
      --strict          exit 1 when any entry could not be read, after the
                          usual output (GNU du's status; the listing, --json,
                          ncdu export, --output and --file, where an older
                          snapshot without the count fails too)
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
                          slower, since it maps every file's extents; btrfs
                          compressed extents count at their uncompressed
                          size, and the summary says how much
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
      --collapse=NAME   list each directory called NAME (or at path NAME)
                          as one row, nothing below it on its own
                          (repeatable; dust --collapse, gdu --collapse-path)
      --exclude-regex=RE  leave out entries whose path matches RE
                          (repeatable; dust -X, gdu --ignore-dirs-pattern)
      --include-regex=RE  count only files whose path matches RE
                          (repeatable; directories are still walked; dust -e)
  -X, --exclude-from=FILE  read patterns from FILE, one per line (# comments
                          and !negations as in .gitignore; - is stdin)
      --filter-from=FILE  leave out what the rsync filter rules in FILE
                          exclude (+ and - lines; the first match wins)
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
                          .git/info/exclude and the core.excludesFile of git
                          config (by default ~/.config/git/ignore), inside
                          git repositories; a ! rule brings nothing back
                          from inside an excluded directory, as in git
      --dirs            list directories only
      --files           list files only
      --older-than=AGE  only list entries whose newest modification is more
                          than AGE old (directories use their whole tree);
                          AGE is DAYS or a duration such as 2h30m, 1w or
                          1y2mo (mo is 30 days, y 365)
      --newer-than=AGE  only list entries modified within the last AGE
      --min-age=AGE     the same as --older-than (gdu)
      --max-age=AGE     the same as --newer-than (gdu)
      --since=DATE      only list entries modified at or after DATE
                          (YYYY-MM-DD, or YYYY-MM-DDTHH:MM[:SS]; local time)
      --until=DATE      only list entries whose newest modification is
                          before DATE
      --home-only       consider only the home trash for the trash summary
      --open-deleted    also report the space that deleted files still open
                          hold on the scanned file systems (Linux /proc,
                          macOS libproc; never in the totals; on Windows a
                          note says it is missing)
      --trash-dir=PATH  consider only this trash directory (repeatable)
      --no-git          do not ask git about listed paths
      --include-dirty   with --print0, also print paths whose git state is
                          dirty, unpushed or unknown
      --allow-filtered  with --print0, --pick or -i, also offer directories
                          that hold entries the filters left out (trashing
                          one takes those too)
      --json            emit a JSON object (scripts)
      --print0          print the listed paths, NUL-separated, for
                          `xargs -0 rtrash put -rf --`
  -o, --output=FILE     write a snapshot of every directory total (to
                          --max-depth) to FILE (- for stdout) instead of
                          listing; compare snapshots with `du diff`
      --dupes           list groups of files with the same contents (size,
                          then edge hash, then full hash); hard links count
                          once, reflinks free nothing; the oldest file stays
                          (online-only cloud files are never read)
      --trash-dupes     trash every copy but the kept one in each group
      --clone-dupes     replace every copy but the kept one with a reflink
                          clone of it, keeping its mode, owner and times
                          (btrfs, XFS, APFS, ReFS)
      --link-dupes      replace every copy but the kept one with a hard
                          link to it (same device only)
      --min-size=SIZE   list only rows of SIZE or more (disk use, or the
                          apparent size under --apparent); totals stay whole.
                          A subdirectory below SIZE hides no parent
                          With --dupes, skip files smaller than SIZE
      --devices         list every mounted file system with its size, use
                          and free space (as df does), instead of scanning
      --ncdu=FILE       write an ncdu export of PATH to FILE (- for
                          stdout) for `ncdu -f FILE` or `du -f FILE`
      --ncdu-binary=FILE  the same in ncdu 2's binary format (ncdu -O),
                          for ncdu 2.6 or later
  -f, --file=FILE       list from a snapshot, or an ncdu export (ncdu -o,
                          ncdu -c -o, ncdu -O, gdu -o), instead of scanning
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
                          show git state and artefact kind; with -f, browse
                          the file's directories, read-only, with no scan
      --no-delete       with -i, browse only: d, u and b do nothing
      --si              show sizes in powers of 1000 (kB, MB, GB)
      --no-prefix       show sizes as plain byte counts, for sort -n and
                          scripts (gdu --no-prefix, GNU du -b); not with
                          --interactive
      --help            display this help and exit
      --version         output version information and exit

Without --si or --no-prefix, the first of RTRASH_BLOCK_SIZE, DU_BLOCK_SIZE,
BLOCK_SIZE and BLOCKSIZE that is set picks the printed units, as in GNU du:
1K prints 1024-byte blocks, K adds the unit, MB uses 1000s, si is --si.
JSON, --print0 and the explorer keep their units.
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
    "collapsed",
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
    /// `--collapse`: directory names (or paths) listed as one row, with
    /// nothing below them listed on its own.
    pub collapse: Vec<String>,
    /// List symlinks (not followed) as file rows of their own size,
    /// tagged [`LINK`]. The explorer sets it; other listings do not.
    pub list_links: bool,
    /// `--min-size`: the floor the listing applies to the rows after the
    /// scan, compared with the sort key (disk use or apparent size). A
    /// subdirectory below it never takes its parent's row, since the
    /// floor would drop it and leave neither.
    pub min_size: Option<u64>,
}

/// The tag of a symlink row listed under [`ScanOpts::list_links`].
pub const LINK: &str = "symlink";

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
            collapse: Vec::new(),
            list_links: false,
            min_size: None,
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
    /// Entries below a directory that the filters left out (0 without
    /// a filter). Trashing the directory takes them too.
    pub filtered_out: u64,
}

impl Item {
    pub fn key(&self, sort: SortKey) -> u64 {
        sort_key(sort, self.apparent, self.disk, self.items, self.mtime)
    }
}

/// gdu #289 and 34(h): the one order of every du ranking, largest key
/// first and ties by name, so the table, the tree, the explorer, the
/// type list, du diff and the dupe groups agree on which row comes first.
/// The keys and names are whatever a list ranks by. Precheck
/// (`check_rank_order`) refuses a comparator anywhere else in du that
/// puts a larger value first.
pub fn rank<K: Ord, N: Ord + ?Sized>(a: (K, &N), b: (K, &N)) -> std::cmp::Ordering {
    b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1))
}

/// [`rank`] of two rows by `sort`, ties by path.
pub fn rank_items(a: &Item, b: &Item, sort: SortKey) -> std::cmp::Ordering {
    rank((a.key(sort), &a.path), (b.key(sort), &b.path))
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
            // Turned away unless it ranks ahead of the floor.
            if rank((k, &item.path), (*fk, fp)).is_ge() {
                return false;
            }
        }
        if self.cap <= SORTED_INSERT_CAP {
            // Small lists stay sorted, so the floor is known as soon as
            // the list fills and workers prune from the start.
            let sort = self.sort;
            let pos = self
                .items
                .partition_point(|x| rank_items(x, &item, sort).is_lt());
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
        self.items.sort_unstable_by(|a, b| rank_items(a, b, sort));
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
    /// Read-only btrfs subvolumes (snapshots) met below a PATH, sorted.
    /// They count in full, as with du(1), unless `--shared` is on.
    pub snapshots: Vec<PathBuf>,
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

/// The sum of `sizes`, stopping at `u64::MAX` rather than overflowing
/// (ncdu #278: a corrupt or crafted size near the top wraps a plain sum,
/// or panics in a debug build), as coreutils du does. Every sum of sizes
/// in the du code goes through here or `saturating_add`; the precheck
/// refuses a bare `.sum()` or `+=` on one.
pub fn size_sum(sizes: impl IntoIterator<Item = u64>) -> u64 {
    sizes.into_iter().fold(0, u64::saturating_add)
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
    rows.sort_by(|a, b| rank((key(a), &a.ext), (key(b), &b.ext)));
    rows
}

/// What the shared extents of a `--unique` scan add up to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SharedTotals {
    /// Space they take, each extent counted once.
    pub distinct: u64,
    /// Bytes the files reference, with repeats.
    pub referenced: u64,
    /// Bytes the mapped files have in compressed (encoded) extents, at
    /// their uncompressed size: the kernel gives no other figure.
    pub encoded: u64,
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

/// The devices of the scan roots of `report`.
#[cfg(unix)]
fn scanned_devices(report: &Report) -> HashSet<u64> {
    report
        .roots
        .iter()
        .filter_map(|r| fs::symlink_metadata(&r.path).ok().map(|m| m.dev()))
        .collect()
}

/// 33(h) `--open-deleted`: what deleted files still open hold on the
/// devices of the scan roots. None where the system cannot say.
#[cfg(unix)]
fn open_deleted_on(report: &Report) -> Option<crate::holders::OpenDeleted> {
    crate::holders::open_deleted(&scanned_devices(report))
}

#[cfg(windows)]
fn open_deleted_on(_report: &Report) -> Option<crate::holders::OpenDeleted> {
    None
}

/// The `--open-deleted` line under the trash line. The space is not in
/// any total: no name is left to count it under.
fn open_deleted_line(h: &crate::holders::OpenDeleted) -> String {
    let partial = if h.unreadable > 0 {
        format!(
            "; {} not readable, so this is a lower bound",
            plural(h.unreadable, "process", "processes")
        )
    } else {
        String::new()
    };
    format!(
        "Held by deleted files still open on these file systems: {} on disk, {} apparent, \
         in {} (not in the totals{partial})",
        format_bytes(h.disk),
        format_bytes(h.apparent),
        plural(h.files, "file", "files")
    )
}

/// The `open_deleted` key of `--json` with `--open-deleted`; no key
/// without it, so other reports keep their shape.
fn open_deleted_json(h: Option<&crate::holders::OpenDeleted>) -> String {
    h.map(|h| {
        format!(
            ",\"open_deleted\":{{\"files\":{},\"disk\":{},\"apparent\":{},\
             \"unreadable_processes\":{}}}",
            h.files, h.disk, h.apparent, h.unreadable
        )
    })
    .unwrap_or_default()
}

/// Summarize the trash dirs that live on the same device as any scan root.
#[cfg(unix)]
pub fn trash_on_scanned_devices(dirs: &[TrashDir], report: &Report) -> TrashSummary {
    let devs = scanned_devices(report);
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

/// Summarize the Recycle Bin of each drive that holds a scan root; under
/// the tests' bin seam, the seam folder instead.
#[cfg(windows)]
pub fn recycle_bin_on_scanned_drives(report: &Report) -> TrashSummary {
    if let Some(dir) = crate::win_recycle::test_bin_dir() {
        let (items, _) = crate::win_recycle::list_items_in(std::slice::from_ref(&dir));
        return TrashSummary {
            items: items.len() as u64,
            bytes: size_sum(
                items
                    .iter()
                    .map(|it| crate::win_recycle::recycled_size(Path::new(&it.id))),
            ),
            roots: vec![dir],
            cap: None,
        };
    }
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
            t.items = t.items.saturating_add(items);
            t.bytes = t.bytes.saturating_add(bytes);
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

/// Builds the hasher of a walk's `(dev, ino)` sets and maps ([`LinkSet`],
/// [`LinkClaims`]). Each directory and each file of several links costs
/// one lookup there, and std's SipHash made those lookups a large part of
/// a walk of deep trees. The keys are file IDs, not names, so two folded
/// multiplies a key do. The seed comes from std's random keys, one per
/// set, so the buckets a tree's IDs fall in change from run to run.
#[derive(Clone)]
struct InoHash(u64);

impl Default for InoHash {
    fn default() -> Self {
        use std::hash::BuildHasher;
        InoHash(std::collections::hash_map::RandomState::new().hash_one(0u64))
    }
}

impl std::hash::BuildHasher for InoHash {
    type Hasher = InoHasher;

    fn build_hasher(&self) -> InoHasher {
        InoHasher(self.0)
    }
}

/// [`InoHash`]'s hasher: each `u64` written is folded into the state by
/// a 128-bit multiply, which mixes the low and high bits alike.
struct InoHasher(u64);

impl std::hash::Hasher for InoHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(u64::from(b));
        }
    }

    fn write_u64(&mut self, n: u64) {
        let p = u128::from(self.0 ^ n) * 0x9E37_79B9_7F4A_7C15_u128;
        self.0 = (p as u64) ^ ((p >> 64) as u64);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

type InoSet = HashSet<(u64, u64), InoHash>;
type InoMap<V> = HashMap<(u64, u64), V, InoHash>;

/// Hard links seen so far, as `(dev, ino)`, split over several locks so
/// workers on a tree full of links (pnpm, nix, ccache) rarely wait on
/// each other.
struct LinkSet {
    shards: Vec<Mutex<InoSet>>,
}

impl LinkSet {
    /// Whether `st` is the first sighting of its file. See
    /// [`Ctx::first_link`] for what link count 0 and file ID 0 mean.
    fn first_link(&self, st: &du_fs::Stat) -> bool {
        st.nlink == 1 || st.ino == 0 || self.insert(st.dev, st.ino)
    }

    const SHARDS: usize = 16;

    /// The shard of file `(dev, ino)`, for this set and [`LinkClaims`].
    fn shard(dev: u64, ino: u64) -> usize {
        // Fibonacci hashing: inode numbers often share their low bits.
        let h = (ino ^ dev.rotate_left(32)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        (h >> 60) as usize % Self::SHARDS
    }

    fn new() -> Self {
        LinkSet {
            shards: (0..Self::SHARDS).map(|_| Mutex::default()).collect(),
        }
    }

    /// Record the file; returns whether it was new.
    fn insert(&self, dev: u64, ino: u64) -> bool {
        self.shards[Self::shard(dev, ino)]
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((dev, ino))
    }

    /// [`LinkSet::first_link`] for each of `sts`, files of several links
    /// that have a file ID: each shard is locked once for the lot rather
    /// than once a file, so workers in one huge directory of links do not
    /// queue on the shard locks.
    fn first_links(&self, sts: &[du_fs::Stat]) -> Vec<bool> {
        let mut first = vec![false; sts.len()];
        for run in shard_runs(sts).chunk_by(|a, b| a.0 == b.0) {
            let mut set = self.shards[run[0].0]
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for &(_, k) in run {
                first[k] = set.insert((sts[k].dev, sts[k].ino));
            }
        }
        first
    }
}

/// `(shard, k)` for each file `sts[k]`, by shard: the runs of one shard
/// for a batch of [`LinkSet::first_links`] or [`LinkClaims::claim_all`].
/// The pairs come out sorted, by shard and then by `k`.
///
/// A batch of 8 files or more is placed by counting: there are only
/// [`LinkSet::SHARDS`] shards, and a batch from one huge directory of
/// links holds tens of thousands of files, where a comparison sort makes
/// about `n log n` comparisons. A smaller batch is sorted, because the
/// count and prefix over every shard cost more than a few comparisons.
/// The vector is filled with `(SHARDS, 0)`, a pair no file gets, so its
/// allocation takes malloc's fast path; a zeroed fill would call calloc,
/// which costs a small batch more than the zeroing itself.
///
/// This was already a function of its own, not inlined into its callers;
/// `#[inline(never)]` keeps it so, whatever those callers become.
#[inline(never)]
fn shard_runs(sts: &[du_fs::Stat]) -> Vec<(usize, usize)> {
    if sts.len() < 8 {
        let mut order: Vec<(usize, usize)> = sts
            .iter()
            .enumerate()
            .map(|(k, st)| (LinkSet::shard(st.dev, st.ino), k))
            .collect();
        order.sort_unstable();
        return order;
    }
    let mut next = [0usize; LinkSet::SHARDS];
    for st in sts {
        next[LinkSet::shard(st.dev, st.ino)] += 1;
    }
    let mut start = 0;
    for slot in &mut next {
        let count = *slot;
        *slot = start;
        start += count;
    }
    let mut order = vec![(LinkSet::SHARDS, 0); sts.len()];
    for (k, st) in sts.iter().enumerate() {
        let shard = LinkSet::shard(st.dev, st.ino);
        order[next[shard]] = (shard, k);
        next[shard] += 1;
    }
    order
}

/// Whether a hard-linked file met at paths `a` and `b` counts at `a`
/// rather than `b` (gdu #439): the first of its paths in `Path` order,
/// name by name. A scan ([`Ctx::counts_here`]) and every reader of a saved
/// tree ([`LinkCredit`], for ncdu exports) pick by this one rule, so a
/// tree read back has the totals of the scan that wrote it.
pub(crate) fn credits_first(a: &Path, b: &Path) -> bool {
    a < b
}

/// `full == dir.join(name)` for `name`, one entry of a listing (never
/// empty, `.`, `..` or with a separator), without allocating. `Path`
/// equality compares components, and `dir.join(name)` has the components
/// of `dir` and then `name`, so this asks the same: `full` ends in `name`
/// and the rest of it equals `dir`. A trailing separator or a `.` inside
/// `dir` therefore changes neither answer, and names compare as bytes,
/// UTF-8 or not.
fn same_joined(full: &Path, dir: &Path, name: &CStr) -> bool {
    full.file_name().is_some_and(|n| n == du_fs::os(name)) && full.parent() == Some(dir)
}

/// `credits_first(&dir.join(name), other)` for `name`, one entry of a
/// listing, without building the path. `Path` order compares components
/// one by one, and `dir.join(name)` has the components of `dir` and then
/// `name`, so this compares those against `other`'s.
fn joined_first(dir: &Path, name: &CStr, other: &Path) -> bool {
    let joined = dir
        .components()
        .chain(std::iter::once(std::path::Component::Normal(du_fs::os(
            name,
        ))));
    let first = joined.lt(other.components());
    debug_assert_eq!(
        first,
        credits_first(&dir.join(du_fs::os(name)), other),
        "joined_first({dir:?}, {name:?}, {other:?})"
    );
    first
}

/// How many leading bytes `a` and `b` share. Eight bytes are compared at
/// a time, so a long shared start costs a few steps.
#[cfg(unix)]
fn shared_len(a: &[u8], b: &[u8]) -> usize {
    let mut i = 0;
    for (x, y) in a.as_chunks::<8>().0.iter().zip(b.as_chunks::<8>().0) {
        let (x, y) = (u64::from_le_bytes(*x), u64::from_le_bytes(*y));
        if x != y {
            // In a little-endian load byte i is the lowest, so the lowest
            // set bit of the difference is in the first byte that differs.
            return i + (x ^ y).trailing_zeros() as usize / 8;
        }
        i += 8;
    }
    let n = a.len().min(b.len());
    while i < n && a[i] == b[i] {
        i += 1;
    }
    i
}

/// Where byte `b` of a path sorts when paths compare by component: an end
/// before a separator, a separator before any other byte, and the rest as
/// bytes. A component that ends first is a prefix of the other's, and
/// `Path` orders it first, as this does.
#[cfg(unix)]
fn path_rank(b: Option<&u8>) -> u16 {
    match b {
        None => 0,
        Some(&b'/') => 1,
        Some(&c) => u16::from(c) + 2,
    }
}

/// [`joined_first`] for the claims' paths, read as bytes. Those paths
/// are below the root ([`Ctx::below_root`]) and were built a name at a
/// time, so each separator is one byte between two names, with no `.`
/// and no separator at the end (`/` alone excepted). For such paths the
/// order by component is the order of the bytes with a separator below
/// every other byte, and that needs no parse of either path.
#[cfg(unix)]
fn plain_joined_first(dir: &Path, name: &CStr, other: &Path) -> bool {
    let d = dir.as_os_str().as_encoded_bytes();
    let o = other.as_os_str().as_encoded_bytes();
    let n = name.to_bytes();
    let first = 'order: {
        let i = shared_len(d, o);
        if i < d.len() {
            break 'order path_rank(d.get(i)) < path_rank(o.get(i));
        }
        // `dir` starts `other`. The joined path goes on with a separator,
        // unless `dir` is empty or ends in one, and then the name.
        let mut rest = &o[d.len()..];
        if !d.is_empty() && d.last() != Some(&b'/') {
            match rest.split_first() {
                Some((&b'/', after)) => rest = after,
                next => break 'order path_rank(Some(&b'/')) < path_rank(next.map(|(c, _)| c)),
            }
        }
        let j = shared_len(n, rest);
        path_rank(n.get(j)) < path_rank(rest.get(j))
    };
    debug_assert_eq!(
        first,
        joined_first(dir, name, other),
        "plain_joined_first({dir:?}, {name:?}, {other:?})"
    );
    first
}

/// The hard-linked files of a saved tree, read one entry at a time: for
/// each, the path it counts at by [`credits_first`] and what it adds
/// there. `du_ncdu::Builder` adds those once the whole tree is read.
pub(crate) struct LinkCredit<T> {
    files: HashMap<(u64, u64), (PathBuf, T)>,
    /// Further names of files already met.
    pub(crate) extra: u64,
}

impl<T> LinkCredit<T> {
    pub(crate) fn new() -> Self {
        LinkCredit {
            files: HashMap::new(),
            extra: 0,
        }
    }

    /// Note file `id` met at `at`, adding `what` there: it replaces the
    /// earlier sighting when `at` comes first.
    pub(crate) fn see(&mut self, id: (u64, u64), at: PathBuf, what: T) {
        match self.files.entry(id) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert((at, what));
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                self.extra += 1;
                if credits_first(&at, &o.get().0) {
                    o.insert((at, what));
                }
            }
        }
    }

    /// What each file adds at the path it counts at.
    pub(crate) fn into_credits(self) -> impl Iterator<Item = T> {
        self.files.into_values().map(|(_, what)| what)
    }
}

/// Where each file with several hard links was met (gdu #439): the
/// smallest of its paths, and whether its count fell there. The first
/// sighting to reach the claims takes the count, and the walk threads
/// meet links in any order, so that sighting moves between runs. Once a
/// walk ends, [`LinkClaims::settled`] says whether every count fell on
/// the smallest path; when one did not, the walk runs again with that
/// path fixed for each file ([`Ctx::decided`]).
struct LinkClaims {
    /// The device whose files [`ClaimShard::here`] keys by inode alone.
    dev: u64,
    shards: Vec<Mutex<ClaimShard>>,
}

/// One shard of [`LinkClaims`] and [`Decided`]. Nearly every linked file
/// of a walk is on the root device, so its claim is keyed by inode number
/// alone, in `here`; a file on any other device keeps the `(dev, ino)`
/// key, in `other`. A file is looked up in one map only, picked by its
/// device, so it still has one claim. The 8-byte key makes each bucket of
/// a walk's largest table 64 bytes, not 72: 2 MiB less on 200,000 linked
/// files, and one `u64` hashed and compared a lookup, not two. `other`
/// is boxed and made at its first file: most walks have none, and every
/// root builds its shards even when it keeps no rows, where a second
/// inline table would take the shard vec past 1,032 bytes, the largest
/// block glibc's per-thread cache holds.
#[derive(Default)]
struct ClaimShard {
    here: HashMap<u64, Claim, InoHash>,
    other: Option<Box<InoMap<Claim>>>,
}

impl ClaimShard {
    /// The claim of file `(dev, ino)`, with `here` the device keyed by
    /// inode alone.
    fn get(&self, here: u64, (dev, ino): (u64, u64)) -> Option<&Claim> {
        if dev == here {
            self.here.get(&ino)
        } else {
            self.other.as_ref()?.get(&(dev, ino))
        }
    }

    fn len(&self) -> usize {
        self.here.len() + self.other.as_ref().map_or(0, |m| m.len())
    }

    fn values(&self) -> impl Iterator<Item = &Claim> {
        self.here
            .values()
            .chain(self.other.iter().flat_map(|m| m.values()))
    }
}

/// One linked file in [`LinkClaims`], kept small: a walk of 200,000
/// linked files keeps one each. The path is built to its length
/// ([`joined_path`]) from below the root ([`Ctx::below_root`]), and the
/// stat leaves out the device and inode number, which are the file's key.
struct Claim {
    smallest: Box<Path>,
    /// Whether the sighting that took the count was at `smallest`.
    counted_there: bool,
    /// The rest of the file's stat ([`Claim::stat`]).
    mode: u32,
    size: u64,
    blocks: u64,
    nlink: u64,
    mtime: i64,
}

impl Claim {
    fn new(st: &du_fs::Stat, smallest: Box<Path>) -> Self {
        Claim {
            smallest,
            counted_there: true,
            mode: st.mode,
            size: st.size,
            blocks: st.blocks,
            nlink: st.nlink,
            mtime: st.mtime,
        }
    }

    /// The stat of the file `(dev, ino)`, the key it is filed under. A
    /// second walk takes it for an entry whose listed inode number names
    /// this file ([`du_fs::Dir::lists_true_inodes`]), so it stats no
    /// linked file again.
    fn stat(&self, (dev, ino): (u64, u64)) -> du_fs::Stat {
        du_fs::Stat {
            mode: self.mode,
            size: self.size,
            blocks: self.blocks,
            nlink: self.nlink,
            ino,
            dev,
            mtime: self.mtime,
        }
    }
}

/// How many bytes of each path below `root` are the root's: the root
/// and the separator `push` puts after it, unless it ends in one.
fn root_skip(root: &Path) -> usize {
    let b = root.as_os_str().as_encoded_bytes();
    let sep = b
        .last()
        .is_some_and(|&c| !std::path::is_separator(char::from(c)));
    b.len() + usize::from(sep)
}

/// `dir.join(name)` with no room to spare. `join` copies `dir` and then
/// grows the copy for `name`, which doubles it: a 45-byte path took a
/// 96-byte block of the allocator, where one of 64 holds it. Used for
/// the claims and for the path of each subdirectory the walk enters.
fn joined_path(dir: &Path, name: &CStr) -> Box<Path> {
    let name = du_fs::os(name);
    let mut p = PathBuf::with_capacity(dir.as_os_str().len() + 1 + name.len());
    p.push(dir);
    p.push(name);
    p.into_boxed_path()
}

/// Where a second walk counts each linked file ([`Ctx::decided`]): the
/// first walk's claims, left in the shards they were filed in, so
/// settling them copies nothing.
struct Decided {
    /// [`LinkClaims::dev`].
    dev: u64,
    shards: Vec<ClaimShard>,
}

impl Decided {
    fn get(&self, id: &(u64, u64)) -> Option<&Claim> {
        self.shards[LinkSet::shard(id.0, id.1)].get(self.dev, *id)
    }

    fn len(&self) -> usize {
        self.shards.iter().map(|m| m.len()).sum()
    }
}

impl LinkClaims {
    /// Claims that key the files of device `dev`, the root's, by inode
    /// number alone.
    fn new(dev: u64) -> Self {
        LinkClaims {
            dev,
            shards: (0..LinkSet::SHARDS).map(|_| Mutex::default()).collect(),
        }
    }

    /// Note a sighting of file `st` in `dir` as `name`, and return
    /// whether it takes the file's count: the first sighting to reach the
    /// claims does. Each path is met once.
    fn claim(&self, st: &du_fs::Stat, dir: &Path, name: &CStr) -> bool {
        let mut map = self.shards[LinkSet::shard(st.dev, st.ino)]
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        Self::see_in(&mut map, self.dev, st, dir, name)
    }

    /// [`LinkClaims::claim`] for each of `sts`, met in `dir` as `name(k)`:
    /// each shard is locked once for the lot.
    fn claim_all<'n>(
        &self,
        sts: &[du_fs::Stat],
        dir: &Path,
        name: impl Fn(usize) -> &'n CStr,
    ) -> Vec<bool> {
        let mut first = vec![false; sts.len()];
        for run in shard_runs(sts).chunk_by(|a, b| a.0 == b.0) {
            let mut map = self.shards[run[0].0]
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for &(_, k) in run {
                first[k] = Self::see_in(&mut map, self.dev, &sts[k], dir, name(k));
            }
        }
        first
    }

    /// [`LinkClaims::claim`] in the locked shard `map` of file `st`. A
    /// later sighting takes the path only when it comes first
    /// ([`credits_first`]), and only then is that path built. On Unix the
    /// paths compare as bytes, in `plain_joined_first`. A file on device
    /// `here` is filed by inode alone ([`ClaimShard`]).
    fn see_in(map: &mut ClaimShard, here: u64, st: &du_fs::Stat, dir: &Path, name: &CStr) -> bool {
        use std::collections::hash_map::Entry;
        let claim = if st.dev == here {
            match map.here.entry(st.ino) {
                Entry::Vacant(v) => {
                    v.insert(Claim::new(st, joined_path(dir, name)));
                    return true;
                }
                Entry::Occupied(o) => o.into_mut(),
            }
        } else {
            let other = map.other.get_or_insert_with(Box::default);
            match other.entry((st.dev, st.ino)) {
                Entry::Vacant(v) => {
                    v.insert(Claim::new(st, joined_path(dir, name)));
                    return true;
                }
                Entry::Occupied(o) => o.into_mut(),
            }
        };
        #[cfg(unix)]
        let first = plain_joined_first(dir, name, &claim.smallest);
        #[cfg(not(unix))]
        let first = joined_first(dir, name, &claim.smallest);
        if first {
            claim.smallest = joined_path(dir, name);
            claim.counted_there = false;
        }
        false
    }

    /// `None` when each linked file counted at its smallest path, else the
    /// smallest path and stat of every linked file, for a walk that counts
    /// there.
    fn settled(mut self) -> Option<Decided> {
        // Most walks: no claims, or each counted at its smallest path.
        // They return here and build no Decided.
        if self
            .shards
            .iter_mut()
            .map(|m| m.get_mut().unwrap_or_else(|e| e.into_inner()))
            .all(|m| m.values().all(|c| c.counted_there))
        {
            return None;
        }
        let shards = self
            .shards
            .into_iter()
            .map(|m| m.into_inner().unwrap_or_else(|e| e.into_inner()))
            .collect();
        Some(Decided {
            dev: self.dev,
            shards,
        })
    }
}

struct Ctx<'a> {
    opts: &'a ScanOpts,
    /// Git probes to start as repositories turn up.
    prefetch: Option<&'a du_git::Prefetch>,
    root_dev: u64,
    /// Multiply-linked files already counted.
    seen: LinkSet,
    /// Where this walk met each file with several links.
    claims: LinkClaims,
    /// The bytes that start every path below the root ([`root_skip`]).
    root_skip: usize,
    /// For a second walk: the one path at which each linked file counts.
    decided: Option<&'a Decided>,
    /// On a second walk, the files met at their path in `decided`, added
    /// once a stat job ([`stat_entries`]).
    met: AtomicU64,
    /// Directory descriptors held open as parents.
    fds: FdBudget,
    /// Workers in the pool, to size the stat jobs of a wide directory.
    threads: usize,
    /// A second walk may take a linked file's stat by the inode number
    /// its listing gives ([`listed_inodes_usable`]).
    ino_reuse: bool,
    /// Whether a long directory's stats start while the rest of it is
    /// read ([`list_and_stat`]): decided once a walk, so a walk that does
    /// not stream lists as it always did. Linux only, where each read of a
    /// listing goes on from the last; on more than one thread, since one
    /// has nothing to overlap; only on a first walk, since a second one
    /// reads inode numbers; and not under filters, which read the whole
    /// listing first (.gitignore, CACHEDIR.TAG).
    streams: bool,
    /// Whether the scan reads anything below a PATH: rows, or file types
    /// by name. When it does not (`-s`, `-n 0`), a linked file may count
    /// at any of its paths, since a PATH's total is the same either way,
    /// so no claims are kept and no second walk runs.
    rows: bool,
    /// Directories entered so far, so one met again is walked once: a
    /// bind mount on the same device on Unix, and with `-L` a symlink
    /// loop or a second link to a directory.
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
    /// Read-only btrfs subvolumes entered below the root.
    snapshots: Mutex<Vec<PathBuf>>,
}

/// Unreadable entries a scan remembers by name; the count covers the rest.
pub(crate) const UNREAD_KEPT: usize = 100;
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
            encoded: 0,
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
        if self.opts.unique && (!s.shared.is_empty() || s.encoded > 0) {
            if let Ok(mut set) = self.shared.lock() {
                set.add(dev, &s.shared);
                set.encoded = set.encoded.saturating_add(s.encoded);
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

    /// [`Ctx::first_link`] for the file `st` met in `dir` as `name`, with
    /// the count of a file of several links held to one path: on a second
    /// walk the one [`Ctx::decided`] names, else the first sighting to
    /// reach [`Ctx::claims`]. A file whose link count is unknown (0) counts at
    /// its first sighting, and so does every file when nothing below a
    /// PATH is read ([`Ctx::rows`]). Every count of a file in the walk goes
    /// through here or [`Ctx::counts_here_all`] (precheck check_link_credit).
    /// A file met at its [`Ctx::decided`] path adds one to `met`.
    fn counts_here(&self, st: &du_fs::Stat, dir: &Path, name: &CStr, met: &mut u64) -> bool {
        // -L: a file reached through links counts once, at its first
        // sighting; any file may be reached twice, and a path for each
        // would cost too much.
        if self.opts.follow {
            return st.ino == 0 || self.seen.insert(st.dev, st.ino);
        }
        if st.nlink <= 1 || st.ino == 0 || !self.rows {
            return self.first_link(st);
        }
        // The path compare looks at a path the first walk stored, or
        // builds one for a claim. Wide `-s` returns above (`rows` is
        // false) and must not carry this body in its stat loop.
        self.counts_at_path(st, dir, name, met)
    }

    /// `dir`, a directory of this walk, as a path from below the root.
    /// The claims keep their paths this way, so that each linked file
    /// does not keep its own copy of the root's path. Every path of one
    /// root has the same start, so the order and equality of the paths
    /// that follow it are those of the whole paths.
    fn below_root<'p>(&self, dir: &'p Path) -> &'p Path {
        let b = dir.as_os_str().as_encoded_bytes();
        let rest = b.get(self.root_skip..).unwrap_or_default();
        debug_assert!(
            rest.is_empty() || std::path::is_separator(char::from(b[self.root_skip - 1])),
            "below_root({dir:?})"
        );
        // SAFETY: a path of the walk is the root's path, a separator and
        // the rest, so `rest` starts right after an ASCII separator or is
        // empty, where an encoded OsStr may be split.
        Path::new(unsafe { OsStr::from_encoded_bytes_unchecked(rest) })
    }

    /// Where a linked file counts. Not inlined into [`stat_entries`]: the
    /// copy without reuse is the plain loop, and a `PathBuf` per name on
    /// the reuse walk was the main-arena pile-up.
    #[inline(never)]
    fn counts_at_path(&self, st: &du_fs::Stat, dir: &Path, name: &CStr, met: &mut u64) -> bool {
        let dir = self.below_root(dir);
        // A second walk already stored the path. Comparing the components
        // builds no PathBuf for any linked name, met at its path or not.
        if let Some(d) = self.decided {
            if let Some(fixed) = d.get(&(st.dev, st.ino)) {
                let here = same_joined(&fixed.smallest, dir, name);
                debug_assert_eq!(
                    here,
                    *fixed.smallest == *dir.join(du_fs::os(name)),
                    "same_joined({:?}, {dir:?}, {name:?})",
                    fixed.smallest
                );
                *met += u64::from(here);
                return here;
            }
        }
        // A second walk meets a file the first did not (it gained a link
        // since): it counts at its first sighting.
        if self.decided.is_some() {
            return self.first_link(st);
        }
        // The first walk: one lock and one lookup decide the count and
        // note the path, which is built only when it is kept.
        self.claims.claim(st, dir, name)
    }

    /// Whether `st`'s count may wait for [`Ctx::counts_here_all`] with
    /// the other linked files of its stat job: a file of several links
    /// and a file ID, on a walk that neither follows links nor has
    /// [`Ctx::decided`] paths.
    fn defers(&self, st: &du_fs::Stat) -> bool {
        !self.opts.follow && self.decided.is_none() && st.nlink > 1 && st.ino != 0
    }

    /// [`Ctx::counts_here`] for files `sts` that all [`Ctx::defers`], met
    /// in `dir` as `name(k)`, with each lock taken once for the lot. The
    /// claims do not depend on the order of the sightings, so a batch
    /// counts as the files one by one would.
    fn counts_here_all<'n>(
        &self,
        sts: &[du_fs::Stat],
        dir: &Path,
        name: impl Fn(usize) -> &'n CStr,
    ) -> Vec<bool> {
        if self.rows {
            self.claims.claim_all(sts, self.below_root(dir), name)
        } else {
            self.seen.first_links(sts)
        }
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
    /// as merged spans; `disk_all` counts every reference. `disk` is then
    /// `base` plus what the spans cover.
    base: u64,
    ranges: Vec<crate::du_extent::Span>,
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
    /// Entries below this directory that the filters left out.
    filtered_out: u64,
    /// The filters left this directory out whole (a tagged cache): it
    /// adds nothing to its parent but one entry left out.
    left_out: bool,
}

/// At most this many repo roots are remembered per directory.
const MAX_REPOS: usize = 64;

/// Descriptors kept back from the parents the walk holds open
/// ([`FdBudget`]): standard streams, the git probes, files read for
/// `--unique`, and the directory each walk thread is reading.
const FD_RESERVE: usize = 64;

/// The most directory descriptors the walk holds open as parents.
const FD_HOLD_MAX: usize = 1 << 16;

/// Directories held open while their subdirectories are read, so those
/// open relative to them, counted against a cap well under the soft
/// `RLIMIT_NOFILE`. Once the cap is reached a directory closes before its
/// children open, and they open by full path (each open then walks the
/// path again), unless the path is too long for that
/// ([`children_open_by_path`]). A fixed depth of 64 used to decide this:
/// on a chain 1500 levels deep each open below it re-read every level.
struct FdBudget {
    held: AtomicUsize,
    cap: usize,
}

impl FdBudget {
    /// A budget of half the open files the soft limit leaves after
    /// [`FD_RESERVE`] and four per walk thread, at most [`FD_HOLD_MAX`].
    /// Windows opens every directory by path, so it holds none.
    fn new(threads: usize) -> Self {
        let cap = if cfg!(windows) {
            0
        } else {
            let limit = open_files_cap().unwrap_or(usize::MAX);
            (limit.saturating_sub(FD_RESERVE + 4 * threads) / 2).min(FD_HOLD_MAX)
        };
        FdBudget {
            held: AtomicUsize::new(0),
            cap,
        }
    }

    /// One more directory held open, if the budget has room; it is given
    /// back when the hold drops.
    fn take(&self) -> Option<FdHold<'_>> {
        let mut n = self.held.load(Ordering::Relaxed);
        loop {
            if n >= self.cap {
                return None;
            }
            match self
                .held
                .compare_exchange_weak(n, n + 1, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return Some(FdHold(&self.held)),
                Err(now) => n = now,
            }
        }
    }

    /// One more directory held open past the cap: one whose children's
    /// paths are too long to open by. It still counts, so other holds
    /// wait for it.
    fn force(&self) -> FdHold<'_> {
        self.held.fetch_add(1, Ordering::Relaxed);
        FdHold(&self.held)
    }
}

/// A directory descriptor counted in an [`FdBudget`].
struct FdHold<'a>(&'a AtomicUsize);

impl Drop for FdHold<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Whether a second walk below `root` may take a linked file's stat by
/// the inode number its listing gives. A regular file bind-mounted over
/// another lists the hidden file's number, so a mount point below `root`
/// that is not a directory turns it off for the scan, and so does a mount
/// table or a mount point that cannot be read. Linux only: no other
/// listing gives the numbers.
fn listed_inodes_usable(root: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        let (Ok(table), Ok(root)) = (crate::trashdir::read_mount_table(), fs::canonicalize(root))
        else {
            return false;
        };
        no_file_mounts_below(&table, &root, |mp| {
            fs::metadata(mp).ok().map(|m| m.is_dir())
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = root;
        false
    }
}

/// Whether every mount point of mount table `table` below `root` (not
/// `root` itself) is a directory. `is_dir` is None for one that cannot
/// be stat-ed, which counts as not.
#[cfg(target_os = "linux")]
fn no_file_mounts_below(table: &str, root: &Path, is_dir: impl Fn(&Path) -> Option<bool>) -> bool {
    crate::trashdir::mount_table(table)
        .iter()
        .filter(|(mp, _)| mp.starts_with(root) && mp != root)
        .all(|(mp, _)| is_dir(mp) == Some(true))
}

/// The soft cap on this process's open files, if there is one.
fn open_files_cap() -> Option<usize> {
    #[cfg(unix)]
    {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit only fills the struct handed to it.
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } == 0
            && lim.rlim_cur != libc::RLIM_INFINITY
        {
            return usize::try_from(lim.rlim_cur).ok();
        }
    }
    None
}

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

/// The depth from which a directory with one subdirectory frees its list
/// of subdirectories before entering it ([`walk_dir`]). On a long chain
/// each level would keep its list the whole way down, a 304-byte block a
/// level. A shallower one keeps its list as before: freeing it early on
/// the kernel tree (548 such directories, none deeper than 13) changed
/// which blocks the allocator handed out later and cost 1.1 percent more
/// instructions (bench2/r304).
const CHAIN_FREE_DEPTH: usize = 64;

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
    link: bool,
}

/// Open and walk one directory. `parent` and `name` locate it relative to
/// an open parent; without them it opens by `path`. `known` is its stat
/// when the caller already has one. Returns `None` for a directory on
/// another file system under `-x`, or one that cannot be stat-ed.
fn enter(
    ctx: &Ctx<'_>,
    parent: Option<(&du_fs::Dir, &CStr)>,
    buf: &mut PathBuf,
    known: Option<du_fs::Stat>,
    depth: usize,
    listing: bool,
    chain: Option<&Arc<Chain>>,
) -> Option<Node> {
    let path: &Path = buf;
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
            filtered_out: 0,
            left_out: false,
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
            // It gets a row of its own, its own blocks only, so the table
            // and the tree can mark it, as ncdu lists it with `!`.
            let node = shut(st);
            node.offer_self(ctx, path, depth, 0, None);
            return Some(node);
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
    // Each btrfs subvolume has a device of its own and its top is inode
    // 256, so the ioctl runs on those alone. A read-only one (a snapshot)
    // counts in full, as with du(1), and is named after the scan.
    if st.ino == BTRFS_SUBVOL_INO && st.dev != ctx.root_dev && dir.is_readonly_snapshot() {
        let mut s = ctx.snapshots.lock().unwrap_or_else(|e| e.into_inner());
        s.push(path.to_path_buf());
    }
    // A directory met again is walked once. Without -L only a bind mount
    // on the same device shows one twice, with the same inode, and its
    // files then counted twice (ncdu #223); coreutils du counts them
    // once only when given several PATHs. With -L a loop or a second link
    // does it too. Windows file IDs are not checked.
    if (follow || cfg!(unix)) && st.ino != 0 && !ctx.dirs_seen.insert(st.dev, st.ino) {
        return None;
    }
    walk_dir(ctx, dir, buf, &st, depth, listing, chain)
}

/// The inode of the top directory of every btrfs subvolume.
const BTRFS_SUBVOL_INO: u64 = 256;

/// Directories with more entries than this stat them in parallel.
const WIDE_DIR: usize = 256;
/// The fewest entries in a parallel stat job of a wide directory.
const WIDE_CHUNK: usize = 64;
/// Stat jobs a worker gets in a wide directory: some to spare, so a worker
/// left idle can take part of the rest.
const WIDE_JOBS_PER_THREAD: usize = 4;

/// Entries per stat job in a wide directory of `n` entries on `threads`
/// workers. Jobs of a fixed 64 entries made 3,125 of them for 200,000
/// entries, and the workers spent their time handing them round.
fn wide_chunk(n: usize, threads: usize) -> usize {
    n.div_ceil(threads.max(1) * WIDE_JOBS_PER_THREAD)
        .max(WIDE_CHUNK)
}

/// What the stats of some entries of one directory add up to.
struct Stats {
    apparent: u64,
    disk: u64,
    unique: u64,
    /// `--shared`: as in [`Node`], for these entries.
    base: u64,
    ranges: Vec<crate::du_extent::Span>,
    mtime: i64,
    /// Files counted (each hard-linked file once).
    files: u64,
    /// Subdirectories, with their stat when it was taken here (for -x or
    /// an entry of unknown type).
    subdirs: Vec<(usize, Option<du_fs::Stat>)>,
    candidates: Vec<FileCandidate>,
    /// Entries the filters left out.
    filtered_out: u64,
    /// `--types`: these entries' sizes by file type.
    types: TypeMap,
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
    ranges: &mut Vec<crate::du_extent::Span>,
) -> u64 {
    if shared.is_empty() {
        *base = base.saturating_add(raw);
        return raw;
    }
    let referenced = size_sum(shared.iter().map(|e| e.length));
    let own = raw.saturating_sub(referenced);
    *base = base.saturating_add(own);
    let mut mine: Vec<crate::du_extent::Span> = shared.iter().map(|e| e.span(dev)).collect();
    let once = crate::du_extent::coalesce(&mut mine);
    ranges.extend(mine);
    raw.min(own.saturating_add(once))
}

/// Count the linked files of a stat job that waited for one batch
/// ([`Ctx::defers`]), entries `idx` of `dir` with stats `sts`: add each
/// that counts here to `out`, and return how many did not. Not inlined:
/// a tree of single-link files never calls it, and its stat loop then
/// carries none of the claims code.
#[inline(never)]
fn count_deferred(
    ctx: &Ctx<'_>,
    out: &mut Stats,
    (dir, entries): (&du_fs::Dir, &du_fs::Entries),
    (idx, sts): (&[usize], &[du_fs::Stat]),
    path: &Path,
    (depth, list_files): (usize, bool),
) -> u64 {
    let name = |k: usize| entries.get(idx[k]).0;
    let first = ctx.counts_here_all(sts, path, name);
    let mut links = 0u64;
    for (k, first) in first.into_iter().enumerate() {
        if first {
            out.add_file(ctx, dir, (idx[k], name(k)), &sts[k], (depth, list_files));
        } else {
            links += 1;
        }
    }
    links
}

/// Stat entries `range` of `dir`: sum the files and collect the
/// subdirectories and the files big enough for the list. `REUSE`: a
/// second walk on `dev` that takes the first walk's stats of linked files
/// ([`Dir::entries_with_inos`](du_fs::Dir::entries_with_inos)); without
/// it `dev` is unused.
fn stat_entries<const REUSE: bool>(
    ctx: &Ctx<'_>,
    (dir, dev): (&du_fs::Dir, u64),
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
        filtered_out: 0,
        types: TypeMap::new(),
    };
    // Linked files whose counts wait for one batch ([`Ctx::defers`]).
    let mut pending: (Vec<usize>, Vec<du_fs::Stat>) = (Vec::new(), Vec::new());
    let mut links = 0u64;
    let mut met = 0u64;
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
            out.filtered_out += 1;
            continue;
        }
        // A directory gets its stat from its own descriptor once open.
        // Under -x it is stat-ed first so a mount point is never opened.
        if kind == du_fs::Kind::Dir && !opts.one_file_system {
            out.subdirs.push((i, None));
            continue;
        }
        // With `REUSE` (a second walk), the first walk's stat of a linked
        // file, found by the inode number the listing gave on device `dev`.
        let listed = if REUSE {
            entries.stat(i).or_else(|| {
                let ino = entries.file_ino(i)?;
                Some(ctx.decided?.get(&(dev, ino))?.stat((dev, ino)))
            })
        } else {
            entries.stat(i)
        };
        let mut m = match listed {
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
            out.filtered_out += 1;
            continue;
        }
        if m.is_dir() {
            if opts.one_file_system && m.dev != ctx.root_dev {
                continue;
            }
            out.subdirs.push((i, Some(m)));
            continue;
        }
        if ctx.defers(&m) {
            pending.0.push(i);
            pending.1.push(m);
            continue;
        }
        if ctx.counts_here(&m, path, name, &mut met) {
            out.add_file(ctx, dir, (i, name), &m, (depth, list_files));
        } else {
            links += 1;
        }
    }
    if !pending.1.is_empty() {
        let job = (&pending.0[..], &pending.1[..]);
        links += count_deferred(
            ctx,
            &mut out,
            (dir, entries),
            job,
            path,
            (depth, list_files),
        );
    }
    // Once a job, not once a file: every worker adds to these.
    ctx.links.fetch_add(links, Ordering::Relaxed);
    ctx.files.fetch_add(out.files, Ordering::Relaxed);
    if met > 0 {
        ctx.met.fetch_add(met, Ordering::Relaxed);
    }
    if let Some(c) = &opts.control {
        c.files.fetch_add(out.files, Ordering::Relaxed);
        c.disk.fetch_add(out.disk, Ordering::Relaxed);
    }
    if !out.types.is_empty() {
        merge_types(
            &mut ctx.types.lock().unwrap_or_else(|e| e.into_inner()),
            std::mem::take(&mut out.types),
        );
    }
    out
}

impl Stats {
    /// Add file `m`, entry `i` named `name` of `dir`, which counts here:
    /// its sizes, its type and, if it is big enough, a place among the
    /// candidates for the list.
    fn add_file(
        &mut self,
        ctx: &Ctx<'_>,
        dir: &du_fs::Dir,
        (i, name): (usize, &CStr),
        m: &du_fs::Stat,
        (depth, list_files): (usize, bool),
    ) {
        let opts = ctx.opts;
        self.files += 1;
        let (a, raw) = m.sizes();
        self.apparent = self.apparent.saturating_add(a);
        self.disk = self.disk.saturating_add(raw);
        let (u, d) = if opts.unique || opts.shared_once {
            let s = ctx.share_of(dir, name, m, raw);
            let d = if opts.shared_once {
                count_once(m.dev, raw, &s.shared, &mut self.base, &mut self.ranges)
            } else {
                raw
            };
            (if opts.unique { s.unique } else { 0 }, d)
        } else {
            (0, raw)
        };
        self.unique = self.unique.saturating_add(u);
        self.mtime = self.mtime.max(m.mtime);
        if opts.types {
            add_type(&mut self.types, name.to_bytes(), a, d);
        }
        let link = opts.list_links && m.is_symlink();
        if list_files
            && (m.is_file() || link)
            && ctx.top_files.wants(ctx.key(a, d, 1, m.mtime))
            && ctx.listable(depth + 1, m.mtime)
        {
            self.candidates.push(FileCandidate {
                entry: i,
                apparent: a,
                disk: d,
                unique: u,
                mtime: m.mtime,
                link,
            });
        }
    }
}

fn merge_types(into: &mut TypeMap, from: TypeMap) {
    for (k, (d, a, n)) in from {
        let e = into.entry(k).or_default();
        e.0 = e.0.saturating_add(d);
        e.1 = e.1.saturating_add(a);
        e.2 += n;
    }
}

/// One directory's stat jobs' [`Stats`], in entry order. A directory of
/// [`WIDE_DIR`] entries or fewer has one job, kept as it is. A wide
/// directory's jobs ([`stat_chunks`]) and a streamed listing's
/// ([`list_and_stat`]) come in a vector. A vector of one cost every small
/// directory an allocation and a free.
enum Parts {
    One(Stats),
    Many(Vec<Stats>),
}

/// Stat every entry of `dir` with [`stat_entries`]: on one thread, or in
/// chunks on the pool when there are many, so a huge flat directory does
/// not serialise the scan. Never inlined: with both copies of the stat loop
/// in one parallel body, round 294's first try listed links 6% slower
/// (more allocations queued on glibc's main arena).
#[inline(never)]
fn stat_chunks<const REUSE: bool>(
    ctx: &Ctx<'_>,
    (dir, dev): (&du_fs::Dir, u64),
    entries: &du_fs::Entries,
    depth: usize,
    list_files: bool,
    (path, rules): (&Path, Option<&Chain>),
) -> Parts {
    let stat_chunk = |range: std::ops::Range<usize>| {
        stat_entries::<REUSE>(
            ctx,
            (dir, dev),
            entries,
            range,
            depth,
            list_files,
            (path, rules),
        )
    };
    let n = entries.len();
    if n > WIDE_DIR {
        let size = wide_chunk(n, ctx.threads);
        Parts::Many(
            (0..n.div_ceil(size))
                .into_par_iter()
                .map(|c| {
                    let mut part = stat_chunk(c * size..n.min((c + 1) * size));
                    if list_files {
                        keep_best(ctx, entries, &mut part.candidates);
                    }
                    part
                })
                .collect(),
        )
    } else {
        Parts::One(stat_chunk(0..n))
    }
}

/// Stat `entries`, or take the stats [`list_and_stat`] `streamed`, add
/// them to `node`, and return the directory's subdirectories, in entry
/// order, and its file candidates, job by job. Within a job the candidates
/// are not in entry order: [`keep_best`] reorders a wide or streamed
/// directory's, and [`count_deferred`] appends the deferred linked files;
/// [`walk_dir`] ranks them. Never inlined, so the [`Stats`] in flight
/// stay in this frame and out of [`enter`]'s, which every level of a deep
/// tree keeps on the stack: round 310's first try handed them back to
/// [`walk_dir`] and grew enter's frame from 984 to 1,352 bytes.
#[inline(never)]
fn add_stats(
    ctx: &Ctx<'_>,
    (dir, reuse): (&du_fs::Dir, Option<u64>),
    entries: &du_fs::Entries,
    (depth, list_files): (usize, bool),
    (path, rules): (&Path, Option<&Chain>),
    streamed: Option<Vec<Stats>>,
    node: &mut Node,
) -> (Vec<(usize, Option<du_fs::Stat>)>, Vec<FileCandidate>) {
    // Two copies of the stat loop: the one without reuse is the plain
    // loop, with no inode lookup in it. Each runs in its own
    // [`stat_chunks`], so each parallel body has one loop.
    let parts = if let Some(parts) = streamed {
        Parts::Many(parts)
    } else {
        match reuse {
            Some(dev) => {
                stat_chunks::<true>(ctx, (dir, dev), entries, depth, list_files, (path, rules))
            }
            None => stat_chunks::<false>(ctx, (dir, 0), entries, depth, list_files, (path, rules)),
        }
    };
    let mut subdirs = Vec::new();
    let mut candidates = Vec::new();
    match parts {
        Parts::One(p) => add_part(node, &mut subdirs, &mut candidates, p),
        Parts::Many(parts) => {
            for p in parts {
                add_part(node, &mut subdirs, &mut candidates, p);
            }
        }
    }
    (subdirs, candidates)
}

/// Add one stat job's [`Stats`] to `node`, `subdirs` and `candidates`.
/// Always inlined: it is [`add_stats`]'s loop body, and as a call of its
/// own (round 310's closure) it cost every directory about 52
/// instructions.
#[inline(always)]
fn add_part(
    node: &mut Node,
    subdirs: &mut Vec<(usize, Option<du_fs::Stat>)>,
    candidates: &mut Vec<FileCandidate>,
    p: Stats,
) {
    node.apparent = node.apparent.saturating_add(p.apparent);
    node.disk = node.disk.saturating_add(p.disk);
    node.disk_all = node.disk_all.saturating_add(p.disk);
    node.base = node.base.saturating_add(p.base);
    take_or_extend(&mut node.ranges, p.ranges);
    node.unique = node.unique.saturating_add(p.unique);
    node.mtime = node.mtime.max(p.mtime);
    node.items += p.files;
    node.filtered_out += p.filtered_out;
    take_or_extend(subdirs, p.subdirs);
    take_or_extend(candidates, p.candidates);
}

/// Keep the `top` best of one stat job's file `candidates`, named in
/// `entries`, and free the room of the rest. [`walk_dir`] keeps the `top`
/// best of the directory's in the same order, by key and then by name,
/// and names are unique in a directory, so the best of all jobs are
/// among the best of each. Done in the job, a wide directory's candidates
/// are cut on every worker: walk_dir used to gather all of them first,
/// one per file of a folder of 500,000 that tie on size, and choose
/// among them on one thread.
#[inline(never)]
fn keep_best(ctx: &Ctx<'_>, entries: &du_fs::Entries, candidates: &mut Vec<FileCandidate>) {
    let top = ctx.opts.top;
    if candidates.len() <= top {
        return;
    }
    let name_of = |c: &FileCandidate| entries.get(c.entry).0.to_bytes();
    candidates.select_nth_unstable_by(top - 1, |x, y| {
        rank(
            (ctx.key(x.apparent, x.disk, 1, x.mtime), name_of(x)),
            (ctx.key(y.apparent, y.disk, 1, y.mtime), name_of(y)),
        )
    });
    candidates.truncate(top);
    candidates.shrink_to_fit();
}

/// Entries a directory's first read must reach for its stats to start
/// while the rest is read ([`list_and_stat`]), and the size of each later
/// read: this many or more, up to the end of the `getdents64` buffer that
/// crossed the mark. Tests use a smaller one, so a test tree of a few
/// thousand entries takes several reads.
const LISTED_BATCH: usize = if cfg!(test) { 512 } else { 16_384 };

/// List `dir` for a walk that [`Ctx::streams`]. A listing that ends in its
/// first read of [`LISTED_BATCH`] entries comes back as it is, for
/// [`walk_dir`] to stat. In a longer one the stats start as the entries
/// come: each read goes to the pool as stat jobs, one per worker, while
/// the next is read, and `parts` gets the jobs' stats in listing order,
/// with their entry numbers counted over the whole listing. A huge
/// directory lists on one thread, and its stats used to wait for the
/// whole listing: a cold one waits on the disk for every block of it.
/// Returns the whole listing and whether reading it stopped on an error.
/// Never inlined: a walk that does not stream must not carry it in
/// [`enter`] (round 298's first try grew it by 721 bytes).
#[inline(never)]
fn list_and_stat(
    ctx: &Ctx<'_>,
    dir: &du_fs::Dir,
    depth: usize,
    listing: bool,
    (path, rules): (&Path, Option<&Chain>),
    parts: &mut Option<Vec<Stats>>,
) -> (du_fs::Entries, bool) {
    let (first, failed) = dir.entries_upto(LISTED_BATCH);
    if failed || first.len() < LISTED_BATCH {
        return (first, failed);
    }
    let opts = ctx.opts;
    let list_files = listing && !opts.artefacts_only && opts.top > 0;
    let workers = ctx.threads.max(1);
    // (read, job in it, stats)
    let done: Mutex<Vec<(usize, usize, Stats)>> = Mutex::new(Vec::new());
    let mut reads: Vec<Arc<du_fs::Entries>> = Vec::new();
    let mut failed = false;
    rayon::scope(|s| {
        let mut next = first;
        loop {
            let k = reads.len();
            let read = Arc::new(next);
            let n = read.len();
            let per_job = n.div_ceil(workers).max(WIDE_CHUNK);
            for (j, start) in (0..n).step_by(per_job).enumerate() {
                let (read, done) = (Arc::clone(&read), &done);
                s.spawn(move |_| {
                    let range = start..n.min(start + per_job);
                    let mut stats = stat_entries::<false>(
                        ctx,
                        (dir, 0),
                        &read,
                        range,
                        depth,
                        list_files,
                        (path, rules),
                    );
                    // Entry numbers are still this read's own here.
                    if list_files {
                        keep_best(ctx, &read, &mut stats.candidates);
                    }
                    done.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((k, j, stats));
                });
            }
            reads.push(read);
            // A short read ended the listing.
            if failed || n < LISTED_BATCH {
                break;
            }
            let (more, f) = dir.entries_upto(LISTED_BATCH);
            failed = f;
            if more.is_empty() {
                break;
            }
            next = more;
        }
    });
    let mut done = done.into_inner().unwrap_or_else(|e| e.into_inner());
    done.sort_unstable_by_key(|d| (d.0, d.1));
    let mut starts = Vec::with_capacity(reads.len());
    let mut before = 0;
    for read in &reads {
        starts.push(before);
        before += read.len();
    }
    // Every job is done, so each read has no other owner: no copy.
    let all = du_fs::Entries::concat(reads.into_iter().map(Arc::unwrap_or_clone).collect());
    let stats = done
        .into_iter()
        .map(|(k, _, mut stats)| {
            let off = starts[k];
            for d in &mut stats.subdirs {
                d.0 += off;
            }
            for c in &mut stats.candidates {
                c.entry += off;
            }
            stats
        })
        .collect();
    *parts = Some(stats);
    (all, failed)
}

/// What a directory's names say about it.
struct Names {
    /// The [`MARKERS`] among them, in entry order.
    markers: Vec<&'static str>,
    has_dot_git: bool,
    has_gitignore: bool,
    /// The [`bare_part`]s of its entries.
    bare_parts: u8,
}

/// Read every name in `entries` once for [`Names`]. Never inlined: this
/// is [`walk_dir`]'s own loop over every entry, and inside [`enter`] its
/// code moves with every change around it. Round 310's second commit
/// changed no line of it, and flat (one directory of 500,000 entries)
/// rose about two instructions an entry.
#[inline(never)]
fn scan_names(entries: &du_fs::Entries) -> Names {
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
    Names {
        markers,
        has_dot_git,
        has_gitignore,
        bare_parts,
    }
}

/// Walk one open directory. `listing` is false below `.git`, artefacts
/// and trash roots: their sizes count, but nothing inside them is listed.
fn walk_dir(
    ctx: &Ctx<'_>,
    dir: du_fs::Dir,
    buf: &mut PathBuf,
    st: &du_fs::Stat,
    depth: usize,
    listing: bool,
    chain: Option<&Arc<Chain>>,
) -> Option<Node> {
    let path: &Path = buf;
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
        filtered_out: 0,
        left_out: false,
    };
    // A second walk takes the first one's stats of linked files where the
    // listing's inode numbers are the files' own. Every other walk lists
    // and stats as if there were no numbers at all.
    let reuse = (ctx.ino_reuse && dir.lists_true_inodes()).then_some(st.dev);
    // A long directory's stats may start while it is read
    // ([`Ctx::streams`]); they come back in `streamed`.
    let mut streamed = None;
    let (entries, failed) = if reuse.is_some() {
        dir.entries_with_inos()
    } else if ctx.streams {
        let rules = chain.map(|c| &**c);
        list_and_stat(ctx, &dir, depth, listing, (path, rules), &mut streamed)
    } else {
        dir.entries()
    };
    let list_files = listing && !opts.artefacts_only && opts.top > 0;
    let Names {
        markers,
        has_dot_git,
        has_gitignore,
        bare_parts,
    } = scan_names(&entries);
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
            return Some(Node {
                left_out: true,
                ..node
            });
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
    let rules = chain.as_deref();
    let (subdirs, mut candidates) = add_stats(
        ctx,
        (&dir, reuse),
        &entries,
        (depth, list_files),
        (path, rules),
        streamed,
        &mut node,
    );

    let name = dir_name(path, depth).to_string_lossy();
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
        artefact_kind(&name, |c| markers.contains(&c)).or_else(|| {
            let named = |c: &String| *c == *name || Path::new(c) == path;
            opts.collapse.iter().any(named).then_some("collapsed")
        })
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
            rank(
                (ctx.key(x.apparent, x.disk, 1, x.mtime), name_of(x)),
                (ctx.key(y.apparent, y.disk, 1, y.mtime), name_of(y)),
            )
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
                artefact: c.link.then_some(LINK),
                repos: Vec::new(),
                git: None,
                unique: opts.unique.then_some(c.unique),
                filtered_out: 0,
            });
            if !kept {
                break;
            }
        }
    }

    // Held while the subdirectories are read, if the budget allows or
    // their paths are too long to open by; the hold drops after `parent`.
    let hold = if subdirs.is_empty() {
        None
    } else {
        ctx.fds
            .take()
            .or_else(|| (!children_open_by_path(path)).then(|| ctx.fds.force()))
    };
    let parent = if hold.is_some() {
        Some(dir)
    } else {
        drop(dir);
        None
    };
    // A lone subdirectory is walked on this thread, as rayon would walk
    // a list of one, with its name pushed onto this level's path and
    // cut off after. A chain of them then keeps one path, not a copy a
    // level: 1,500 deep, the copies came to 2.3 MB a thread. A PATH is
    // left as given: popping a name from `dir/` would drop the slash.
    let lone = (depth > 0 && subdirs.len() == 1).then(|| subdirs[0]);
    let children: Vec<Option<Node>> = if let Some((i, known)) = lone {
        if depth >= CHAIN_FREE_DEPTH {
            drop(subdirs);
        }
        let cname = entries.get(i).0;
        let rel = parent.as_ref().map(|d| (d, cname));
        let len = buf.as_os_str().len();
        buf.push(du_fs::os(cname));
        let child = enter(
            ctx,
            rel,
            buf,
            known,
            depth + 1,
            inner_listing,
            chain.as_ref(),
        );
        cut_path(buf, len);
        vec![child]
    } else {
        subdirs
            .into_par_iter()
            .map(|(i, known)| {
                let cname = entries.get(i).0;
                let mut child = joined_path(path, cname).into_path_buf();
                let rel = parent.as_ref().map(|d| (d, cname));
                enter(
                    ctx,
                    rel,
                    &mut child,
                    known,
                    depth + 1,
                    inner_listing,
                    chain.as_ref(),
                )
            })
            .collect()
    };
    drop(parent);
    drop(hold);
    let path: &Path = buf;

    let mut largest_child = 0u64;
    for child in children.into_iter().flatten() {
        if child.left_out {
            node.filtered_out += 1;
            continue;
        }
        node.filtered_out += child.filtered_out;
        let k = ctx.key(child.apparent, child.disk, child.items, child.mtime);
        if child.listable && opts.min_size.is_none_or(|floor| k >= floor) {
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

/// The name of the directory at `path`, walked at `depth`. Below the
/// PATH, [`enter`] only walks a path with one listed name pushed on its
/// parent's. That name has no `/`, and `du_fs::Entries::push` never
/// lists `.` or `..`, so on Unix it is the bytes after the last `/`, and
/// no parse of the path is needed. The PATH keeps `file_name`: it is
/// walked once, and `/` has no name.
fn dir_name(path: &Path, depth: usize) -> &OsStr {
    #[cfg(unix)]
    if depth > 0 {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        let start = bytes.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1);
        let name = OsStr::from_bytes(&bytes[start..]);
        debug_assert_eq!(Some(name), path.file_name());
        return name;
    }
    #[cfg(not(unix))]
    let _ = depth;
    path.file_name().unwrap_or_default()
}

/// Cut `buf` back to its first `len` bytes. That is the path before
/// [`enter`] pushed a name on it, and `PathBuf::pop` cuts there too.
/// A deep chain cuts once a level. The pop parses the last component
/// twice from the back to find that known length.
fn cut_path(buf: &mut PathBuf, len: usize) {
    debug_assert_eq!(buf.parent().map(|p| p.as_os_str().len()), Some(len));
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let mut bytes = std::mem::take(buf).into_os_string().into_vec();
        bytes.truncate(len);
        *buf = PathBuf::from(std::ffi::OsString::from_vec(bytes));
    }
    #[cfg(not(unix))]
    buf.pop();
}

/// Append `more` to `all`, or take `more` whole while `all` is empty. A
/// directory stated in one job then keeps that job's lists as they are,
/// with no new block, copy and free for each.
fn take_or_extend<T>(all: &mut Vec<T>, more: Vec<T>) {
    if all.is_empty() {
        *all = more;
    } else {
        all.extend(more);
    }
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
    /// `largest` counts only subdirectories at or above `--min-size`.
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
        if ctx.opts.artefacts_only && matches!(artefact, None | Some("collapsed")) {
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
            filtered_out: self.filtered_out,
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
    let mut snapshots = Vec::new();
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
        let decided;
        let new_ctx = |decided| Ctx {
            opts,
            prefetch,
            root_dev: meta.dev,
            seen: LinkSet::new(),
            claims: LinkClaims::new(meta.dev),
            root_skip: root_skip(&abs),
            decided,
            met: AtomicU64::new(0),
            rows: opts.top > 0 || opts.types,
            fds: FdBudget::new(rayon::current_num_threads()),
            threads: rayon::current_num_threads(),
            ino_reuse: decided.is_some() && listed_inodes_usable(&abs),
            streams: cfg!(target_os = "linux")
                && decided.is_none()
                && rayon::current_num_threads() > 1
                && !opts.filter.as_deref().is_some_and(|s| s.is_active()),
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
            snapshots: Default::default(),
        };
        let mut ctx = new_ctx(None);
        let (apparent, disk, disk_all, unique, artefact) = if meta.is_dir() {
            let walk = |ctx: &Ctx<'_>| {
                let chain = ctx.filter.as_ref().and_then(|f| f.root_chain.as_ref());
                enter(ctx, None, &mut abs.clone(), Some(meta), 0, true, chain)
            };
            let live = opts.control.as_deref();
            let before = live.map(|c| {
                (
                    c.files.load(Ordering::Relaxed),
                    c.disk.load(Ordering::Relaxed),
                )
            });
            let mut node = walk(&ctx);
            // gdu #439: a linked file counted at whichever path a thread
            // met first; walk again with each one held to its smallest
            // path, so every run gives each folder the same total. A
            // stopped scan keeps what it has, and the live counters start
            // over for the second walk. A stop during that walk keeps the
            // first. A scan that reads no rows keeps no claims, so it
            // walks once.
            let claims = std::mem::replace(&mut ctx.claims, LinkClaims::new(ctx.root_dev));
            let settled = if live.is_some_and(ScanControl::stopped) {
                None
            } else {
                claims.settled()
            };
            if let Some(d) = settled {
                if let (Some(c), Some((files, disk))) = (live, before) {
                    c.files.store(files, Ordering::Relaxed);
                    c.disk.store(disk, Ordering::Relaxed);
                }
                decided = d;
                let second = new_ctx(Some(&decided));
                let again = walk(&second);
                // A file that went, or a directory two names show (walked
                // once, under either), leaves a fixed path unmet and its
                // file uncounted: keep the first walk then.
                if second.met.load(Ordering::Relaxed) == decided.len() as u64 {
                    (ctx, node) = (second, again);
                }
            }
            node.map(|n| (n.apparent, n.disk, n.disk_all, n.unique, n.artefact))
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
                    filtered_out: 0,
                });
            }
            (a, d, raw, u, None)
        };
        unreadable += ctx.unreadable.load(Ordering::Relaxed);
        unread.extend(ctx.unread.into_inner().unwrap_or_else(|e| e.into_inner()));
        too_deep += ctx.too_deep.load(Ordering::Relaxed);
        partial |= ctx.cut.load(Ordering::Relaxed);
        snapshots.extend(
            ctx.snapshots
                .into_inner()
                .unwrap_or_else(|e| e.into_inner()),
        );
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
            encoded: shared.encoded,
        }),
        partial,
        types: type_rows(types, opts.sort),
        snapshots: {
            snapshots.sort();
            snapshots
        },
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
        encoded: 0,
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

/// Set the units of this run's text sizes. The `--si` and `--no-prefix`
/// flags win. Without either, and when `env` allows, the first block size
/// variable set ([`crate::util::BLOCK_SIZE_VARS`]) picks them, as
/// `BLOCK_SIZE` does for GNU du and gdu. The `si` config key (in `si`
/// along with the flag) comes last.
fn set_text_units(prog: &str, si: bool, si_flag: bool, raw: bool, env: bool) {
    use crate::util::BlockSize;
    let picked = (env && !si_flag && !raw)
        .then(|| crate::util::block_size_from_env(prog))
        .flatten();
    crate::util::set_si_units(match picked {
        Some(BlockSize::Si) => true,
        Some(_) => false,
        None => si,
    });
    crate::util::set_block_units(match picked {
        Some(BlockSize::Blocks { size, suffix }) => Some((size, suffix)),
        _ => None,
    });
}

/// `du --devices`: the mounted file systems, as text or JSON.
fn devices_out(prog: &str, paths: &[PathBuf], json: bool, (si, si_flag): (bool, bool)) -> i32 {
    if !paths.is_empty() {
        eprintln!("{prog}: --devices lists every mounted file system and reads no PATH");
        return 2;
    }
    set_text_units(prog, si, si_flag, false, true);
    match crate::du_dev::listed(prog) {
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

/// How `--dupes` reports: JSON, NUL-separated names or the table, what it
/// does with the extras, and the put options of `--trash-dupes`
/// (`--home-only`, `--trash-dir`), so the extras go to the trash the
/// summary reads, as a plain put would send them.
struct DupesOut {
    json: bool,
    print0: bool,
    act: DupesAct,
    put_flags: Vec<String>,
}

fn dupes_out(prog: &str, paths: &[PathBuf], opts: &ScanOpts, min_size: u64, out: DupesOut) -> i32 {
    use std::io::Write;
    let DupesOut {
        json,
        print0,
        act,
        put_flags,
    } = out;
    let files = crate::du_dupes::walk(
        paths,
        crate::du_dupes::Opts {
            min_size,
            one_file_system: opts.one_file_system,
            include_kernfs: opts.include_kernfs,
        },
        opts.filter.as_deref(),
    );
    let (groups, cloud) = crate::du_dupes::find(files);
    // On stderr, so --json and --print0 stay as they were.
    if cloud > 0 {
        eprintln!(
            "{prog}: did not read {}; hashing would download the data from the cloud",
            crate::util::plural(cloud, "online-only file", "online-only files")
        );
    }
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
        let tty = crate::util::stdout_is_tty();
        let mut out = std::io::stdout().lock();
        for p in &extras {
            let _ = out.write_all(&crate::util::path_line(p, 0, tty));
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
        let mut args = put_flags;
        args.push("--".into());
        args.extend(extras.iter().map(|p| p.to_string_lossy().into_owned()));
        return crate::put::run(prog, &args);
    }
    0
}

/// `--strict`: exit 1 when the export marked any entry `read_error`.
fn ncdu_out(
    prog: &str,
    path: &Path,
    out: &Path,
    binary: bool,
    opts: &ScanOpts,
    now: i64,
    strict: bool,
) -> i32 {
    let spec = opts.filter.as_deref();
    let mounts = crate::du_ncdu::Mounts {
        one_file_system: opts.one_file_system,
        include_kernfs: opts.include_kernfs,
    };
    let write = |w: &mut dyn Write, own: Option<&Path>| {
        let mut w = io::BufWriter::new(w);
        if binary {
            crate::du_ncdu::export_binary_with(path, mounts, spec, own, &mut w)
        } else {
            crate::du_ncdu::export_with(path, mounts, spec, now, own, &mut w)
        }
    };
    let written = if out.as_os_str() == "-" {
        write(&mut io::stdout().lock(), None)
    } else {
        crate::file_replace::replace_file(out, write)
    };
    let done = match written {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "{prog}: cannot write ncdu export '{}': {e}",
                crate::util::err_path(&out)
            );
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
    if strict && done.unreadable > 0 {
        return 1;
    }
    0
}

/// The PATHs of one run with each tree counted once. A PATH given twice
/// is scanned once, with a note to print. A PATH inside another one is
/// refused, as gdu 5.38 refuses it: the walk of the outer PATH counts it
/// already, so the totals, the explorer and `--dupes` would count it
/// twice. Under `-x` a PATH on another file system than the outer one
/// stays, since that walk stops at the mount. A PATH that cannot be read
/// stays too, for the scan to report.
fn roots_once(
    paths: Vec<PathBuf>,
    follow: bool,
    one_fs: bool,
) -> Result<(Vec<PathBuf>, Vec<String>), String> {
    let stat = |p: &Path| {
        let st = if follow {
            du_fs::stat_follow(p)
        } else {
            du_fs::lstat(p)
        };
        st.ok()
    };
    // Without -L a final link is scanned as a link, so only its parent
    // resolves; with -L the walk goes where the link points.
    let key = |p: &Path| -> Option<PathBuf> {
        let name = p.file_name().filter(|_| !follow);
        let Some(name) = name else {
            return canonical(p).ok();
        };
        let parent = p.parent().filter(|d| !d.as_os_str().is_empty());
        Some(canonical(parent.unwrap_or(Path::new("."))).ok()?.join(name))
    };
    let mut kept: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
    let mut notes = Vec::new();
    for p in paths {
        let k = key(&p);
        if k.is_some() && kept.iter().any(|(_, q)| *q == k) {
            let shown = crate::util::err_path(&p);
            notes.push(format!("'{shown}' is given twice; counting it once"));
        } else {
            kept.push((p, k));
        }
    }
    for (inner, in_key) in &kept {
        for (outer, out_key) in &kept {
            let (Some(in_key), Some(out_key)) = (in_key, out_key) else {
                continue;
            };
            if in_key == out_key || !in_key.starts_with(out_key) {
                continue;
            }
            let (Some(o), Some(i)) = (stat(outer), stat(inner)) else {
                continue;
            };
            if !o.is_dir() || (one_fs && i.dev != o.dev) {
                continue;
            }
            return Err(format!(
                "'{}' is inside '{}', which counts it already; give one of them",
                crate::util::err_path(inner),
                crate::util::err_path(outer)
            ));
        }
    }
    Ok((kept.into_iter().map(|(p, _)| p).collect(), notes))
}

/// Whether the scan can start at PATH `p`: the same lookups [`scan_on`]
/// makes before it walks, so a PATH that passes here does not end the run.
fn root_stat(p: &Path, follow: bool) -> io::Result<()> {
    let abs = absolute(p)?;
    if follow {
        du_fs::stat_follow(&abs)?;
    } else {
        du_fs::lstat(&abs)?;
    }
    Ok(())
}

pub(crate) fn absolute(p: &Path) -> io::Result<PathBuf> {
    if p.file_name().is_none() {
        // `.`, `..`, `/`: no final component to keep unresolved.
        return canonical(p);
    }
    let parent = match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => canonical(d)?,
        // A verbatim current directory prints in its plain form too.
        _ => crate::util::plain_path(&std::env::current_dir()?),
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
    Ok(crate::util::plain_path(&c))
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
/// than there are CPUs; cached walks go slower, so this decides. It
/// opens no subdirectory that `filter` leaves out (see [`probe_opens`]).
fn looks_cold(path: &Path, filter: Option<&du_filter::Filter>) -> bool {
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
        if !is_dir || subdirs == PROBE_SUBDIRS || !probe_opens(filter, path, name) {
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

/// Whether the cold-cache probe may open the subdirectory `name` of the
/// scan root `dir`. Not one the filters leave out: opening an excluded
/// folder is what brings up the macOS privacy prompts (dust #546). The
/// probe reads no `.gitignore`, so under `--gitignore` it opens none.
fn probe_opens(filter: Option<&du_filter::Filter>, dir: &Path, name: &CStr) -> bool {
    filter.is_none_or(|f| {
        !f.gitignore && !f.excluded(f.root_chain.as_deref(), dir, name.to_bytes(), true)
    })
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

/// `,"filtered_out":N` on a directory that holds entries the filters
/// left out; nothing otherwise, so unfiltered output keeps its shape.
fn filtered_json(n: u64) -> String {
    if n > 0 {
        format!(",\"filtered_out\":{n}")
    } else {
        String::new()
    }
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
        unique_json(it.unique) + &filtered_json(it.filtered_out),
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

/// `--show-symlink-target`: each listed row that is itself a symlink,
/// by its path, with the target as the link stores it (as `ls -l` shows
/// it, relative or not). Rows reached through a linked directory are not
/// links themselves and are left out.
type Targets = HashMap<PathBuf, PathBuf>;

/// The [`Targets`] of the rows of `r`, read from disk after the scan.
fn link_targets(r: &Report) -> Targets {
    r.dirs
        .iter()
        .chain(&r.files)
        .filter_map(|it| Some((it.path.clone(), fs::read_link(&it.path).ok()?)))
        .collect()
}

/// The target of `it` when `targets` names one.
fn target_of<'a>(targets: Option<&'a Targets>, it: &Item) -> Option<&'a Path> {
    targets?.get(&it.path).map(PathBuf::as_path)
}

/// `shown`, then ` -> TARGET` when the row is a link, as gdu and `ls -l`
/// print it.
fn with_target(shown: String, target: Option<&Path>) -> String {
    match target {
        Some(t) => format!("{shown} -> {}", crate::util::term_path(t)),
        None => shown,
    }
}

/// [`item_json`] with `"ratio"` under `--ratio`, then `"target"` last
/// when the row is a link; other rows keep their shape.
fn item_json_with(it: &Item, target: Option<&Path>, ratio: bool) -> String {
    let j = item_json(it);
    let target = target
        .map(|t| format!(",\"target\":\"{}\"", json_escape(&t.to_string_lossy())))
        .unwrap_or_default();
    format!(
        "{}{}{target}}}",
        &j[..j.len() - 1],
        ratio_key(ratio, it.disk, it.apparent)
    )
}

/// Tags shown in the NOTE column.
/// Unreadable entries as the report keeps them: path and reason.
pub(crate) type Unread = [(PathBuf, String)];

/// `unreadable` for a directory the scan could not read, `incomplete` for
/// one with an unreadable entry below it (its sizes are low), as ncdu
/// marks `!` and `.`; None otherwise. The report keeps the first 100
/// unreadable paths, so rows past those go unmarked.
pub(crate) fn read_mark(it: &Item, unread: &Unread) -> Option<&'static str> {
    if !it.is_dir {
        return None;
    }
    if unread.iter().any(|(p, _)| *p == it.path) {
        Some("unreadable")
    } else if unread.iter().any(|(p, _)| p.starts_with(&it.path)) {
        Some("incomplete")
    } else {
        None
    }
}

fn notes(it: &Item, unread: &Unread) -> String {
    let mut v: Vec<String> = Vec::new();
    if let Some(a) = it.artefact {
        v.push(a.to_string());
    }
    if let Some(m) = read_mark(it, unread) {
        v.push(m.to_string());
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
    report_json_with(r, trash, Some(r.unreadable), None, None, false)
}

/// [`report_json`] with the unreadable count given, `null` when unknown
/// (a snapshot written before it was recorded).
fn report_json_with(
    r: &Report,
    trash: &TrashSummary,
    unreadable: Option<u64>,
    targets: Option<&Targets>,
    held: Option<&crate::holders::OpenDeleted>,
    ratio: bool,
) -> String {
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
                unique_json(t.unique) + &ratio_key(ratio, t.disk, t.apparent),
                t.files,
                t.links,
                t.dirs,
                t.excluded,
                t.inodes(),
                json_opt(t.artefact)
            )
        })
        .collect();
    let row = |it: &Item| item_json_with(it, target_of(targets, it), ratio);
    let dirs: Vec<String> = r.dirs.iter().map(row).collect();
    let files: Vec<String> = r.files.iter().map(row).collect();
    let shared = r
        .shared
        .map(|s| {
            // Only a scan that met compressed extents has the key.
            let encoded = if s.encoded > 0 {
                format!(",\"encoded\":{}", s.encoded)
            } else {
                String::new()
            };
            format!(
                ",\"shared\":{{\"distinct\":{},\"referenced\":{}{encoded}}}",
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
    let too_deep = if r.snapshots.is_empty() {
        too_deep
    } else {
        let rows: Vec<String> = r
            .snapshots
            .iter()
            .map(|p| format!("\"{}\"", json_escape(&p.to_string_lossy())))
            .collect();
        format!("{too_deep},\"snapshots\":[{}]", rows.join(","))
    };
    // The open_deleted key, when there is one, comes last.
    let trash_and_held = trash_json(trash) + &open_deleted_json(held);
    format!(
        "{{\"roots\":[{}],\"dirs\":[{}],\"files\":[{}],\"unreadable\":{}{unread}{too_deep}{shared}{partial}{types},\"trash\":{}}}",
        roots.join(","),
        dirs.join(","),
        files.join(","),
        unreadable.map_or_else(|| "null".to_string(), |n| n.to_string()),
        trash_and_held
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
    /// Whether the SHARE column shows; `share` also feeds the bars.
    show_share: bool,
    /// `--bars`: a BAR column of the same share, as dust draws.
    bars: bool,
    /// `--graph-style`: the characters the bars are drawn with.
    graph: GraphStyle,
    /// `--reverse`: the smallest (or oldest) row first, so the largest
    /// ends at the bottom next to the prompt, as dust -r.
    reverse: bool,
    /// The report's unreadable paths, for the NOTE column.
    unread: &'a Unread,
    /// `--show-symlink-target`: the rows that are links, and their targets.
    targets: Option<&'a Targets>,
    /// `--ratio`: the RATIO column, disk use over apparent size.
    ratio: bool,
}

/// Disk use over apparent size in thousandths, rounded; None for a row
/// with no apparent size.
fn ratio_milli(disk: u64, apparent: u64) -> Option<u64> {
    if apparent == 0 {
        return None;
    }
    let m = (u128::from(disk) * 1000 + u128::from(apparent) / 2) / u128::from(apparent);
    Some(u64::try_from(m).unwrap_or(u64::MAX))
}

/// `38%`: the RATIO column, disk use as a percent of apparent size; `-`
/// with no apparent size and `>999%` past that (a few bytes in a block).
fn ratio_text(disk: u64, apparent: u64) -> String {
    match ratio_milli(disk, apparent) {
        None => "-".into(),
        Some(m) if m >= 9_995 => ">999%".into(),
        Some(m) => format!("{}%", (m + 5) / 10),
    }
}

/// `,"ratio":0.381` under `--ratio` (null with no apparent size), else
/// nothing, so other output keeps its shape.
fn ratio_key(on: bool, disk: u64, apparent: u64) -> String {
    if !on {
        return String::new();
    }
    match ratio_milli(disk, apparent) {
        Some(m) => format!(",\"ratio\":{}.{:03}", m / 1000, m % 1000),
        None => ",\"ratio\":null".into(),
    }
}

/// `, ratio 38%` for a Total line under `--ratio`, else nothing.
fn ratio_note(on: bool, disk: u64, apparent: u64) -> String {
    if on {
        format!(", ratio {}", ratio_text(disk, apparent))
    } else {
        String::new()
    }
}

/// `42.5%`: the part of the scan root that holds `it`, or `-` when no
/// root holds it or the root is empty.
fn share_of(it: &Item, roots: &[RootTotals], apparent: bool) -> String {
    match root_part(it, roots, apparent) {
        Some(part) => format!("{:.1}%", part * 100.0),
        None => "-".into(),
    }
}

/// The part (0 to 1) of the scan root that holds `it`, or None when no
/// root holds it or the root is empty.
fn root_part(it: &Item, roots: &[RootTotals], apparent: bool) -> Option<f64> {
    let root = roots
        .iter()
        .filter(|r| it.path.starts_with(&r.path))
        .max_by_key(|r| r.path.as_os_str().len())?;
    let pick = |a: u64, d: u64| if apparent { a } else { d };
    let total = pick(root.apparent, root.disk);
    if total == 0 {
        return None;
    }
    Some((pick(it.apparent, it.disk) as f64 / total as f64).min(1.0))
}

/// Cells of a `--bars` bar.
const BAR_CELLS: usize = 10;

/// `--graph-style`: how a bar fills its cells, as ncdu names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum GraphStyle {
    /// One `#` per tenth.
    #[default]
    Hash,
    /// Full blocks, and a left half block for a half cell.
    HalfBlock,
    /// Full blocks, and a left block of one to seven eighths.
    EighthBlock,
}

impl GraphStyle {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "hash" => Some(Self::Hash),
            "half-block" => Some(Self::HalfBlock),
            "eighth-block" => Some(Self::EighthBlock),
            _ => None,
        }
    }

    /// Steps in one cell.
    fn steps(self) -> usize {
        match self {
            Self::Hash => 1,
            Self::HalfBlock => 2,
            Self::EighthBlock => 8,
        }
    }
}

/// Left blocks of one to seven eighths of a cell.
const EIGHTHS: [char; 7] = [
    '\u{258f}', '\u{258e}', '\u{258d}', '\u{258c}', '\u{258b}', '\u{258a}', '\u{2589}',
];

/// A bar of [`BAR_CELLS`] cells for `part` (rounded to the style's
/// steps), padded with spaces; blank for no part.
fn bar(part: Option<f64>, style: GraphStyle) -> String {
    let steps = style.steps();
    let total = BAR_CELLS * steps;
    let filled = part.map_or(0, |p| (p * total as f64).round() as usize);
    let filled = filled.min(total);
    let (full, rest) = (filled / steps, filled % steps);
    let mut out = match style {
        GraphStyle::Hash => "#".repeat(full),
        _ => "\u{2588}".repeat(full),
    };
    let mut cells = full;
    if rest > 0 {
        let eighths = if style == GraphStyle::HalfBlock {
            4
        } else {
            rest
        };
        out.push(EIGHTHS[eighths - 1]);
        cells += 1;
    }
    out.push_str(&" ".repeat(BAR_CELLS - cells));
    out
}

/// `--tree` without a depth: two levels below each PATH.
const TREE_DEPTH: usize = 2;

/// Rows a `--tree` scan keeps, so no folder of the tree loses a child to
/// the flat top-N cut; each folder then shows its `--top` largest.
const TREE_ROWS: usize = 1 << 20;

/// The rows of `report` under each folder, ranked by `sort` (ties by
/// path), at most `top` per folder. With `dirs_first`, the folders kept
/// then move ahead of the files, each group in rank order.
fn tree_children(
    report: &Report,
    sort: SortKey,
    top: usize,
    dirs_first: bool,
) -> HashMap<&Path, Vec<&Item>> {
    let mut kids: HashMap<&Path, Vec<&Item>> = HashMap::new();
    for it in report.dirs.iter().chain(&report.files) {
        if let Some(parent) = it.path.parent() {
            kids.entry(parent).or_default().push(it);
        }
    }
    for v in kids.values_mut() {
        v.sort_by(|a, b| rank_items(a, b, sort));
        v.truncate(top);
        if dirs_first {
            // A stable sort keeps the rank order inside each group.
            v.sort_by_key(|it| !it.is_dir);
        }
    }
    kids
}

/// The lines `--tree` draws: a middle branch, the last branch, and the
/// rails under a middle and a last branch.
type TreeGlyphs = [&'static str; 4];

/// Box lines, as dust draws its tree.
const TREE_UNICODE: TreeGlyphs = [
    "\u{251c}\u{2500} ",
    "\u{2514}\u{2500} ",
    "\u{2502}  ",
    "   ",
];

/// `--no-unicode`: the same shape in ASCII, as `tree --charset=ascii`.
const TREE_ASCII: TreeGlyphs = ["|-- ", "`-- ", "|   ", "    "];

/// `--tree`: each PATH with its total, then the rows below it nested
/// under their folders, drawn with `glyphs`.
fn tree_lines(
    report: &Report,
    sort: SortKey,
    top: usize,
    dirs_first: bool,
    glyphs: &TreeGlyphs,
    targets: Option<&Targets>,
) -> Vec<String> {
    let kids = tree_children(report, sort, top, dirs_first);
    let mut out = Vec::new();
    for root in &report.roots {
        out.push(format!(
            "{:>10}  {:>10}  {}",
            format_bytes(root.disk),
            format_bytes(root.apparent),
            root.path.display()
        ));
        let at = Level {
            glyphs,
            unread: &report.unread,
            targets,
        };
        tree_level(&kids, &root.path, "", at, &mut out);
    }
    out
}

/// What every level of a `--tree` draws with.
#[derive(Clone, Copy)]
struct Level<'a> {
    glyphs: &'a TreeGlyphs,
    unread: &'a Unread,
    targets: Option<&'a Targets>,
}

fn tree_level(
    kids: &HashMap<&Path, Vec<&Item>>,
    dir: &Path,
    prefix: &str,
    at: Level,
    out: &mut Vec<String>,
) {
    let Level {
        glyphs,
        unread,
        targets,
    } = at;
    let Some(rows) = kids.get(dir) else {
        return;
    };
    let last = rows.len().saturating_sub(1);
    for (n, it) in rows.iter().enumerate() {
        let [mid, end, rail, gap] = *glyphs;
        let (branch, more) = if n == last { (end, gap) } else { (mid, rail) };
        let name = it.path.file_name().unwrap_or(it.path.as_os_str());
        let slash = if it.is_dir { "/" } else { "" };
        let mark = read_mark(it, unread)
            .map(|m| format!("  ({m})"))
            .unwrap_or_default();
        let shown = with_target(
            format!("{}{slash}", name.to_string_lossy()),
            target_of(targets, it),
        );
        out.push(format!(
            "{:>10}  {:>10}  {prefix}{branch}{shown}{mark}",
            format_bytes(it.disk),
            format_bytes(it.apparent),
        ));
        if it.is_dir {
            let prefix = format!("{prefix}{more}");
            tree_level(kids, &it.path, &prefix, at, out);
        }
    }
}

fn print_table(title: &str, items: &[Item], now: i64, cols: Columns) {
    let Columns {
        count,
        show_mtime,
        share,
        show_share,
        bars,
        graph,
        reverse,
        unread,
        targets,
        ratio,
    } = cols;
    if items.is_empty() {
        return;
    }
    println!("{title}");
    let width = items
        .iter()
        .map(|it| notes(it, unread).len())
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
    let ratios = |v: &str| {
        if ratio {
            format!("{v:>6}  ")
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
        Some(_) if show_share => format!("{v:>6}  "),
        _ => String::new(),
    };
    let bars_of = |v: &str| {
        if bars {
            format!("{v:<BAR_CELLS$}  ")
        } else {
            String::new()
        }
    };
    let when_of = |it: &Item| {
        if show_mtime {
            format_mtime(it.mtime)
        } else {
            format_age(it.mtime, now)
        }
    };
    println!(
        "{:>10}  {:>10}  {}{}{}{}{}{:>when_width$}  {:<width$}  PATH",
        "DISK",
        "APPARENT",
        ratios("RATIO"),
        bars_of("BAR"),
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
        let path = with_target(path, target_of(targets, it));
        println!(
            "{:>10}  {:>10}  {}{}{}{}{}{:>when_width$}  {:<width$}  {}",
            format_bytes(it.disk),
            format_bytes(it.apparent),
            ratios(&ratio_text(it.disk, it.apparent)),
            bars_of(&bar(share.and_then(|(r, a)| root_part(it, r, a)), graph)),
            shares(&share.map_or(String::new(), |(r, a)| share_of(it, r, a))),
            uniques(&it.unique.map(format_bytes).unwrap_or_default()),
            counts(&it.items.to_string()),
            when_of(it),
            notes(it, unread),
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

/// The `--unique` note on compressed extents, when the scan met any: they
/// count at their uncompressed size, the only one FIEMAP and `st_blocks`
/// give.
fn encoded_line(s: SharedTotals) -> Option<String> {
    (s.encoded > 0).then(|| {
        format!(
            "Compressed extents: {} of the mapped files, counted at their uncompressed size \
             (compsize, as root, reads what they take)",
            format_bytes(s.encoded)
        )
    })
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
    let width = items
        .iter()
        .map(|it| notes(it, &report.unread).len())
        .max()
        .unwrap_or(1);
    items
        .into_iter()
        .map(|it| crate::put_tui::PathRow {
            path: it.path.clone(),
            is_dir: it.is_dir,
            label: Some(format!(
                "{:>10}  {:<width$}  {}",
                format_bytes(it.disk),
                notes(it, &report.unread),
                it.path.display()
            )),
            warn: it.git.as_ref().is_some_and(|(s, _)| s.is_risky()),
        })
        .collect()
}

/// Drop the directory rows that hold entries the filters left out, for
/// --print0 and --pick: trashing one would take the hidden entries too.
/// Returns how many went.
fn hold_filtered(dirs: &mut Vec<Item>) -> usize {
    let before = dirs.len();
    dirs.retain(|it| it.filtered_out == 0);
    before - dirs.len()
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

/// 35(e): the flags of `du` that do not go together, one row per flag:
/// the flag, what it does, and the flags it takes none of. `run` checks
/// the table right after reading the arguments, and refuses the first
/// row that matches with exit 2 and a message that names both flags. A
/// refusal of two flags lives here and nowhere else; precheck refuses an
/// "it takes no" message elsewhere in du.rs, and the test
/// `every_pair_in_the_conflict_table_is_refused` runs each pair.
/// The flags that filter what a scan walks. A snapshot read with -f holds
/// totals, not a walk, so it takes none of them (36 b). The config keys
/// that set filters stay quiet with -f, since they are defaults.
pub const SCAN_FILTERS: &[&str] = &[
    "--exclude",
    "--exclude-from",
    "--filter-from",
    "--type",
    "--exclude-type",
    "--exclude-regex",
    "--include-regex",
    "--no-hidden",
    "--gitignore",
    "--exclude-caches",
];

pub const MODE_CONFLICTS: &[(&str, &str, &[&str])] = &[
    (
        "--json",
        "prints a JSON report",
        &["--print0", "--pick", "-i"],
    ),
    ("--print0", "prints paths for xargs -0", &["--pick", "-i"]),
    ("--pick", "picks rows to trash from a list", &["-i"]),
    (
        "--tree",
        "prints the table as a tree",
        &["--json", "--print0", "-s", "--types"],
    ),
    ("-s", "prints the totals only", &["--skip-total"]),
    (
        "--no-prefix",
        "is for printed sizes, and the explorer keeps its units",
        &["-i"],
    ),
    (
        "-L",
        "only reports, since a followed link lists its target's size under its own name",
        &["--print0", "--pick", "-i", "--ncdu"],
    ),
    (
        "--open-deleted",
        "adds a line under the totals of a scan",
        &[
            "--print0",
            "--pick",
            "-i",
            "--dupes",
            "--devices",
            "--stacks",
            "--ncdu",
            "-o",
            "-f",
        ],
    ),
    (
        "--min-size",
        "floors the listed rows",
        &[
            "-i",
            "--pick",
            "-o",
            "--stacks",
            "--ncdu",
            "--types",
            "--devices",
        ],
    ),
    (
        "--devices",
        "lists every mounted file system",
        &[
            "--ncdu", "-o", "-f", "--stacks", "--print0", "--pick", "-i", "--unique", "--shared",
            "--types",
        ],
    ),
    (
        "--dupes",
        "finds duplicate files",
        &["--ncdu", "-o", "-f", "--stacks", "--pick", "-i", "--types"],
    ),
    (
        "--trash-dupes",
        "trashes the extra copies",
        &["--link-dupes", "--clone-dupes", "--json", "--print0"],
    ),
    (
        "--link-dupes",
        "hard-links the extra copies",
        &["--clone-dupes", "--json", "--print0"],
    ),
    (
        "--clone-dupes",
        "clones the extra copies",
        &["--json", "--print0"],
    ),
    (
        "-o",
        "writes a snapshot",
        &["-f", "--json", "--print0", "--pick", "-i"],
    ),
    (
        "--ncdu",
        "writes an ncdu export",
        &[
            "-o", "-f", "--stacks", "--json", "--print0", "--pick", "-i", "--unique", "--shared",
            "--types",
        ],
    ),
    (
        "--stacks",
        "prints folded stacks",
        &["-o", "--json", "--print0", "--pick", "-i"],
    ),
    (
        "--unique",
        "adds a column to a live listing",
        &["-f", "-o", "--stacks", "-i"],
    ),
    (
        "--shared",
        "changes the sizes of a live listing",
        &["-f", "-o", "--stacks", "-i", "--types"],
    ),
    (
        "--types",
        "lists file types from a live scan",
        &["-f", "-o", "--stacks", "--print0", "--pick", "-i"],
    ),
    (
        "--ratio",
        "adds a RATIO column to the table and a ratio key to the JSON",
        &[
            "--tree",
            "--print0",
            "--pick",
            "-i",
            "-o",
            "--ncdu",
            "--stacks",
            "--dupes",
            "--devices",
            "--types",
        ],
    ),
    ("--filter-from", "filters what a scan walks", &["--devices"]),
    (
        "-f",
        "reads saved totals, which no filter can change",
        SCAN_FILTERS,
    ),
    (
        "-f",
        "reads a snapshot, which can be stale",
        &["--print0", "--pick"],
    ),
];

/// The refusal of the first row of [`MODE_CONFLICTS`] whose flag and one
/// of whose other flags `given` reports. `ncdu_flag` is the spelling of
/// --ncdu on the command line (`--ncdu` or `--ncdu-binary`).
fn mode_conflict(given: &dyn Fn(&str) -> bool, ncdu_flag: &str) -> Option<String> {
    MODE_CONFLICTS.iter().find_map(|&(flag, does, others)| {
        let hit = given(flag) && others.iter().any(|&o| given(o));
        hit.then(|| {
            let shown = if flag == "--ncdu" { ncdu_flag } else { flag };
            format!("{shown} {does}; it takes no {}", or_list(others))
        })
    })
}

/// `a`, `a or b`, `a, b or c`.
fn or_list(items: &[&str]) -> String {
    match items.split_last() {
        Some((last, [])) => (*last).to_string(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
        None => String::new(),
    }
}

pub fn run(prog: &str, args: &[String]) -> i32 {
    let mut missing = false;
    let code = run_paths(prog, args, &mut missing);
    // A PATH that could not be read was named and left out and the rest
    // were scanned; the run still fails, as in GNU du.
    if missing && code == 0 {
        1
    } else {
        code
    }
}

/// [`run`], which sets `missing` when it leaves out a PATH it cannot read.
fn run_paths(prog: &str, args: &[String], missing: &mut bool) -> i32 {
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
    let mut allow_filtered = false;
    let mut pick = false;
    let mut interactive = false;
    let mut no_delete = false;
    let mut output: Option<PathBuf> = None;
    let mut ncdu: Option<PathBuf> = None;
    let mut ncdu_binary = false;
    let mut devices = false;
    // The filter flags given on the command line, by long name.
    let mut filter_flags: Vec<&str> = Vec::new();
    let mut open_deleted = false;
    let mut dupes = false;
    let mut trash_dupes = false;
    let mut link_dupes = false;
    let mut clone_dupes = false;
    let mut min_size: Option<u64> = None;
    let mut from_file: Option<PathBuf> = None;
    let mut threads: Option<usize> = None;
    let mut filter = du_filter::Spec::default();
    // --type extensions: only files with one of these count.
    let mut keep_types: Vec<String> = Vec::new();
    let mut count = false;
    let mut show_mtime = false;
    let mut show_targets = false;
    let mut relative = false;
    let mut ratio = false;
    let mut bars = false;
    let mut graph = GraphStyle::default();
    let mut tree: Option<usize> = None;
    let mut ascii = false;
    let mut dirs_first = false;
    let mut strict = false;
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
    let mut si_flag = false;
    let mut raw_bytes = false;
    // The config key only shapes -i; the flag alone is an error below.
    let mut read_only = cfg.bool("du", "no_delete").unwrap_or(false);
    filter.exclude_caches = cfg.bool("du", "exclude_caches").unwrap_or(false);
    filter.gitignore = cfg.bool("du", "gitignore").unwrap_or(false);
    for p in cfg.list("du", "exclude") {
        if du_filter::Pattern::parse(p).is_some() {
            filter.patterns.push(p.clone());
        }
    }
    // Regexes as --exclude-regex and --include-regex take them; the flags
    // add to these. A regex that does not compile is named and skipped.
    for (key, out) in [
        ("exclude_regex", &mut filter.regex_out),
        ("include_regex", &mut filter.regex_in),
    ] {
        for re in cfg.list("du", key) {
            match du_filter::path_regex(re) {
                Ok(_) => out.push(re.clone()),
                Err(e) => eprintln!("{prog}: warning: config du.{key}: {e}"),
            }
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
        } else if let Some(v) = take_value(args, &mut i, arg, "--graph-style", None) {
            match v.map(GraphStyle::parse) {
                Ok(Some(g)) => {
                    graph = g;
                    bars = true;
                }
                Ok(None) => bail!("--graph-style takes hash, half-block or eighth-block"),
                Err(e) => bail!(e),
            }
        } else if let Some(v) = arg.strip_prefix("--tree=") {
            match parse_count(v, "depth") {
                Ok(n) => tree = Some(n),
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
            filter_flags.push("--type");
        } else if let Some(v) = take_value(args, &mut i, arg, "--exclude-type", None) {
            match v.and_then(|v| du_filter::extensions(v, "--exclude-type")) {
                Ok(exts) => filter
                    .patterns
                    .extend(exts.iter().map(|e| format!("*.{e}"))),
                Err(e) => bail!(e),
            }
            filter_flags.push("--exclude-type");
        } else if let Some(v) = take_value(args, &mut i, arg, "--collapse", None) {
            match v {
                Ok("") => bail!("--collapse: empty name"),
                Ok(v) => opts.collapse.push(v.trim_end_matches(['/', '\\']).into()),
                Err(e) => bail!(e),
            }
        } else if let Some(v) = take_value(args, &mut i, arg, "--exclude-regex", None) {
            match v.and_then(du_filter::path_regex) {
                Ok(r) => filter.regex_out.push(r.as_str().into()),
                Err(e) => bail!(format!("--exclude-regex: {e}")),
            }
            filter_flags.push("--exclude-regex");
        } else if let Some(v) = take_value(args, &mut i, arg, "--include-regex", None) {
            match v.and_then(du_filter::path_regex) {
                Ok(r) => filter.regex_in.push(r.as_str().into()),
                Err(e) => bail!(format!("--include-regex: {e}")),
            }
            filter_flags.push("--include-regex");
        } else if let Some(v) = take_value(args, &mut i, arg, "--exclude", None) {
            match v {
                Ok(v) if du_filter::Pattern::parse(v).is_some() => filter.patterns.push(v.into()),
                Ok(v) => bail!(format!("--exclude: empty pattern '{v}'")),
                Err(e) => bail!(e),
            }
            filter_flags.push("--exclude");
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
            filter_flags.push("--exclude-from");
        } else if let Some(v) = take_value(args, &mut i, arg, "--filter-from", None) {
            match v.map(|v| (v, du_filter::read_rsync_file(&crate::util::arg_path(v)))) {
                Ok((_, Ok(Ok(rules)))) => filter.rsync.extend(rules),
                Ok((v, Ok(Err(e)))) => bail!(format!("--filter-from '{v}': {e}")),
                Ok((v, Err(e))) => {
                    eprintln!("{prog}: cannot read filter file '{v}': {e}");
                    return 1;
                }
                Err(e) => bail!(e),
            }
            filter_flags.push("--filter-from");
        } else if let Some(v) = take_value(args, &mut i, arg, "--older-than", None)
            .or_else(|| take_value(args, &mut i, arg, "--min-age", None))
        {
            match v.and_then(crate::filters::parse_age_secs) {
                Ok(secs) => tighten(&mut opts.older_cutoff, now - secs, i64::min),
                Err(e) => bail!(e),
            }
        } else if let Some(v) = take_value(args, &mut i, arg, "--newer-than", None)
            .or_else(|| take_value(args, &mut i, arg, "--max-age", None))
        {
            match v.and_then(crate::filters::parse_age_secs) {
                Ok(secs) => tighten(&mut opts.newer_cutoff, now - secs, i64::max),
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
                "--show-symlink-target" => show_targets = true,
                "--show-relative-size" => relative = true,
                "--ratio" => ratio = true,
                "--bars" => bars = true,
                "--tree" => tree = Some(TREE_DEPTH),
                "--no-unicode" => ascii = true,
                "--dirs-first" => dirs_first = true,
                "--strict" => strict = true,
                "--reverse" => reverse = true,
                // GNU du -s and gdu --summarize: the totals, no rows.
                "-s" | "--summarize" => summarize = true,
                "--skip-total" => skip_total = true,
                "--inodes" => opts.sort = SortKey::Items,
                "--all-dirs" => opts.all_dirs = true,
                "--artefacts" => opts.artefacts_only = true,
                "--exclude-caches" => {
                    filter.exclude_caches = true;
                    filter_flags.push("--exclude-caches");
                }
                // gdu's --no-hidden: a dot name at any depth, in order
                // with --exclude so a later !pattern can bring one back.
                "--no-hidden" => {
                    filter.patterns.push(".*".to_string());
                    filter_flags.push("--no-hidden");
                }
                "--gitignore" => {
                    filter.gitignore = true;
                    filter_flags.push("--gitignore");
                }
                "--dirs" => only_dirs = true,
                "--files" => only_files = true,
                "--devices" => devices = true,
                "--open-deleted" => open_deleted = true,
                "--json" => json = true,
                "--print0" => print0 = true,
                "--dupes" => dupes = true,
                "--trash-dupes" => trash_dupes = true,
                "--link-dupes" => link_dupes = true,
                "--clone-dupes" => clone_dupes = true,
                a if a.starts_with("--min-size=") => {
                    match crate::filters::parse_size(&a["--min-size=".len()..]) {
                        Some(n) => min_size = Some(n),
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
                "--allow-filtered" => allow_filtered = true,
                "--pick" => pick = true,
                "-i" | "--interactive" => interactive = true,
                "--no-delete" => no_delete = true,
                "--si" => (si, si_flag) = (true, true),
                "--no-prefix" => raw_bytes = true,
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
    // The SHARE column and the bars weigh rows the same way.
    let want_share = relative || bars;
    let given = |flag: &str| match flag {
        "--json" => json,
        "--print0" => print0,
        "--pick" => pick,
        "-i" => interactive,
        "--tree" => tree.is_some(),
        "-s" => summarize,
        "--skip-total" => skip_total,
        "--no-prefix" => raw_bytes,
        "-L" => opts.follow,
        "--open-deleted" => open_deleted,
        // --dupes reads --min-size as its own floor.
        "--min-size" => min_size.is_some() && !dupes,
        "--devices" => devices,
        "--dupes" => dupes,
        "--trash-dupes" => trash_dupes,
        "--link-dupes" => link_dupes,
        "--clone-dupes" => clone_dupes,
        "-o" => output.is_some(),
        "--ncdu" => ncdu.is_some(),
        "-f" => from_file.is_some(),
        "--stacks" => stacks,
        "--unique" => opts.unique,
        "--shared" => opts.shared_once,
        "--types" => opts.types,
        "--ratio" => ratio,
        other => filter_flags.contains(&other),
    };
    let ncdu_flag = if ncdu_binary {
        "--ncdu-binary"
    } else {
        "--ncdu"
    };
    if let Some(refusal) = mode_conflict(&given, ncdu_flag) {
        eprintln!("{prog}: {refusal}");
        return 2;
    }
    // A floor on listed rows covers the table, --json, --tree and
    // --print0 of a scan, and -f. A snapshot (-o) records every total for
    // du diff, so it takes none; the rest are not covered yet.
    if min_size.is_some() && !dupes {
        if !matches!(opts.sort, SortKey::Disk | SortKey::Apparent) {
            eprintln!(
                "{prog}: --min-size compares disk use, or apparent size with --apparent, \
                 so --sort=items and --sort=mtime do not apply"
            );
            return 2;
        }
        // A subdirectory the floor drops must not hide its parent.
        opts.min_size = min_size;
    }
    if summarize {
        // After the loop, so a later -n does not bring rows back.
        opts.top = 0;
    }
    // The --top of each tree folder; the scan itself keeps every row.
    let tree_top = opts.top;
    if let Some(depth) = tree {
        opts.max_depth = Some(opts.max_depth.map_or(depth, |m| m.min(depth)));
        opts.all_dirs = true;
        opts.top = TREE_ROWS;
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
    if pick && !pick_available() {
        eprintln!("{prog}: --pick needs a terminal and the tui feature");
        return 2;
    }
    if interactive && !interactive_available() {
        eprintln!("{prog}: --interactive needs a terminal and the tui feature");
        return 2;
    }
    if opts.follow && cfg!(windows) {
        eprintln!("{prog}: --follow-symlinks is not supported on Windows");
        return 2;
    }
    // Without -L the walk lists only regular files and directories, so no
    // row is a link; a snapshot keeps paths, not the links on disk now.
    if show_targets && (!opts.follow || from_file.is_some()) {
        eprintln!(
            "{prog}: --show-symlink-target needs --follow-symlinks (-L) on a live scan: \
             without -L no row is a link, and -f reads no links"
        );
        return 2;
    }
    if devices {
        return devices_out(prog, &paths, json, (si, si_flag));
    }
    if paths.is_empty() && listed {
        eprintln!("{prog}: the path list names nothing to scan");
        return 2;
    }
    if paths.is_empty() {
        paths.push(PathBuf::from("."));
    }
    if from_file.is_none() {
        match roots_once(
            std::mem::take(&mut paths),
            opts.follow,
            opts.one_file_system,
        ) {
            Ok((once, notes)) => {
                for n in notes {
                    eprintln!("{prog}: {n}");
                }
                paths = once;
            }
            Err(e) => {
                eprintln!("{prog}: {e}");
                return 2;
            }
        }
        // dust #585: an empty or missing PATH used to end the whole run.
        // It is named and left out, and the others are scanned.
        paths.retain(|p| match root_stat(p, opts.follow) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("{prog}: cannot access '{}': {e}", crate::util::err_path(p));
                *missing = true;
                false
            }
        });
        if paths.is_empty() {
            return 1;
        }
    }
    // Git never opens an excluded directory, so no `!` rule brings back
    // what is inside: say so rather than show an empty listing unexplained.
    if !dupes && from_file.is_none() && opts.filter.as_ref().is_some_and(|f| f.gitignore) {
        for p in paths
            .iter()
            .filter(|p| absolute(p).is_ok_and(|a| du_filter::git_ignores(&a)))
        {
            let shown = crate::util::err_path(p);
            eprintln!("{prog}: git ignores '{shown}'; --gitignore leaves out all of it");
        }
    }
    if (trash_dupes || link_dupes || clone_dupes) && !dupes {
        eprintln!("{prog}: --trash-dupes, --link-dupes and --clone-dupes go with --dupes");
        return 2;
    }
    if dupes {
        set_text_units(prog, si, si_flag, raw_bytes, true);
        let act = if trash_dupes {
            DupesAct::Trash
        } else if link_dupes {
            DupesAct::Link
        } else if clone_dupes {
            DupesAct::Clone
        } else {
            DupesAct::List
        };
        if trash_dupes && trash_pins.len() > 1 {
            eprintln!("{prog}: --trash-dupes puts into one trash; give at most one --trash-dir");
            return 2;
        }
        let mut put_flags: Vec<String> = Vec::new();
        if home_only {
            put_flags.push("--home-only".into());
        }
        for pin in &trash_pins {
            let pin = crate::util::str_from_os(pin.clone().into_os_string());
            put_flags.push(format!("--trash-dir={pin}"));
        }
        let min_size = min_size.unwrap_or(1);
        let out = DupesOut {
            json,
            print0,
            act,
            put_flags,
        };
        return dupes_out(prog, &paths, &opts, min_size, out);
    }
    // Text sizes everywhere in this run, the explorer included; JSON,
    // snapshots and stacks stay in bytes. The explorer keeps its units
    // under a block size variable too, as it refuses --no-prefix.
    set_text_units(prog, si, si_flag, raw_bytes, !interactive);
    crate::util::set_raw_bytes(raw_bytes);
    if no_delete && !interactive {
        eprintln!("{prog}: --no-delete applies to --interactive; other modes never delete");
        return 2;
    }
    read_only |= no_delete;
    #[cfg(not(feature = "tui"))]
    let _ = read_only;
    if let Some(out) = &ncdu {
        if paths.len() > 1 {
            eprintln!("{prog}: {ncdu_flag} exports one PATH");
            return 2;
        }
        return ncdu_out(prog, &paths[0], out, ncdu_binary, &opts, now, strict);
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
        let snap = match crate::du_snap::read(file) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "{prog}: cannot read snapshot '{}': {e}",
                    crate::util::err_path(&file)
                );
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
        #[cfg(feature = "tui")]
        if interactive {
            // The explorer reads Ctrl-C as a key.
            crate::du_interrupt::uninstall();
            if crate::du_interrupt::pressed() {
                return 130;
            }
            return crate::du_tui::run_snapshot(prog, file, &snap, opts);
        }
        let known = snap.unreadable;
        let mut report = crate::du_snap::to_report(snap, opts.sort, opts.top);
        if let Some(floor) = min_size {
            floor_rows(&mut report, floor, opts.sort == SortKey::Apparent);
        }
        let trash = TrashSummary::default();
        if json {
            println!(
                "{}",
                report_json_with(&report, &trash, known, None, None, ratio)
            );
        } else {
            print_table(
                &format!("{} in {}", opts.sort.title("directories"), file.display()),
                &report.dirs,
                now,
                Columns {
                    count: false,
                    show_mtime,
                    share: want_share.then_some((&report.roots[..], apparent_share)),
                    show_share: relative,
                    bars,
                    graph,
                    reverse,
                    unread: &report.unread,
                    targets: None,
                    ratio,
                },
            );
            for t in report.roots.iter().filter(|_| !skip_total) {
                println!(
                    "Total {}: {} on disk, {} apparent{}",
                    crate::util::term_path(&t.path),
                    format_bytes(t.disk),
                    format_bytes(t.apparent),
                    ratio_note(ratio, t.disk, t.apparent)
                );
            }
        }
        warn_unread(prog, &report);
        if strict {
            match known {
                Some(0) => {}
                Some(_) => return 1,
                None => {
                    eprintln!(
                        "{prog}: '{}' records no unreadable count (an older snapshot or a gdu export); --strict cannot pass it",
                        crate::util::err_path(&file)
                    );
                    return 1;
                }
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
        return crate::du_tui::run(prog, &paths, opts, use_git, read_only, allow_filtered);
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
        let filter = opts.filter.as_deref().filter(|s| s.is_active());
        let filter = filter.map(|s| du_filter::Filter::new(s, &first));
        looks_cold(&first, filter.as_ref()).then(cold_threads)
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
            crate::du_snap::write_file(out, &report, &opts, now)
        };
        if let Err(e) = written {
            eprintln!(
                "{prog}: cannot write snapshot '{}': {e}",
                crate::util::err_path(&out)
            );
            return 1;
        }
        if out.as_os_str() != "-" {
            eprintln!(
                "{prog}: wrote {} to '{}'",
                plural(report.dirs.len() as u64, "directory", "directories"),
                crate::util::err_path(&out)
            );
        }
        // The snapshot keeps the count; --strict still reports the scan.
        if strict && report.unreadable > 0 {
            warn_unread(prog, &report);
            return 1;
        }
        return 0;
    }
    if !want_dirs {
        report.dirs.clear();
    }
    if !want_files {
        report.files.clear();
    }
    if let Some(floor) = min_size {
        floor_rows(&mut report, floor, opts.sort == SortKey::Apparent);
    }
    let prefetched = prefetch.map(du_git::Prefetch::finish);
    if use_git && !annotate_git_with(&mut report, prefetched.as_ref()) {
        eprintln!("{prog}: warning: git not found; rows carry no git state");
    }

    #[cfg(unix)]
    let trash = trash_on_scanned_devices(&trash_dirs, &report);
    #[cfg(windows)]
    let trash = recycle_bin_on_scanned_drives(&report);
    let held = if open_deleted {
        open_deleted_on(&report)
    } else {
        None
    };
    if open_deleted && held.is_none() {
        eprintln!(
            "{prog}: note: --open-deleted lists open files through /proc on Linux \
             and libproc on macOS, and this system gave no list; no line for it here"
        );
    }
    if (print0 || pick) && !allow_filtered {
        let held = hold_filtered(&mut report.dirs);
        if held > 0 {
            eprintln!(
                "{prog}: left out {} holding entries the filters hide; trashing one \
                 takes those too (--allow-filtered offers them)",
                plural(held as u64, "directory", "directories")
            );
        }
    }
    #[cfg(all(unix, feature = "tui"))]
    if pick {
        let title = report
            .roots
            .first()
            .map(|r| r.path.clone())
            .unwrap_or_default();
        return crate::put_tui::run_rows(prog, &title, pick_rows(&report));
    }
    let targets = show_targets.then(|| link_targets(&report));
    if json {
        let unreadable = Some(report.unreadable);
        let open = held.as_ref();
        let j = report_json_with(&report, &trash, unreadable, targets.as_ref(), open, ratio);
        println!("{j}");
    } else if print0 {
        let tty = crate::util::stdout_is_tty();
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
            let _ = out.write_all(&crate::util::path_line(&it.path, 0, tty));
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
        } else if tree.is_some() {
            let glyphs = if ascii { &TREE_ASCII } else { &TREE_UNICODE };
            let lines = tree_lines(
                &report,
                opts.sort,
                tree_top,
                dirs_first,
                glyphs,
                targets.as_ref(),
            );
            for line in lines {
                println!("{line}");
            }
        } else {
            let cols = Columns {
                count,
                show_mtime,
                share: want_share.then_some((&report.roots[..], apparent_share)),
                show_share: relative,
                bars,
                graph,
                reverse,
                unread: &report.unread,
                targets: targets.as_ref(),
                ratio,
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
                "Total {}{kind}: {} on disk{once}{unique}, {} apparent{}, \
                 {}{links}, {}, {}{excluded}",
                crate::util::term_path(&t.path),
                format_bytes(t.disk),
                format_bytes(t.apparent),
                ratio_note(ratio, t.disk, t.apparent),
                plural(t.files, "file", "files"),
                plural(t.dirs, "directory", "directories"),
                plural(t.inodes(), "inode", "inodes")
            );
        }
        if let Some(s) = report.shared {
            println!("{}", shared_line(s));
            if let Some(line) = encoded_line(s) {
                println!("{line}");
            }
        }
        if report.partial {
            println!("Partial scan: stopped by Ctrl-C, so sizes are lower bounds");
        }
        print_trash(&trash);
        if let Some(h) = &held {
            println!("{}", open_deleted_line(h));
        }
    }
    warn_unread(prog, &report);
    if !opts.shared_once {
        note_snapshots(prog, &report.snapshots);
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
    if strict && report.unreadable > 0 {
        return 1;
    }
    0
}

/// `--min-size`: drop the rows of `report` below `floor`, by disk use or,
/// with `apparent`, by apparent size; the totals stay. The rows came out
/// of a size ranking capped at `--top`, so the ones left are the largest
/// at or above the floor. A folder is at least as large as anything in
/// it, so every row that stays keeps the folder above it for `--tree`.
fn floor_rows(report: &mut Report, floor: u64, apparent: bool) {
    let size = |it: &Item| if apparent { it.apparent } else { it.disk };
    report.dirs.retain(|it| size(it) >= floor);
    report.files.retain(|it| size(it) >= floor);
}

/// Name the read-only btrfs snapshots a scan counted in full, up to
/// [`UNREAD_SHOWN`] of them, with the two ways to count them otherwise.
fn note_snapshots(prog: &str, snapshots: &[PathBuf]) {
    if snapshots.is_empty() {
        return;
    }
    eprintln!(
        "{prog}: note: {} counted in full \
         (--shared counts shared extents once, -x leaves them out)",
        plural(
            snapshots.len() as u64,
            "read-only btrfs snapshot",
            "read-only btrfs snapshots"
        )
    );
    for p in snapshots.iter().take(UNREAD_SHOWN) {
        eprintln!("{prog}: snapshot '{}'", crate::util::err_path(p));
    }
    if snapshots.len() > UNREAD_SHOWN {
        eprintln!(
            "{prog}: and {} more (--json lists them all)",
            snapshots.len() - UNREAD_SHOWN
        );
    }
}

/// Name the first unreadable entries of `report` on stderr, then how many
/// more there were.
fn warn_unread(prog: &str, report: &Report) {
    if report.unreadable > 0 {
        for (p, why) in report.unread.iter().take(UNREAD_SHOWN) {
            eprintln!("{prog}: cannot read '{}': {why}", crate::util::err_path(p));
        }
        let named = report.unread.len().min(UNREAD_SHOWN) as u64;
        let more = report.unreadable.saturating_sub(named);
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_name_matches_file_name() {
        // Below the PATH: a listed name pushed on its parent's path.
        for (dir, name) in [("/", "a"), ("r", "b"), ("r/s/", "c d"), ("./r", ".git")] {
            let mut path = PathBuf::from(dir);
            path.push(name);
            assert_eq!(dir_name(&path, 1), OsStr::new(name), "{path:?}");
        }
        // A name that is not UTF-8 and holds a backslash.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let name = OsStr::from_bytes(b"\xff\\x");
            let path = Path::new("r").join(name);
            assert_eq!(dir_name(&path, 1), name, "{path:?}");
        }
        // The PATH as given, where `file_name` decides.
        for path in ["/", ".", "..", "r/..", "r/", "r/.", "r"] {
            let path = Path::new(path);
            let want = path.file_name().unwrap_or_default();
            assert_eq!(dir_name(path, 0), want, "{path:?}");
        }
    }

    #[test]
    fn cut_path_leaves_what_pop_does() {
        for (dir, name) in [("/", "a"), ("r", "b"), ("r/s", "c d"), ("./r", "\u{e9}")] {
            let mut buf = PathBuf::from(dir);
            let len = buf.as_os_str().len();
            buf.push(name);
            let mut popped = buf.clone();
            popped.pop();
            cut_path(&mut buf, len);
            assert_eq!(buf, popped, "{dir:?} + {name:?}");
            assert_eq!(buf.as_os_str().len(), len);
        }
    }

    #[test]
    fn parts_are_taken_whole_into_an_empty_list() {
        let first = vec![1, 2, 3];
        let block = first.as_ptr();
        let mut all = Vec::new();
        take_or_extend(&mut all, Vec::new());
        take_or_extend(&mut all, first);
        assert_eq!(all.as_ptr(), block);
        take_or_extend(&mut all, vec![4]);
        take_or_extend(&mut all, Vec::new());
        assert_eq!(all, [1, 2, 3, 4]);
    }

    #[test]
    fn bars_fill_a_tenth_per_cell() {
        assert_eq!(bar(Some(0.5), GraphStyle::Hash), "#####     ");
        assert_eq!(bar(Some(1.0), GraphStyle::Hash), "##########");
        assert_eq!(bar(Some(0.04), GraphStyle::Hash), "          ");
        assert_eq!(bar(None, GraphStyle::Hash), "          ");
    }

    #[test]
    fn graph_styles_split_a_cell_into_halves_or_eighths() {
        // 0.55 of 10 cells: 5.5 cells.
        let half = bar(Some(0.55), GraphStyle::HalfBlock);
        assert_eq!(half, "\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{258c}    ");
        // 0.4375 of 80 eighths is 35: four full cells and three eighths.
        let eighth = bar(Some(0.4375), GraphStyle::EighthBlock);
        assert_eq!(eighth, "\u{2588}\u{2588}\u{2588}\u{2588}\u{258d}     ");
        // Whole cells need no partial block.
        assert_eq!(bar(Some(0.3), GraphStyle::EighthBlock).chars().count(), 10);
        assert_eq!(GraphStyle::parse("half-block"), Some(GraphStyle::HalfBlock));
        assert_eq!(GraphStyle::parse("blocks"), None);
    }

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
            filtered_out: 0,
        }
    }

    /// A link row reads `PATH -> TARGET` and gains `"target"` last in its
    /// JSON; a row with no target keeps its text and JSON as they were.
    #[test]
    fn link_rows_show_their_target_and_others_keep_their_shape() {
        let t = Path::new("real.bin");
        assert_eq!(with_target("link".into(), Some(t)), "link -> real.bin");
        assert_eq!(with_target("own.bin".into(), None), "own.bin");
        let it = item("/w/link", 4096, 10);
        let plain = item_json(&it);
        assert_eq!(item_json_with(&it, None, false), plain);
        let j = item_json_with(&it, Some(Path::new("q\"t")), false);
        assert!(j.starts_with(&plain[..plain.len() - 1]), "{j}");
        assert!(j.ends_with(",\"target\":\"q\\\"t\"}"), "{j}");
        let mut targets = Targets::new();
        targets.insert(PathBuf::from("/w/link"), PathBuf::from("real.bin"));
        assert_eq!(target_of(Some(&targets), &it), Some(t));
        assert_eq!(target_of(None, &it), None);
        assert_eq!(target_of(Some(&targets), &item("/w/other", 1, 1)), None);
    }

    /// Under -L a file link and a directory link are rows with targets; a
    /// file reached through the linked directory is a row but no link, and
    /// the tree and JSON show the two targets only when asked.
    #[cfg(unix)]
    #[test]
    fn follow_scan_names_the_targets_of_link_rows_only() {
        use std::os::unix::fs::symlink;
        let base = std::env::temp_dir().join(format!("rtrash-du-targets-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let base = fs::canonicalize(&base).unwrap();
        let (root, outside) = (base.join("root"), base.join("outside"));
        fs::create_dir_all(root.join("plain")).unwrap();
        fs::create_dir_all(outside.join("dir")).unwrap();
        fs::write(outside.join("big.bin"), vec![1u8; 200_000]).unwrap();
        fs::write(outside.join("dir/inner.bin"), vec![2u8; 150_000]).unwrap();
        fs::write(root.join("plain/own.bin"), vec![3u8; 100_000]).unwrap();
        symlink(outside.join("big.bin"), root.join("big-link")).unwrap();
        symlink(outside.join("dir"), root.join("dir-link")).unwrap();
        let opts = ScanOpts {
            follow: true,
            all_dirs: true,
            ..ScanOpts::default()
        };
        let r = scan(std::slice::from_ref(&root), &opts).unwrap();
        let targets = link_targets(&r);
        assert_eq!(
            targets.get(&root.join("big-link")),
            Some(&outside.join("big.bin"))
        );
        assert_eq!(
            targets.get(&root.join("dir-link")),
            Some(&outside.join("dir"))
        );
        assert_eq!(targets.len(), 2, "{targets:?}");
        let inner = root.join("dir-link/inner.bin");
        assert!(r.files.iter().any(|f| f.path == inner), "{:?}", r.files);
        let lines = tree_lines(&r, SortKey::Disk, 20, false, &TREE_ASCII, Some(&targets));
        let arrow = format!("big-link -> {}", outside.join("big.bin").display());
        assert!(lines.iter().any(|l| l.ends_with(&arrow)), "{lines:?}");
        let plain = tree_lines(&r, SortKey::Disk, 20, false, &TREE_ASCII, None);
        assert!(plain.iter().all(|l| !l.contains(" -> ")), "{plain:?}");
        let none = TrashSummary::default();
        let j = report_json_with(&r, &none, Some(0), Some(&targets), None, false);
        assert_eq!(j.matches("\"target\":").count(), 2, "{j}");
        assert!(!report_json(&r, &none).contains("\"target\""));
        let _ = fs::remove_dir_all(&base);
    }

    /// Without -L a symlink counts its own size either way; only
    /// `list_links` (the explorer) lists it, as a row tagged [`LINK`].
    #[cfg(unix)]
    #[test]
    fn links_are_rows_only_when_the_scan_lists_links() {
        use std::os::unix::fs::{symlink, MetadataExt};
        let base = std::env::temp_dir().join(format!("rtrash-du-links-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("dir")).unwrap();
        let base = fs::canonicalize(&base).unwrap();
        fs::write(base.join("big.bin"), vec![1u8; 100_000]).unwrap();
        symlink("big.bin", base.join("file-link")).unwrap();
        symlink("dir", base.join("dir-link")).unwrap();
        let opts = ScanOpts {
            all_dirs: true,
            top: 50,
            ..ScanOpts::default()
        };
        let plain = scan(std::slice::from_ref(&base), &opts).unwrap();
        let links = ScanOpts {
            list_links: true,
            ..opts
        };
        let listed = scan(std::slice::from_ref(&base), &links).unwrap();
        assert!(
            plain.files.iter().all(|f| f.artefact.is_none()),
            "{:?}",
            plain.files
        );
        assert_eq!(plain.files.len(), 1, "{:?}", plain.files);
        assert_eq!(plain.roots[0].disk, listed.roots[0].disk);
        assert_eq!(plain.roots[0].files, listed.roots[0].files);
        let mut rows: Vec<_> = listed
            .files
            .iter()
            .filter(|f| f.artefact == Some(LINK))
            .collect();
        rows.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(rows.len(), 2, "{:?}", listed.files);
        for (row, name) in rows.iter().zip(["dir-link", "file-link"]) {
            let m = fs::symlink_metadata(base.join(name)).unwrap();
            assert_eq!(row.path, base.join(name));
            assert!(!row.is_dir);
            assert_eq!(row.apparent, m.len());
            assert_eq!(row.disk, m.blocks() * 512);
        }
        let _ = fs::remove_dir_all(&base);
    }

    /// A directory of several reads ([`LISTED_BATCH`]) is stat-ed while
    /// it is read, and comes out as one listing would give it: the same
    /// totals with and without rows, each file once (a hard link across
    /// reads too), the biggest files under their own names, and its
    /// folder found by its name.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_directory_stat_while_it_is_read_counts_as_one_listing() {
        use std::os::unix::fs::MetadataExt;
        let base = std::env::temp_dir().join(format!("rtrash-du-reads-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("big").join("sub")).unwrap();
        let base = fs::canonicalize(&base).unwrap();
        let big = base.join("big");
        for i in 0..20 * LISTED_BATCH + 100 {
            fs::write(big.join(format!("f{i:06}")), vec![1u8; i % 7]).unwrap();
        }
        for i in 0..5 {
            fs::write(big.join(format!("large-{i}")), vec![2u8; 20_000 + i]).unwrap();
        }
        fs::write(big.join("sub").join("x"), vec![3u8; 5_000]).unwrap();
        fs::hard_link(big.join("large-0"), big.join("zz-link")).unwrap();
        let own = |p: &Path| fs::symlink_metadata(p).unwrap();
        let mut want_disk: u64 = [&base, &big, &big.join("sub")]
            .iter()
            .map(|p| own(p).blocks() * 512)
            .sum();
        let (mut want_apparent, mut want_files) = (0u64, 0u64);
        let mut inos = HashSet::new();
        let listed = fs::read_dir(&big).unwrap().map(|e| e.unwrap().path());
        for p in listed.chain([big.join("sub").join("x")]) {
            let m = own(&p);
            if m.is_dir() || !inos.insert(m.ino()) {
                continue;
            }
            want_disk += m.blocks() * 512;
            want_apparent += m.len();
            want_files += 1;
        }
        let rows = ScanOpts {
            all_dirs: true,
            top: 5,
            ..ScanOpts::default()
        };
        let plain = ScanOpts {
            top: 0,
            ..ScanOpts::default()
        };
        for opts in [&rows, &plain] {
            let r = scan(std::slice::from_ref(&base), opts).unwrap();
            let t = &r.roots[0];
            assert_eq!(
                (t.disk, t.apparent, t.files),
                (want_disk, want_apparent, want_files)
            );
        }
        let r = scan(std::slice::from_ref(&base), &rows).unwrap();
        let mut top: Vec<PathBuf> = r.files.iter().map(|f| f.path.clone()).collect();
        top.sort();
        let want: Vec<PathBuf> = (0..5).map(|i| big.join(format!("large-{i}"))).collect();
        assert_eq!(top, want, "{:?}", r.files);
        let sub = r.dirs.iter().find(|d| d.path == big.join("sub"));
        let sub = sub.unwrap_or_else(|| panic!("{:?}", r.dirs));
        assert_eq!(sub.apparent, 5_000, "{sub:?}");
        let _ = fs::remove_dir_all(&base);
    }

    /// A folder of more than [`WIDE_DIR`] files that tie on size. On one
    /// thread its stat jobs cut their candidates in [`stat_chunks`], on
    /// four (on Linux) in [`list_and_stat`] while it is read. Either way
    /// the list is the larger file and then the tied ones by name, as if
    /// one job had seen them all.
    #[test]
    fn a_wide_directory_lists_its_best_files_in_the_one_order() {
        let base = std::env::temp_dir().join(format!("rtrash-du-best-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        // The scan reports `absolute` of the root: the parent is canonical,
        // and on Windows the `\\?\` prefix is left off. Expected paths use
        // that same root.
        let root = absolute(&base).unwrap();
        let n = 2 * LISTED_BATCH + 3 * WIDE_DIR;
        // Made in reverse, so the first files made are not the answer.
        for i in (0..n).rev() {
            fs::write(root.join(format!("f{i:05}")), b"same").unwrap();
        }
        fs::write(root.join("zz-big"), vec![1u8; 100_000]).unwrap();
        let opts = ScanOpts {
            top: 4,
            ..ScanOpts::default()
        };
        let mut want = vec![root.join("zz-big")];
        want.extend((0..3).map(|i| root.join(format!("f{i:05}"))));
        for threads in [1, 4] {
            let r = scan_on(std::slice::from_ref(&root), &opts, None, Some(threads)).unwrap();
            let got: Vec<PathBuf> = r.files.iter().map(|f| f.path.clone()).collect();
            assert_eq!(got, want, "{threads} threads");
            assert_eq!(r.roots[0].files, n as u64 + 1, "{threads} threads");
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cold_walks_get_three_threads_per_cpu_up_to_64() {
        let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
        assert_eq!(cold_threads(), (cpus * 3).min(64));
        assert!(!looks_cold(Path::new("/nonexistent-rtrash-probe"), None));
    }

    #[test]
    fn the_cold_probe_opens_no_excluded_folder() {
        let root = Path::new("/home/u");
        let spec = du_filter::Spec {
            patterns: vec!["Library".into()],
            ..Default::default()
        };
        let f = du_filter::Filter::new(&spec, root);
        assert!(!probe_opens(Some(&f), root, c"Library"));
        assert!(probe_opens(Some(&f), root, c"Music"));
        assert!(probe_opens(None, root, c"Library"));
        let spec = du_filter::Spec {
            gitignore: true,
            ..Default::default()
        };
        let f = du_filter::Filter::new(&spec, root);
        assert!(!probe_opens(Some(&f), root, c"Music"));
    }

    /// A file bind-mounted below a PATH turns the inode-number reuse off,
    /// and so does a mount point that cannot be stat-ed; mounts of
    /// directories, the PATH's own mount and mounts elsewhere do not.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_file_mount_below_the_root_turns_listed_inodes_off() {
        let table = "/dev/sda1 / ext4 rw 0 0\n\
                     /dev/sda1 /data/scan ext4 rw 0 0\n\
                     /dev/sdb1 /data/scan/sub ext4 rw 0 0\n\
                     /dev/sda1 /data/scan/sub/file.txt ext4 rw,bind 0 0\n\
                     /dev/sdc1 /data/scan\\040two ext4 rw 0 0\n\
                     /dev/sdd1 /data/other/dir ext4 rw 0 0\n";
        let dirs = |mp: &Path| match mp.to_str() {
            Some("/data/scan/sub/file.txt") => Some(false),
            Some("/data/other/dir") => None,
            _ => Some(true),
        };
        let usable = |root: &str| no_file_mounts_below(table, Path::new(root), dirs);
        assert!(!usable("/data/scan"), "a file mount below");
        assert!(!usable("/data/scan/sub"), "a file mount below");
        assert!(!usable("/"), "a file mount below");
        assert!(usable("/data/scan/sub/file.txt"), "its own mount only");
        // "/data/scan two" is not below "/data/scan two/x" nor "/data/scan".
        assert!(usable("/data/scan two/x"));
        assert!(usable("/srv"), "no mount below");
        assert!(
            !usable("/data/other"),
            "a mount point that cannot be stat-ed"
        );
        assert!(usable("/data/other/dir"), "its own mount only");
    }

    #[test]
    fn ino_hash_spreads_close_ids() {
        use std::hash::BuildHasher;
        let h = InoHash::default();
        let (mut low, mut top) = (HashSet::new(), HashSet::new());
        for ino in 0..1024u64 {
            let x = h.hash_one((7u64, ino));
            low.insert(x & 1023);
            top.insert(x >> 57);
        }
        // A table of 1024 buckets indexes by the low bits, and its tags
        // are the top 7 bits.
        assert!(low.len() > 512, "{} low buckets", low.len());
        assert!(top.len() > 120, "{} tags", top.len());
        assert_ne!(h.hash_one((7u64, 1u64)), h.hash_one((1u64, 7u64)));
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
    fn same_joined_matches_exactly_where_join_does() {
        let name = |s: &[u8]| std::ffi::CString::new(s).unwrap();
        let cases: [(&str, &str, &[u8], bool); 8] = [
            ("/r/a/x", "/r/a", b"x", true),
            ("/r/b/x", "/r/a", b"x", false),  // another parent
            ("/r/a/y", "/r/a", b"x", false),  // another name
            ("/r/a/x", "/r/a/", b"x", true),  // a trailing slash on the dir
            ("/r/a/x", "/r/./a", b"x", true), // a `.` inside the dir
            ("/r/ax", "/r/a", b"x", false),   // no separator in between
            ("/r/a/x", "/r", b"a/x", false),  // a name is one component
            ("x", "", b"x", true),            // a relative PATH
        ];
        for (full, dir, n, want) in cases {
            let (full, dir, n) = (Path::new(full), Path::new(dir), name(n));
            assert_eq!(same_joined(full, dir, &n), want, "{full:?} {dir:?} {n:?}");
            // `join` agrees on every name a listing can give.
            if !n.to_bytes().contains(&b'/') {
                assert_eq!(
                    full == dir.join(du_fs::os(&n)),
                    want,
                    "{full:?} {dir:?} {n:?}"
                );
            }
        }
        // Names are bytes: a name that is not UTF-8 matches only itself.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let raw = name(b"f\xff");
            let full = Path::new("/r").join(std::ffi::OsStr::from_bytes(b"f\xff"));
            assert!(same_joined(&full, Path::new("/r"), &raw));
            assert!(!same_joined(&full, Path::new("/r"), &name(b"f\xfe")));
        }
    }

    #[test]
    fn wide_directories_split_into_a_few_jobs_a_worker() {
        assert_eq!(wide_chunk(200_000, 8), 6_250);
        assert_eq!(200_000usize.div_ceil(wide_chunk(200_000, 8)), 32);
        assert_eq!(wide_chunk(257, 8), WIDE_CHUNK, "never under the floor");
        assert_eq!(wide_chunk(200_000, 1), 50_000);
        assert_eq!(wide_chunk(1_000, 0), 250, "no workers counts as one");
    }

    /// 600 sightings of 200 linked files, three each, the first of each
    /// at its largest path: sighting `k` is in `dir(k)` as `name(k)`.
    #[allow(clippy::type_complexity)]
    fn sightings() -> (
        Vec<du_fs::Stat>,
        impl Fn(usize) -> PathBuf,
        impl Fn(usize) -> std::ffi::CString,
    ) {
        let ino = |k: usize| 1 + (k as u64 * 7) % 200;
        let sts = (0..600)
            .map(|k| du_fs::Stat {
                mode: 0o100_000, // regular file
                size: ino(k),
                blocks: 1,
                nlink: 3,
                ino: ino(k),
                dev: 5,
                mtime: 0,
            })
            .collect();
        let dir = |k: usize| PathBuf::from(format!("/r/d{}", 2 - k / 200));
        let name = move |k: usize| std::ffi::CString::new(format!("f{}", ino(k))).unwrap();
        (sts, dir, name)
    }

    #[test]
    fn shard_runs_match_a_sort_by_shard_and_entry() {
        let stat = |ino: u64, dev: u64| du_fs::Stat {
            mode: 0o100_000, // regular file
            size: 1,
            blocks: 1,
            nlink: 2,
            ino,
            dev,
            mtime: 0,
        };
        for n in [0usize, 1, 2, 17, 600, 5000] {
            // Repeated files and two devices, as links in one batch are.
            let sts: Vec<du_fs::Stat> = (0..n)
                .map(|k| stat(1 + (k as u64 * 7919) % 997, 5 + (k % 2) as u64))
                .collect();
            let mut want: Vec<(usize, usize)> = sts
                .iter()
                .enumerate()
                .map(|(k, st)| (LinkSet::shard(st.dev, st.ino), k))
                .collect();
            want.sort_unstable();
            assert_eq!(shard_runs(&sts), want, "{n} files");
        }
    }

    #[test]
    fn a_batch_of_links_counts_as_the_files_one_by_one() {
        let (sts, _, _) = sightings();
        let one = LinkSet::new();
        let want: Vec<bool> = sts.iter().map(|st| one.first_link(st)).collect();
        let batch = LinkSet::new();
        // Two batches, each with files met more than once in it.
        let (a, b) = sts.split_at(250);
        let mut got = batch.first_links(a);
        got.extend(batch.first_links(b));
        assert_eq!(got, want);
        assert_eq!(got.iter().filter(|f| **f).count(), 200);
    }

    #[test]
    fn claims_seen_in_a_batch_settle_as_one_by_one() {
        let (sts, dir, name) = sightings();
        let names: Vec<std::ffi::CString> = (0..sts.len()).map(name).collect();
        let one = LinkClaims::new(5);
        let want_first: Vec<bool> = sts
            .iter()
            .enumerate()
            .map(|(k, st)| one.claim(st, &dir(k), &names[k]))
            .collect();
        // One batch per directory, as a stat job files them.
        let batch = LinkClaims::new(5);
        let mut got_first = Vec::new();
        for d in 0..3 {
            let r = d * 200..(d + 1) * 200;
            let first = batch.claim_all(&sts[r.clone()], &dir(r.start), |k| {
                names[r.start + k].as_c_str()
            });
            got_first.extend(first);
        }
        assert_eq!(got_first, want_first);
        // Each file counts once, at its first sighting.
        assert_eq!(got_first.iter().filter(|f| **f).count(), 200);
        assert!(got_first[..200].iter().all(|f| *f));
        let paths = |c: LinkClaims| -> std::collections::BTreeMap<(u64, u64), PathBuf> {
            let fixed = c.settled().expect("first sightings at the largest paths");
            assert_eq!(fixed.len(), 200);
            let found = fixed.get(&(5, 1)).map(|c| c.smallest.to_path_buf());
            assert_eq!(found, Some(PathBuf::from("/r/d0/f1")));
            let dev = fixed.dev;
            fixed
                .shards
                .into_iter()
                .flat_map(|s| {
                    let here = s.here.into_iter().map(move |(ino, c)| ((dev, ino), c));
                    here.chain(s.other.into_iter().flat_map(|m| *m))
                })
                .map(|(id, c)| (id, c.smallest.into_path_buf()))
                .collect()
        };
        let (want, got) = (paths(one), paths(batch));
        assert_eq!(got, want);
        assert_eq!(got.len(), 200);
        assert!(got.values().all(|p| p.starts_with("/r/d0")), "{got:?}");
    }

    /// A claim gives back the stat it was made from under its key, and
    /// its path is the joined one, with no room left over.
    #[test]
    fn a_claim_keeps_its_path_and_stat_in_the_room_they_need() {
        let st = du_fs::Stat {
            mode: 0o100644,
            size: 4097,
            blocks: 16,
            nlink: 2,
            ino: 77,
            dev: 5,
            mtime: -3,
        };
        let name = |s: &str| std::ffi::CString::new(s).unwrap();
        for (dir, n) in [
            ("/r/a", "x"),
            ("/r/a/", "x"),
            ("/", "x"),
            ("", "x"),
            ("r", "e000123"),
        ] {
            let (dir, n) = (Path::new(dir), name(n));
            let p = joined_path(dir, &n);
            assert_eq!(*p, *dir.join(du_fs::os(&n)), "{dir:?} {n:?}");
            let c = Claim::new(&st, p);
            assert!(c.counted_there);
            assert_eq!(c.stat((st.dev, st.ino)), st);
        }
        #[cfg(target_pointer_width = "64")]
        assert!(
            std::mem::size_of::<Claim>() <= 56,
            "{}",
            std::mem::size_of::<Claim>()
        );
    }

    #[test]
    fn claims_that_counted_at_the_smallest_path_settle() {
        let (sts, dir, name) = sightings();
        let c = LinkClaims::new(5);
        // Smallest paths first: every count falls where it belongs.
        for k in (0..sts.len()).rev() {
            c.claim(&sts[k], &dir(k), &name(k));
        }
        assert!(c.settled().is_none());
    }

    #[test]
    fn root_skip_leaves_what_is_below_the_root() {
        let roots: &[&str] = if cfg!(windows) {
            &[r"C:\", r"C:\r", r"C:\r\", r"\\?\C:\r"]
        } else {
            &["/", "/r", "/r/", "/r/./"]
        };
        let bytes = |p: &Path| p.as_os_str().as_encoded_bytes().to_vec();
        for root in roots {
            let root = Path::new(root);
            let skip = root_skip(root);
            assert_eq!(bytes(root).get(skip..).unwrap_or_default(), b"", "{root:?}");
            let mut p = root.to_path_buf();
            p.push("a");
            assert_eq!(&bytes(&p)[skip..], b"a", "{root:?}");
            p.push("b");
            let want = bytes(&Path::new("a").join("b"));
            assert_eq!(bytes(&p)[skip..], want[..], "{root:?}");
        }
    }

    #[test]
    fn joined_first_orders_as_the_joined_path_does() {
        let name = |s: &str| std::ffi::CString::new(s).unwrap();
        let cases: [(&str, &str, &str, bool); 10] = [
            ("/r/a", "x", "/r/a/y", true),
            ("/r/a", "y", "/r/a/x", false),
            ("/r/a", "x", "/r/a/x", false), // the same path is not first
            ("/r/a", "x", "/r/b", true),
            ("/r/b", "x", "/r/a/z", false),
            ("/r", "a", "/r/a/x", true), // a parent comes before its children
            ("/r/a/", "x", "/r/a/y", true), // a trailing slash on the dir
            ("/r/./a", "y", "/r/a/x", false), // a `.` inside the dir
            ("/r/a", "x", "/r/a.b/x", true), // by component, not by byte
            ("", "x", "y", true),        // a relative PATH
        ];
        for (dir, n, other, want) in cases {
            let (dir, n, other) = (Path::new(dir), name(n), Path::new(other));
            assert_eq!(
                joined_first(dir, &n, other),
                want,
                "{dir:?} {n:?} {other:?}"
            );
            assert_eq!(
                credits_first(&dir.join(du_fs::os(&n)), other),
                want,
                "{dir:?} {n:?}"
            );
        }
    }

    /// The byte order of below-root paths is the order by component, for
    /// bytes on either side of the separator, prefixes of a name, a name
    /// long enough to be read eight bytes at a time, and an empty or root
    /// dir, so a claim lands where it did before.
    #[cfg(unix)]
    #[test]
    fn plain_joined_first_orders_as_joined_first_does() {
        use std::os::unix::ffi::OsStrExt;
        let parts: [&[u8]; 8] = [
            b"a",
            b"ab",
            b"a-",
            b"a.b",
            b" !",
            b"\x01",
            b"a\xff",
            b"abcdefgh-ij",
        ];
        let mut paths: Vec<Vec<u8>> = vec![Vec::new()];
        for p in parts {
            paths.push(p.to_vec());
            for q in parts {
                paths.push([p, &b"/"[..], q].concat());
            }
        }
        let rooted: Vec<Vec<u8>> = paths.iter().map(|p| [&b"/"[..], &p[..]].concat()).collect();
        paths.extend(rooted);
        let mut firsts = 0;
        for d in &paths {
            for n in parts {
                let n = std::ffi::CString::new(n).unwrap();
                for o in &paths {
                    let (dir, other) = (Path::new(OsStr::from_bytes(d)), OsStr::from_bytes(o));
                    let first = plain_joined_first(dir, &n, Path::new(other));
                    assert_eq!(
                        first,
                        joined_first(dir, &n, Path::new(other)),
                        "{dir:?} {n:?} {other:?}"
                    );
                    firsts += usize::from(first);
                }
            }
        }
        // Both answers come up, so the order is not one-sided.
        let all = paths.len() * parts.len() * paths.len();
        assert!(
            firsts > all / 4 && firsts < all * 3 / 4,
            "{firsts} of {all}"
        );
    }

    /// The same over deeper paths: up to five names of up to twenty
    /// bytes, many of them starting alike so the eight-byte reads meet the
    /// first difference at every offset, and the second path often the
    /// first one cut short, varied at its end or gone on further. A fixed
    /// seed draws them, so a failure repeats.
    #[cfg(unix)]
    #[test]
    fn plain_joined_first_orders_as_joined_first_does_on_deeper_paths() {
        use std::os::unix::ffi::OsStrExt;
        // xorshift64: a number below `n`.
        fn below(seed: &mut u64, n: usize) -> usize {
            *seed ^= *seed << 13;
            *seed ^= *seed >> 7;
            *seed ^= *seed << 17;
            (*seed % n as u64) as usize
        }
        // A name of one listing: never `.` or `..`, and no separator.
        fn name(seed: &mut u64) -> Vec<u8> {
            const STEM: &[u8] = b"abcdefghijklmnopqrst";
            const BYTES: &[u8] = b"a-. \x01\xff0";
            loop {
                let len = 1 + below(seed, 20);
                let mut n = STEM[..below(seed, len + 1)].to_vec();
                while n.len() < len {
                    n.push(BYTES[below(seed, BYTES.len())]);
                }
                if n != b"." && n != b".." {
                    break n;
                }
            }
        }
        let join = |rooted: bool, names: &[Vec<u8>]| {
            let p = names.join(&b'/');
            if rooted {
                [&b"/"[..], &p[..]].concat()
            } else {
                p
            }
        };
        let mut seed = 0x5eed_0313;
        let (cases, mut firsts, mut long) = (20_000, 0, 0);
        for _ in 0..cases {
            let rooted = below(&mut seed, 2) == 0;
            let k = below(&mut seed, 5);
            let mut names: Vec<Vec<u8>> = (0..k).map(|_| name(&mut seed)).collect();
            let d = join(rooted, &names);
            let n = name(&mut seed);
            // The other path: a start of the joined one, its last name
            // perhaps replaced, then more names, five at most.
            names.push(n.clone());
            names.truncate(below(&mut seed, k + 2));
            if !names.is_empty() && below(&mut seed, 3) == 0 {
                *names.last_mut().unwrap() = name(&mut seed);
            }
            for _ in 0..below(&mut seed, 6 - names.len()) {
                names.push(name(&mut seed));
            }
            let o = join(rooted != (below(&mut seed, 8) == 0), &names);
            let n = std::ffi::CString::new(n).unwrap();
            let (dir, other) = (
                Path::new(OsStr::from_bytes(&d)),
                Path::new(OsStr::from_bytes(&o)),
            );
            let first = plain_joined_first(dir, &n, other);
            assert_eq!(
                first,
                joined_first(dir, &n, other),
                "{dir:?} {n:?} {other:?}"
            );
            firsts += usize::from(first);
            long += usize::from(shared_len(&d, &o) >= 16);
        }
        // Both answers come up, and a quarter of the pairs share sixteen
        // bytes or more, so the eight-byte reads are exercised.
        assert!(
            firsts > cases / 4 && firsts < cases * 3 / 4 && long > cases / 4,
            "{firsts} first and {long} long of {cases}"
        );
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

    /// 34(h): one order for every ranking: the larger key first, then
    /// the smaller name.
    #[test]
    fn rank_puts_the_larger_first_and_ties_by_name() {
        use std::cmp::Ordering::{Greater, Less};
        assert_eq!(rank((5u64, "b"), (3, "a")), Less);
        assert_eq!(rank((3u64, "a"), (5, "b")), Greater);
        assert_eq!(rank((4u64, "a"), (4, "b")), Less);
        assert!(rank((4u64, "a"), (4, "a")).is_eq());
        let mut rows = [item("/m", 4, 4), item("/z", 9, 9), item("/a", 4, 4)];
        rows.sort_by(|a, b| rank_items(a, b, SortKey::Disk));
        let order: Vec<&Path> = rows.iter().map(|r| r.path.as_path()).collect();
        assert_eq!(order, [Path::new("/z"), Path::new("/a"), Path::new("/m")]);
    }

    /// 34(h): the table (TopN) keeps the rows that `rank_items` puts
    /// first, in that order, whatever order the scan met them in.
    #[test]
    fn the_table_keeps_the_rows_that_rank_puts_first() {
        let rows = [
            item("/r/c", 7, 7),
            item("/r/a", 7, 7),
            item("/r/b", 9, 9),
            item("/r/d", 1, 1),
        ];
        let mut top = TopN::new(3, SortKey::Disk);
        top.merge_vec(rows.iter().rev().cloned().collect());
        let table: Vec<PathBuf> = top.into_vec().into_iter().map(|i| i.path).collect();
        let mut sorted = rows.clone();
        sorted.sort_by(|a, b| rank_items(a, b, SortKey::Disk));
        let ranked: Vec<PathBuf> = sorted.into_iter().take(3).map(|i| i.path).collect();
        assert_eq!(table, ranked);
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
            encoded: false,
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
        let whole = crate::du_extent::Span {
            dev: 1,
            encoded: false,
            start: 0,
            end: 20480,
        };
        assert_eq!(ranges, vec![whole]);
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
            encoded: 0,
        });
        assert!(
            s.contains(&format_bytes(1 << 20)) && s.contains(&format_bytes(3 << 20)),
            "{s}"
        );
    }

    #[test]
    fn only_a_scan_with_compressed_extents_notes_them() {
        assert_eq!(encoded_line(SharedTotals::default()), None);
        let s = SharedTotals {
            encoded: 3 << 20,
            ..SharedTotals::default()
        };
        let line = encoded_line(s).unwrap_or_default();
        assert!(line.contains(&format_bytes(3 << 20)), "{line}");
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

    #[test]
    fn hold_filtered_keeps_whole_directories() {
        let mut held = item("/w/held", 9, 9);
        held.is_dir = true;
        held.filtered_out = 3;
        let mut whole = item("/w/whole", 8, 8);
        whole.is_dir = true;
        let mut dirs = vec![held, whole];
        assert_eq!(hold_filtered(&mut dirs), 1);
        let left: Vec<&Path> = dirs.iter().map(|d| d.path.as_path()).collect();
        assert_eq!(left, [Path::new("/w/whole")]);
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
            snapshots: Vec::new(),
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

    /// A directory adds its own st_blocks to disk use and nothing to the
    /// apparent size, as du(1) counts it: none for an inline or tmpfs
    /// directory, several blocks for a wide one. One that cannot be read
    /// still adds its own blocks.
    #[cfg(unix)]
    #[test]
    fn directories_count_their_own_blocks_as_du_does() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let root = std::env::temp_dir().join(format!("rtrash-du-dirblk-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::create_dir_all(root.join("locked")).unwrap();
        fs::create_dir_all(root.join("wide")).unwrap();
        let root = fs::canonicalize(&root).unwrap();
        fs::write(root.join("f.bin"), vec![5u8; 1000]).unwrap();
        for i in 0..400 {
            let name = format!("an-entry-with-a-long-name-to-grow-the-directory-{i:04}");
            fs::write(root.join("wide").join(name), b"").unwrap();
        }
        let own = |p: &Path| fs::symlink_metadata(p).unwrap().blocks() * 512;
        let mut want = own(&root) + own(&root.join("f.bin"));
        for d in ["empty", "locked", "wide"] {
            want += own(&root.join(d));
        }
        let locked = root.join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let opts = ScanOpts {
            all_dirs: true,
            top: 50,
            ..ScanOpts::default()
        };
        let r = scan(std::slice::from_ref(&root), &opts);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let r = r.unwrap();
        assert_eq!(r.roots[0].disk, want, "{:?}", r.roots[0]);
        assert_eq!(r.roots[0].apparent, 1000, "{:?}", r.roots[0]);
        assert_eq!(r.roots[0].dirs, 4, "{:?}", r.roots[0]);
        let wide = r.dirs.iter().find(|d| d.path == root.join("wide"));
        let wide = wide.unwrap_or_else(|| panic!("{:?}", r.dirs));
        assert_eq!(wide.disk, own(&root.join("wide")), "{wide:?}");
        assert_eq!(wide.apparent, 0, "{wide:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn size_sums_stop_at_the_top_instead_of_wrapping() {
        let half = u64::MAX / 2 + 1;
        assert_eq!(size_sum([half, half]), u64::MAX);
        assert_eq!(size_sum([half, half, 7]), u64::MAX);
        assert_eq!(size_sum([1, 2, 3]), 6);
        assert_eq!(size_sum(std::iter::empty()), 0);
    }

    /// 35(e): no row of the conflict table refuses its own flag or names
    /// a flag twice, and the message lists the others as prose.
    #[test]
    fn the_conflict_table_names_each_flag_once_per_row() {
        for &(flag, _, others) in MODE_CONFLICTS {
            assert!(!others.contains(&flag), "{flag}");
            let mut sorted = others.to_vec();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), others.len(), "{flag}");
        }
        assert_eq!(or_list(&["-i"]), "-i");
        assert_eq!(or_list(&["-o", "-f"]), "-o or -f");
        assert_eq!(or_list(&["-o", "-f", "-i"]), "-o, -f or -i");
        let given = |flag: &str| flag == "--ncdu" || flag == "--json";
        let refusal = mode_conflict(&given, "--ncdu-binary");
        let want = "--ncdu-binary writes an ncdu export; it takes no -o, -f, --stacks, --json, \
                    --print0, --pick, -i, --unique, --shared or --types";
        assert_eq!(refusal.as_deref(), Some(want));
        assert_eq!(
            mode_conflict(&|flag: &str| flag == "--json", "--ncdu"),
            None
        );
    }

    /// 35(d), gdu #294 and #265: RATIO is disk use as a percent of
    /// apparent size, rounded, and the JSON key is the same quotient to
    /// three places. A row with no apparent size has none; plain output
    /// keeps its shape.
    #[test]
    fn ratio_is_disk_use_over_apparent_size() {
        assert_eq!(ratio_text(1, 2), "50%");
        assert_eq!(ratio_text(1, 3), "33%");
        assert_eq!(ratio_text(2, 3), "67%");
        assert_eq!(ratio_text(4096, 4096), "100%");
        assert_eq!(ratio_text(4096, 1000), "410%");
        assert_eq!(ratio_text(4096, 10), ">999%");
        assert_eq!(ratio_text(0, 0), "-");
        assert_eq!(ratio_text(u64::MAX, u64::MAX), "100%");
        assert_eq!(ratio_key(true, 1, 3), ",\"ratio\":0.333");
        assert_eq!(ratio_key(true, 4096, 1000), ",\"ratio\":4.096");
        assert_eq!(ratio_key(true, 0, 0), ",\"ratio\":null");
        assert_eq!(ratio_key(false, 1, 3), "");
        assert_eq!(ratio_note(true, 1, 2), ", ratio 50%");
        assert_eq!(ratio_note(false, 1, 2), "");
        let it = item("/w/packed", 4096, 16384);
        let j = item_json_with(&it, None, true);
        assert!(j.ends_with(",\"ratio\":0.250}"), "{j}");
        let j = item_json_with(&it, Some(Path::new("t")), true);
        assert!(j.ends_with(",\"ratio\":0.250,\"target\":\"t\"}"), "{j}");
    }
}
