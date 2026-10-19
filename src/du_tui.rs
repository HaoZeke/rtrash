//! `rtrash du -i`: an interactive disk-usage explorer in the spirit of
//! ncdu. Each directory view comes from a one-level `du` scan, kept in a
//! cache while you move around. Deleting moves rows to the trash, and `u`
//! brings the last deletion back.
//!
//! Scans run in the background: a directory opens at once with a quick
//! listing of its entries (files with their sizes, directories pending),
//! and the full view replaces it when the scan finishes. You can move,
//! open other directories and go back meanwhile.
//!
//! A delete runs in the background too, as in dua 2.45: its rows go to
//! the trash one at a time on a thread of their own, each leaves the
//! views as it goes, and the status line counts the rows and bytes left,
//! and the bytes copied of a row that crosses file systems. A row that
//! stays says why in the status line, and `e` lists every one. Marks
//! wait while it runs, as do the keys that would act on its rows.
//! Quitting stops the delete: a copy in progress ends and its part
//! copied goes, so that row stays whole where it was, with the rows not
//! yet started.
//!
//! Given several directories (`du -i a b`, as `dua i a b`), the explorer
//! opens on a view of its own above them, one row per directory, kept in
//! the cache under the empty path, which names no directory.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::du::{self, Item, ScanControl, ScanOpts, SortKey};
use crate::tui_binds::{Action, Keymap};
use crate::tui_list;
use crate::tui_select::Selection;
use crate::tui_term;
use crate::util::format_bytes;

/// One scanned directory: its rows (children) and totals.
#[derive(Debug, Clone)]
struct View {
    rows: Vec<Item>,
    disk: u64,
    apparent: u64,
    /// Cursor row when the view was left, so going back lands on it.
    cursor: usize,
    /// A quick listing shown while the scan runs: directory sizes are
    /// still unknown and nothing can be deleted yet.
    scanning: bool,
    /// The scan was stopped (q or Ctrl-C): sizes are lower bounds.
    partial: bool,
    /// Rows the name filter hides (current view only).
    stash: Vec<Item>,
    /// The entries this view's scan could not read, as the report keeps
    /// them; rows on or above one get the table's `unreadable` or
    /// `incomplete` mark.
    unread: Vec<(PathBuf, String)>,
    /// Entries below this view's directory that the filters left out.
    filtered_out: u64,
    /// The directory records of this view's scan, for `E` to write as a
    /// `du -o` snapshot (files left out, as a snapshot holds none).
    /// `None` for a quick listing, the view above several roots, and once
    /// a delete, an undo or a rescan below has changed the sizes.
    scan: Option<Arc<du::Report>>,
}

/// A scan running in the background.
struct Job {
    control: Arc<ScanControl>,
    rx: mpsc::Receiver<io::Result<Scanned>>,
    /// A rescan (`r`) of a view whose sizes the parents already hold.
    refresh: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Browse,
    Confirm,
    Help,
    /// Details of the cursor row (`i`).
    Info,
    /// Typing a name filter (`/`).
    Filter,
    /// Every mark, from all directories (`m`).
    Marks,
    /// Typing the file name `E` writes this view's snapshot to.
    Export,
}

/// What the explorer orders rows by: a size, count or age key, or name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Order {
    Key(SortKey),
    Name,
}

/// The row order, flipped by `reverse`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Sorting {
    order: Order,
    reverse: bool,
}

impl Sorting {
    fn of(sort: SortKey) -> Self {
        Sorting {
            order: Order::Key(sort),
            reverse: false,
        }
    }

    fn label(self) -> String {
        let what = match self.order {
            Order::Key(SortKey::Disk) => "disk use",
            Order::Key(SortKey::Apparent) => "apparent size",
            Order::Key(SortKey::Items) => "items",
            Order::Key(SortKey::Mtime) => "newest change",
            Order::Name => "name",
        };
        if self.reverse {
            format!("{what}, reversed")
        } else {
            what.to_string()
        }
    }
}

/// Rows trashed by one delete, with what is needed to put them back.
type UndoGroup = Vec<(bin::Ticket, Item)>;

/// One row a delete got to: its ticket, or why it stayed.
type Trashed = (Item, Result<bin::Ticket, String>);

/// A delete in progress: the rows go to the trash on a thread of their
/// own (inline in tests) and come back one at a time through `rx`.
struct Deleting {
    rx: mpsc::Receiver<Trashed>,
    /// The stop flag and the bytes copied of the row in hand.
    watch: Watch,
    /// Rows not yet done, in order, with their apparent sizes; none of
    /// them can be opened meanwhile. The first is the row in hand.
    pending: Vec<(PathBuf, u64)>,
    total: usize,
    /// Disk use of the rows not yet done.
    left: u64,
    group: UndoGroup,
    /// Rows that stayed, with why.
    failed: Vec<(PathBuf, String)>,
    /// Rows done and bytes copied when the status line last showed them.
    shown: (usize, u64),
}

/// What a delete thread answers to: `stop` ends it, inside a copy across
/// file systems too, and `copied` counts the bytes of the row in hand.
#[derive(Clone, Default)]
struct Watch {
    stop: Arc<AtomicBool>,
    copied: Arc<AtomicU64>,
}

/// Trash `items` one at a time and send each with its ticket, until all
/// are done, `watch.stop` is set, or nobody listens any more. A row the
/// stop ended partway is not sent: its copy was undone, so it stays.
fn trash_rows(
    prog: &str,
    items: Vec<Item>,
    pin: Option<&PathBuf>,
    watch: &Watch,
    tx: &mpsc::Sender<Trashed>,
) {
    for item in items {
        if watch.stop.load(Ordering::Relaxed) {
            return;
        }
        let done = bin::trash(prog, &item.path, pin, watch);
        // Before the send, so the count never shows against the next row.
        watch.copied.store(0, Ordering::Relaxed);
        if done.is_err() && watch.stop.load(Ordering::Relaxed) {
            return;
        }
        if tx.send((item, done)).is_err() {
            return;
        }
    }
}

/// Trash and untrash for the browser: the FreeDesktop trash on Unix, the
/// Recycle Bin on Windows.
#[cfg(unix)]
mod bin {
    use std::path::{Path, PathBuf};

    use crate::put;

    /// What a delete left behind, enough to undo it.
    pub type Ticket = put::Trashed;

    /// Trash `path`, or say why it stays. A copy across file systems
    /// counts its bytes into `watch` and ends, undone, once `watch.stop`
    /// is set.
    pub fn trash(
        prog: &str,
        path: &Path,
        pin: Option<&PathBuf>,
        watch: &super::Watch,
    ) -> Result<Ticket, String> {
        let opts = put::Opts {
            recursive: true,
            force: true,
            quiet: true,
            trash_pin: pin.cloned(),
            ..put::Opts::default()
        };
        let scoped = crate::interrupt::Watch {
            stop: watch.stop.clone(),
            copied: watch.copied.clone(),
        };
        let (done, said) =
            put::heard(|| crate::interrupt::watched(&scoped, || put::put_one(prog, path, &opts)));
        match done {
            Ok(Some(ticket)) => Ok(ticket),
            Ok(None) => Err("gone, with no trash record to undo".into()),
            Err(_) => Err(put::why(prog, &said)),
        }
    }

    pub fn untrash(t: &Ticket) -> bool {
        let Ok(dir) = crate::trashdir::TrashDir::from_root(&t.trash_dir) else {
            return false;
        };
        // The record as it reads now, and only if this run's put wrote
        // it: an item that took the name since stays in the trash. The
        // restore checks the record again under the trash lock.
        let Some(entry) = crate::list::entry_named(&dir, &t.name)
            .filter(|e| e.batch.as_deref() == Some(crate::info::batch_id()))
        else {
            return false;
        };
        crate::restore::restore_one_to(&entry, &t.original, false).is_ok()
    }
}

#[cfg(windows)]
mod bin {
    use std::path::{Path, PathBuf};

    /// The original path; undo restores the newest Recycle Bin item from it.
    pub type Ticket = PathBuf;

    /// The Recycle Bin call cannot stop partway, so `watch` waits for it.
    pub fn trash(
        _prog: &str,
        path: &Path,
        _pin: Option<&PathBuf>,
        _watch: &super::Watch,
    ) -> Result<Ticket, String> {
        crate::allow_roots::trash_delete(path)
            .map(|()| path.to_path_buf())
            .map_err(|e| format!("cannot trash '{}': {e}", path.display()))
    }

    pub fn untrash(original: &Ticket) -> bool {
        if original.symlink_metadata().is_ok() {
            return false;
        }
        let Ok(items) = crate::win_recycle::list_items() else {
            return false;
        };
        let Some(item) = items
            .into_iter()
            .filter(|i| i.original_path() == *original)
            .max_by_key(|i| i.time_deleted)
        else {
            return false;
        };
        // Only the item as listed: another may have taken its name.
        crate::win_recycle::recheck_item(&item).is_ok()
            && crate::win_recycle::restore_payload(std::path::Path::new(&item.id), original).is_ok()
    }
}

struct App {
    prog: String,
    /// The top view: the one root, or the empty path above several.
    root: PathBuf,
    /// The directories given, each a row of the top view when several.
    roots: Vec<PathBuf>,
    cur: PathBuf,
    opts: ScanOpts,
    git: bool,
    /// Trash root for deletes (tests pin one; `None` uses put's choice).
    trash_pin: Option<PathBuf>,
    /// `RTRASH_ALLOW_ROOTS` as read at start (tests set it directly); a
    /// delete with any row outside it trashes nothing.
    scope: Option<Vec<PathBuf>>,
    cache: HashMap<PathBuf, View>,
    list_state: ListState,
    sel: Selection,
    /// Marks left behind in other views, by path: they stay marked while
    /// the user browses elsewhere and join the next trash, print or copy.
    away: BTreeMap<PathBuf, Item>,
    /// The cursor in the marks panel.
    marks_at: usize,
    mode: Mode,
    status: String,
    quit: bool,
    viewport_rows: usize,
    keys: Keymap,
    undo: Vec<UndoGroup>,
    /// `--no-delete`: browse only; `d` and `u` explain instead of acting.
    read_only: bool,
    /// `du -f FILE -i`: the snapshot or ncdu export being browsed. Every
    /// view comes from it, read-only, and nothing is walked ([`App::walk`]).
    snapshot: Option<PathBuf>,
    /// `--allow-filtered`: a directory that holds entries the filters
    /// left out may be trashed, and they go with it.
    allow_filtered: bool,
    /// Scan in the background (the terminal session); tests scan inline.
    background: bool,
    jobs: HashMap<PathBuf, Job>,
    /// The delete in progress, if any (one at a time).
    deleting: Option<Deleting>,
    /// Rows the last delete left in place, with why (`e` lists them).
    failures: Vec<(PathBuf, String)>,
    /// The current row order (`s`, `n`, `C` and `M` change it).
    sorting: Sorting,
    /// The size `s` sorts by and rows show: apparent under `--apparent`.
    size_key: SortKey,
    /// Lines of the info panel while it is open.
    info: Vec<String>,
    /// Name filter on the current view (`/`); empty shows every row.
    filter: String,
    /// Whether `.` hides names that start with a dot, in every view.
    hide_dots: bool,
    /// What each row shows of its share of the view (`%` cycles it).
    share: Share,
    /// Whether the bars use a log scale (`L` switches it, ncdu #75).
    log_bars: bool,
    /// Paths `p` prints on stdout once the explorer has closed.
    print_on_exit: Vec<PathBuf>,
    /// Text `y` sends to the terminal clipboard on the next draw.
    clipboard: Option<String>,
    /// The file name typed after `E`.
    export_to: String,
    /// What gets the terminal on the next turn of the loop (`b`, `v`).
    hand_off: Option<HandOff>,
    /// Program and arguments `o` opens a row with (tests set their own).
    opener: Vec<std::ffi::OsString>,
    /// The room left on the file system of a view, by the view's path
    /// (ncdu #77): read when the view changes, and again after a
    /// delete, an undo or a rescan. `None` inside when it can't be read.
    free: Option<(PathBuf, Option<crate::space::Space>)>,
    /// How the free space of a view is read: [`free_space`], or a fixed
    /// value in tests, so a title never depends on the host's disk.
    free_of: fn(&Path) -> Option<crate::space::Space>,
}

/// A program the explorer hands the terminal to.
#[derive(Debug, PartialEq, Eq)]
enum HandOff {
    /// A shell in this directory (`b`).
    Shell(PathBuf),
    /// The pager on this file (`v`).
    Pager(PathBuf),
}

/// The size a row shows and its bar measures: apparent size under
/// `--apparent`, disk use otherwise.
fn key_of(it: &Item, apparent: bool) -> u64 {
    if apparent {
        it.apparent
    } else {
        it.disk
    }
}

/// What a row counts toward the view's items, as the title's total
/// does: a file or link is one, and a directory is itself and all below.
fn row_items(it: &Item) -> u64 {
    if it.is_dir {
        it.items.saturating_add(1)
    } else {
        1
    }
}

/// Everything below a view's directory, as ncdu's Items count: each row,
/// and for a directory also what it holds. Rows the name filter or `.`
/// hides count too, as they do in the view's size.
fn view_items(v: &View) -> u64 {
    du::size_sum(v.rows.iter().chain(&v.stash).map(row_items))
}

/// Rows in the order of `s`: largest, most items or newest change first,
/// or by name from A; `reverse` flips it.
fn sort_rows(rows: &mut [Item], s: Sorting) {
    rows.sort_by(|a, b| row_order(a, b, s));
}

/// Where `a` goes against `b` in the order of `s`.
fn row_order(a: &Item, b: &Item, s: Sorting) -> std::cmp::Ordering {
    let name = |it: &Item| {
        it.path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase()
    };
    let o = match s.order {
        Order::Key(k) => du::rank_items(a, b, k),
        Order::Name => name(a).cmp(&name(b)).then_with(|| a.path.cmp(&b.path)),
    };
    if s.reverse {
        o.reverse()
    } else {
        o
    }
}

/// Put `it` among `rows` before the first row it sorts ahead of, leaving
/// the others where they are. A delete or an undo changes sizes without
/// reordering a view (ncdu #237): the rows stay where the user saw them
/// until a rescan or a sort key orders them again.
fn insert_in_order(rows: &mut Vec<Item>, it: Item, s: Sorting) {
    let at = rows
        .iter()
        .position(|r| row_order(&it, r, s).is_lt())
        .unwrap_or(rows.len());
    rows.insert(at, it);
}

fn is_trash(it: &Item) -> bool {
    it.artefact == Some("trash")
}

/// Whether `it` is a symlink row: shown with `@`, never entered, and
/// trashed as the link alone.
fn is_link(it: &Item) -> bool {
    it.artefact == Some(du::LINK)
}

/// Scan `dir` one level deep: every child directory, file and symlink
/// with its total, git state and artefact kind.
fn scan_view(dir: &Path, opts: &ScanOpts, git: bool, sorting: Sorting) -> io::Result<View> {
    let o = ScanOpts {
        top: usize::MAX,
        // A count ranks no files in a listing; the explorer needs them all
        // and orders rows itself.
        sort: SortKey::Disk,
        max_depth: Some(1),
        all_dirs: true,
        artefacts_only: false,
        older_cutoff: None,
        newer_cutoff: None,
        list_links: true,
        ..opts.clone()
    };
    let mut report = du::scan(&[dir.to_path_buf()], &o)?;
    if git {
        du::annotate_git(&mut report);
    }
    let files = std::mem::take(&mut report.files);
    let scan = Some(Arc::new(report.clone()));
    let (disk, apparent, filtered_out) = report
        .roots
        .first()
        .map(|r| (r.disk, r.apparent, r.excluded))
        .unwrap_or((0, 0, 0));
    let mut rows: Vec<Item> = report
        .dirs
        .into_iter()
        .chain(files)
        .filter(|it| it.path != dir)
        .collect();
    sort_rows(&mut rows, sorting);
    Ok(View {
        rows,
        disk,
        apparent,
        cursor: 0,
        scanning: false,
        partial: report.partial,
        stash: Vec::new(),
        unread: report.unread,
        filtered_out,
        scan,
    })
}

/// What one `read_dir` shows of `dir` before its scan ends: every entry,
/// files with their own sizes, directories first by name with sizes to
/// come. Filters are left to the scan.
fn quick_view(dir: &Path) -> io::Result<View> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for e in std::fs::read_dir(dir)?.flatten() {
        let Ok(m) = e.metadata() else { continue };
        let mtime = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs() as i64);
        let is_dir = m.is_dir();
        #[cfg(unix)]
        let disk = std::os::unix::fs::MetadataExt::blocks(&m).saturating_mul(512);
        // The listing gives only the length, which is not disk use: an
        // online-only OneDrive file (gdu #688), a sparse or a compressed
        // one takes less. The scan counts the allocated size; so does
        // this row, so it keeps its size when the scan's arrive.
        #[cfg(not(unix))]
        let disk = if is_dir {
            0
        } else {
            crate::du_fs::lstat(&e.path()).map_or(m.len(), |s| s.sizes().1)
        };
        let it = Item {
            path: e.path(),
            is_dir,
            apparent: if is_dir { 0 } else { m.len() },
            disk: if is_dir { 0 } else { disk },
            mtime,
            items: u64::from(!is_dir),
            artefact: m.file_type().is_symlink().then_some(du::LINK),
            repos: Vec::new(),
            git: None,
            unique: None,
            filtered_out: 0,
        };
        if is_dir {
            dirs.push(it);
        } else {
            files.push(it);
        }
    }
    dirs.sort_by(|a, b| a.path.cmp(&b.path));
    sort_rows(&mut files, Sorting::of(SortKey::Disk));
    let disk = du::size_sum(files.iter().map(|f| f.disk));
    let apparent = du::size_sum(files.iter().map(|f| f.apparent));
    dirs.extend(files);
    Ok(View {
        rows: dirs,
        disk,
        apparent,
        cursor: 0,
        scanning: true,
        partial: false,
        stash: Vec::new(),
        unread: Vec::new(),
        filtered_out: 0,
        scan: None,
    })
}

/// Whether `dir` is the view above several roots.
fn is_top_of_roots(dir: &Path) -> bool {
    dir.as_os_str().is_empty()
}

/// A finished scan: the view, and the views of the roots below it when
/// it is the view of several roots (each scanned on the way).
type Scanned = (View, Vec<(PathBuf, View)>);

/// The view of several roots: one row per root, from a one-level scan of
/// each, whose views come back too so that opening a root costs nothing.
/// A file linked from two roots counts in both.
fn scan_roots(
    roots: &[PathBuf],
    opts: &ScanOpts,
    git: bool,
    sorting: Sorting,
) -> io::Result<Scanned> {
    let mut rows = Vec::new();
    let mut below = Vec::new();
    let mut partial = false;
    let mut unread = Vec::new();
    for root in roots {
        let view = scan_view(root, opts, git, sorting)?;
        unread.extend(view.unread.iter().cloned());
        let mut repos: Vec<PathBuf> = Vec::new();
        for r in view.rows.iter().flat_map(|r| &r.repos) {
            if !repos.contains(r) {
                repos.push(r.clone());
            }
        }
        let mtime = std::fs::metadata(root)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs() as i64);
        rows.push(Item {
            path: root.clone(),
            is_dir: true,
            apparent: view.apparent,
            disk: view.disk,
            mtime: view.rows.iter().map(|r| r.mtime).fold(mtime, i64::max),
            items: du::size_sum(view.rows.iter().map(|r| {
                if r.is_dir {
                    r.items.saturating_add(1)
                } else {
                    1
                }
            })),
            artefact: root
                .file_name()
                .and_then(|n| du::artefact_kind(&n.to_string_lossy(), |c| root.join(c).exists())),
            repos,
            git: None,
            unique: None,
            filtered_out: view.filtered_out,
        });
        partial |= view.partial;
        below.push((root.clone(), view));
        if opts.control.as_ref().is_some_and(|c| c.stopped()) {
            partial = true;
            break;
        }
    }
    if git {
        let queries: Vec<crate::du_git::RowQuery<'_>> = rows
            .iter()
            .map(|it| crate::du_git::RowQuery {
                path: &it.path,
                is_dir: true,
                repos: &it.repos,
            })
            .collect();
        if let Some(states) = crate::du_git::classify_all_with(&queries, None) {
            for (it, s) in rows.iter_mut().zip(states) {
                it.git = s;
            }
        }
    }
    let disk = du::size_sum(rows.iter().map(|r| r.disk));
    let apparent = du::size_sum(rows.iter().map(|r| r.apparent));
    sort_rows(&mut rows, sorting);
    let filtered_out = du::size_sum(rows.iter().map(|r| r.filtered_out));
    let view = View {
        rows,
        disk,
        apparent,
        cursor: 0,
        scanning: false,
        partial,
        stash: Vec::new(),
        unread,
        filtered_out,
        scan: None,
    };
    Ok((view, below))
}

/// One report of the one-level scans of several roots, in their order:
/// what `du -o FILE --max-depth=1 A B` scans, since du walks each PATH
/// alone too (a file linked from two roots counts in both). Every field
/// is named, with no `..` fill, so a field added to the report fails to
/// build here until it is joined too.
fn join_reports(parts: &[Arc<du::Report>]) -> du::Report {
    let mut unread: Vec<(PathBuf, String)> = parts
        .iter()
        .flat_map(|p| p.unread.iter().cloned())
        .collect();
    // As du keeps them: sorted, the first UNREAD_KEPT.
    unread.sort();
    unread.truncate(du::UNREAD_KEPT);
    let mut snapshots: Vec<PathBuf> = parts
        .iter()
        .flat_map(|p| p.snapshots.iter().cloned())
        .collect();
    snapshots.sort();
    du::Report {
        roots: parts.iter().flat_map(|p| p.roots.iter().cloned()).collect(),
        dirs: parts.iter().flat_map(|p| p.dirs.iter().cloned()).collect(),
        files: Vec::new(),
        unreadable: du::size_sum(parts.iter().map(|p| p.unreadable)),
        unread,
        too_deep: du::size_sum(parts.iter().map(|p| p.too_deep)),
        depth_limit: parts
            .iter()
            .map(|p| p.depth_limit)
            .min()
            .unwrap_or(du::MAX_DEPTH),
        shared: None,
        partial: parts.iter().any(|p| p.partial),
        types: Vec::new(),
        snapshots,
    }
}

/// The view of several roots before their scan ends: every root, its
/// size to come.
fn quick_roots(roots: &[PathBuf]) -> View {
    let rows = roots
        .iter()
        .map(|r| Item {
            path: r.clone(),
            is_dir: true,
            apparent: 0,
            disk: 0,
            mtime: 0,
            items: 0,
            artefact: None,
            repos: Vec::new(),
            git: None,
            unique: None,
            filtered_out: 0,
        })
        .collect();
    View {
        rows,
        disk: 0,
        apparent: 0,
        cursor: 0,
        scanning: true,
        partial: false,
        stash: Vec::new(),
        unread: Vec::new(),
        filtered_out: 0,
        scan: None,
    }
}

/// The views of a snapshot or ncdu export (`du -f FILE -i`), all built
/// at once from its records: one per directory, and above two or more
/// roots one of them all. A view's rows are the directories recorded
/// below it. A snapshot holds no files, so a view's size, its record's,
/// can be more than its rows add up to.
fn snapshot_views(snap: &crate::du_snap::Snapshot, sorting: Sorting) -> HashMap<PathBuf, View> {
    let blank = |disk: u64, apparent: u64| View {
        rows: Vec::new(),
        disk,
        apparent,
        cursor: 0,
        scanning: false,
        partial: false,
        stash: Vec::new(),
        unread: Vec::new(),
        filtered_out: 0,
        scan: None,
    };
    let mut views: HashMap<PathBuf, View> = HashMap::new();
    for r in &snap.roots {
        views.insert(r.path.clone(), blank(r.disk, r.apparent));
    }
    let dirs = || snap.dirs.iter().filter(|d| d.is_dir);
    for d in dirs() {
        views
            .entry(d.path.clone())
            .or_insert_with(|| blank(d.disk, d.apparent));
    }
    let is_root = |p: &Path| snap.roots.iter().any(|r| r.path == p);
    for d in dirs().filter(|d| !is_root(&d.path)) {
        if let Some(v) = d.path.parent().and_then(|p| views.get_mut(p)) {
            v.rows.push(d.clone());
        }
    }
    for (dir, v) in views.iter_mut() {
        sort_rows(&mut v.rows, sorting);
        v.unread = snap
            .unread
            .iter()
            .filter(|(p, _)| p.starts_with(dir))
            .cloned()
            .collect();
    }
    if snap.roots.len() > 1 {
        let mut rows: Vec<Item> = snap
            .roots
            .iter()
            .map(|r| Item {
                path: r.path.clone(),
                is_dir: true,
                apparent: r.apparent,
                disk: r.disk,
                mtime: 0,
                // The root's files and directories, itself left out.
                items: r.files.saturating_add(r.dirs).saturating_sub(1),
                artefact: r.artefact,
                repos: Vec::new(),
                git: None,
                unique: None,
                filtered_out: 0,
            })
            .collect();
        sort_rows(&mut rows, sorting);
        let mut top = blank(
            du::size_sum(rows.iter().map(|r| r.disk)),
            du::size_sum(rows.iter().map(|r| r.apparent)),
        );
        top.rows = rows;
        top.unread = snap.unread.clone();
        views.insert(PathBuf::new(), top);
    }
    views
}

/// [`scan_view`] of `dir`, or [`scan_roots`] for the view above them.
fn scan_any(
    dir: &Path,
    roots: &[PathBuf],
    opts: &ScanOpts,
    git: bool,
    sorting: Sorting,
) -> io::Result<Scanned> {
    if is_top_of_roots(dir) {
        scan_roots(roots, opts, git, sorting)
    } else {
        scan_view(dir, opts, git, sorting).map(|v| (v, Vec::new()))
    }
}

/// [`quick_view`] of `dir`, or [`quick_roots`] for the view above them.
fn quick_any(dir: &Path, roots: &[PathBuf]) -> io::Result<View> {
    if is_top_of_roots(dir) {
        Ok(quick_roots(roots))
    } else {
        quick_view(dir)
    }
}

/// The view above `dir`: the parent directory, the view of the roots
/// for one of several roots, and none at the top.
fn up(roots: &[PathBuf], dir: &Path) -> Option<PathBuf> {
    if is_top_of_roots(dir) {
        return None;
    }
    if roots.iter().any(|r| r == dir) {
        return (roots.len() > 1).then(PathBuf::new);
    }
    dir.parent().map(Path::to_path_buf)
}

/// What lies below a directory, as the explorer carries it up the chain:
/// its disk use, apparent size and item count. Every change moves all of
/// them at once through [`carry`], so a total added here cannot be left
/// behind (round 254 nearly kept a stale scan, and round 255 found item
/// counts that deletes never touched).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Totals {
    disk: u64,
    apparent: u64,
    items: u64,
}

impl Totals {
    /// What the row `it` adds below its parent: itself and, for a
    /// directory, everything in it.
    fn of_row(it: &Item) -> Self {
        Totals {
            disk: it.disk,
            apparent: it.apparent,
            items: row_items(it),
        }
    }

    /// What lies below the view's directory.
    fn of_view(v: &View) -> Self {
        Totals {
            disk: v.disk,
            apparent: v.apparent,
            items: view_items(v),
        }
    }
}

/// `v` with `from` taken out and `to` put in, stopping at 0 and at the top.
fn shift(v: u64, from: u64, to: u64) -> u64 {
    v.saturating_sub(from).saturating_add(to)
}

/// Set the row that leads to a directory to the directory's totals.
fn put_row(row: &mut Item, t: Totals) {
    row.disk = t.disk;
    row.apparent = t.apparent;
    row.items = t.items;
}

/// Carry a change below `dir`, from `from` to `to`, up the chain to the
/// top: each view's own sizes (its item count is the sum of its rows,
/// [`view_items`]) and the row that leads to each directory in the view
/// above it. Each field moves on its own, so a disk use that grows while
/// the apparent size shrinks moves both the right way. Every view on the
/// way drops its kept scan, which no longer says what it holds.
fn carry(
    cache: &mut HashMap<PathBuf, View>,
    roots: &[PathBuf],
    dir: &Path,
    from: Totals,
    to: Totals,
) {
    let mut d = dir.to_path_buf();
    loop {
        if let Some(v) = cache.get_mut(&d) {
            v.disk = shift(v.disk, from.disk, to.disk);
            v.apparent = shift(v.apparent, from.apparent, to.apparent);
            v.scan = None;
        }
        let Some(parent) = up(roots, &d) else {
            break;
        };
        if let Some(v) = cache.get_mut(&parent) {
            if let Some(row) = v.rows.iter_mut().chain(&mut v.stash).find(|r| r.path == d) {
                let was = Totals {
                    disk: row.disk,
                    apparent: row.apparent,
                    items: row.items,
                };
                put_row(
                    row,
                    Totals {
                        disk: shift(was.disk, from.disk, to.disk),
                        apparent: shift(was.apparent, from.apparent, to.apparent),
                        items: shift(was.items, from.items, to.items),
                    },
                );
            }
        }
        d = parent;
    }
}

impl App {
    /// An explorer whose first view is scanned before it returns.
    #[cfg(test)]
    fn new(prog: &str, root: PathBuf, opts: ScanOpts, git: bool) -> io::Result<Self> {
        Self::new_in(prog, vec![root], opts, git)
    }

    /// [`App::new`] on one or several roots.
    #[cfg(test)]
    fn new_in(prog: &str, roots: Vec<PathBuf>, opts: ScanOpts, git: bool) -> io::Result<Self> {
        let top = Self::top_of(&roots);
        let (view, below) = scan_any(&top, &roots, &opts, git, Sorting::of(opts.sort))?;
        let mut app = Self::with_view(prog, roots, opts, git, view);
        app.take_below(below);
        Ok(app)
    }

    /// An explorer that opens on a quick listing and scans in the background.
    fn start(prog: &str, roots: Vec<PathBuf>, opts: ScanOpts, git: bool) -> io::Result<Self> {
        let top = Self::top_of(&roots);
        let view = quick_any(&top, &roots)?;
        let mut app = Self::with_view(prog, roots, opts, git, view);
        app.background = true;
        app.spawn(top, false);
        Ok(app)
    }

    /// A read-only explorer over the snapshot or ncdu export `snap`, read
    /// from `file`: every view comes from its records, and nothing is
    /// walked. Errors name why it cannot be browsed.
    fn from_snapshot(
        prog: &str,
        file: &Path,
        snap: &crate::du_snap::Snapshot,
        opts: ScanOpts,
    ) -> Result<Self, String> {
        let roots: Vec<PathBuf> = snap.roots.iter().map(|r| r.path.clone()).collect();
        if roots.is_empty() {
            return Err("records no directory to browse".into());
        }
        for (i, a) in roots.iter().enumerate() {
            if let Some(b) = roots[..i]
                .iter()
                .find(|b| a.starts_with(b) || b.starts_with(a))
            {
                return Err(format!(
                    "records '{}' and '{}', one inside the other",
                    b.display(),
                    a.display()
                ));
            }
        }
        let mut views = snapshot_views(snap, Sorting::of(opts.sort));
        let top = Self::top_of(&roots);
        let view = views.remove(&top).ok_or("records no top directory")?;
        let mut app = Self::with_view(prog, roots, opts, false, view);
        app.cache.extend(views);
        app.read_only = true;
        app.snapshot = Some(file.to_path_buf());
        app.status = format!(
            "browsing '{}' · read-only · {} entries",
            crate::util::term_path(file),
            app.rows().len()
        );
        Ok(app)
    }

    /// The key of the top view of `roots`.
    fn top_of(roots: &[PathBuf]) -> PathBuf {
        match roots {
            [one] => one.clone(),
            _ => PathBuf::new(),
        }
    }

    fn with_view(prog: &str, roots: Vec<PathBuf>, opts: ScanOpts, git: bool, view: View) -> Self {
        let root = Self::top_of(&roots);
        let mut cache = HashMap::new();
        let n = view.rows.len();
        cache.insert(root.clone(), view);
        let mut list_state = ListState::default();
        if n > 0 {
            list_state.select(Some(0));
        }
        let keys = Keymap::load();
        let size_key = match opts.sort {
            SortKey::Apparent => SortKey::Apparent,
            _ => SortKey::Disk,
        };
        App {
            sorting: Sorting::of(opts.sort),
            size_key,
            info: Vec::new(),
            filter: String::new(),
            hide_dots: false,
            share: Share::Bar,
            log_bars: false,
            print_on_exit: Vec::new(),
            clipboard: None,
            export_to: String::new(),
            hand_off: None,
            opener: tui_term::opener(),
            prog: prog.to_string(),
            cur: root.clone(),
            root,
            roots,
            opts,
            git,
            trash_pin: None,
            scope: crate::allow_roots::roots(),
            cache,
            list_state,
            sel: Selection::new(),
            away: BTreeMap::new(),
            marks_at: 0,
            mode: Mode::Browse,
            status: String::new(),
            quit: false,
            viewport_rows: 10,
            keys,
            undo: Vec::new(),
            read_only: false,
            snapshot: None,
            allow_filtered: false,
            background: false,
            jobs: HashMap::new(),
            deleting: None,
            failures: Vec::new(),
            free: None,
            free_of: free_space,
        }
    }

    /// Why nothing may be walked: the explorer is on a snapshot.
    fn no_walk(&self) -> Option<io::Error> {
        self.snapshot.as_ref().map(|f| {
            io::Error::other(format!(
                "browsing the snapshot '{}'; nothing is scanned",
                crate::util::term_path(f)
            ))
        })
    }

    /// Scan `dir` inline. Every walk the explorer makes goes through
    /// here, [`App::quick`] or [`App::spawn_with`], which refuse on a
    /// snapshot (precheck explorer-walk check).
    fn walk(&self, dir: &Path) -> io::Result<Scanned> {
        if let Some(e) = self.no_walk() {
            return Err(e);
        }
        scan_any(dir, &self.roots, &self.opts, self.git, self.sorting)
    }

    /// The quick listing of `dir`, as [`App::walk`] guards it.
    fn quick(&self, dir: &Path) -> io::Result<View> {
        if let Some(e) = self.no_walk() {
            return Err(e);
        }
        quick_any(dir, &self.roots)
    }

    /// Start a background scan of `dir`.
    fn spawn(&mut self, dir: PathBuf, refresh: bool) {
        self.spawn_with(dir, refresh, Arc::new(ScanControl::default()));
    }

    fn spawn_with(&mut self, dir: PathBuf, refresh: bool, control: Arc<ScanControl>) {
        if let Some(e) = self.no_walk() {
            self.status = e.to_string();
            return;
        }
        let opts = ScanOpts {
            control: Some(control.clone()),
            ..self.opts.clone()
        };
        let (tx, rx) = mpsc::channel();
        let (d, git, sorting) = (dir.clone(), self.git, self.sorting);
        let roots = self.roots.clone();
        std::thread::spawn(move || {
            let _ = tx.send(scan_any(&d, &roots, &opts, git, sorting));
        });
        self.jobs.insert(
            dir,
            Job {
                control,
                rx,
                refresh,
            },
        );
    }

    /// Take in every finished scan. Returns whether any finished.
    fn poll_jobs(&mut self) -> bool {
        let done: Vec<(PathBuf, io::Result<Scanned>)> = self
            .jobs
            .iter()
            .filter_map(|(dir, job)| match job.rx.try_recv() {
                Ok(r) => Some((dir.clone(), r)),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some((dir.clone(), Err(io::Error::other("scan ended early"))))
                }
            })
            .collect();
        let any = !done.is_empty();
        for (dir, result) in done {
            let refresh = self.jobs.remove(&dir).is_some_and(|j| j.refresh);
            match result {
                Ok((v, below)) => {
                    self.take_view(dir, v, refresh);
                    self.take_below(below);
                }
                Err(e) => {
                    if let Some(v) = self.cache.get_mut(&dir) {
                        v.scanning = false;
                    }
                    self.status = format!("cannot scan '{}': {e}", dir.display());
                }
            }
        }
        any
    }

    /// Put a finished scan of `dir` in place, keeping the cursor on the
    /// same row when `dir` is on screen.
    fn take_view(&mut self, dir: PathBuf, mut fresh: View, refresh: bool) {
        // Trashed while it scanned: there is nothing left to show.
        if dir != self.cur && dir != self.root && !dir.exists() {
            self.cache.remove(&dir);
            return;
        }
        let on_screen = dir == self.cur;
        let keep = on_screen
            .then(|| self.selected().map(|r| r.path.clone()))
            .flatten();
        fresh.cursor = self.cache.get(&dir).map_or(0, |v| v.cursor);
        if fresh.partial {
            // Directories the stopped scan never reached keep their
            // quick-listing rows, so the view still names every entry.
            if let Some(old) = self.cache.get(&dir) {
                let missing: Vec<Item> = old
                    .rows
                    .iter()
                    .chain(&old.stash)
                    .filter(|r| !fresh.rows.iter().any(|f| f.path == r.path))
                    .cloned()
                    .collect();
                fresh.rows.extend(missing);
            }
        }
        // The order may have changed while the scan ran.
        sort_rows(&mut fresh.rows, self.sorting);
        let stopped = fresh.partial;
        if on_screen {
            self.stash_marks();
        }
        if refresh {
            self.apply_fresh(&dir, fresh);
        } else {
            self.cache.insert(dir.clone(), fresh);
        }
        self.prune_marks(&dir);
        if on_screen && self.narrowed() {
            self.refilter();
        }
        if on_screen {
            self.sel.clear();
            self.restore_marks();
            let at = keep
                .and_then(|p| self.rows().iter().position(|r| r.path == p))
                .unwrap_or(0);
            self.select(at);
            self.status = if stopped {
                "scan stopped · sizes are lower bounds · r rescans".into()
            } else {
                format!("scanned · {} entries", self.rows().len())
            };
        }
    }

    /// Cache the views of roots scanned for the view above them, except
    /// where a view of that root already has sizes or a scan of its own.
    fn take_below(&mut self, below: Vec<(PathBuf, View)>) {
        for (dir, mut v) in below {
            let has_sizes = self.cache.get(&dir).is_some_and(|c| !c.scanning);
            if has_sizes || self.jobs.contains_key(&dir) {
                continue;
            }
            sort_rows(&mut v.rows, self.sorting);
            let on_screen = dir == self.cur;
            self.cache.insert(dir, v);
            if on_screen {
                if self.narrowed() {
                    self.refilter();
                }
                let at = self.list_state.selected().unwrap_or(0);
                self.select(at);
            }
        }
    }

    /// Stop the scan of the current view, keeping what it counted.
    /// Returns false when there is none to stop, so the key can quit.
    fn stop_scan(&mut self) -> bool {
        match self.jobs.get(&self.cur) {
            Some(job) if !job.control.stopped() => {
                job.control.stop.store(true, Ordering::Relaxed);
                self.status = "stopping the scan… (press again to quit)".into();
                true
            }
            _ => false,
        }
    }

    /// Files and bytes counted so far by the scan of the current view.
    fn progress(&self) -> Option<(u64, u64)> {
        self.jobs.get(&self.cur).map(|j| {
            (
                j.control.files.load(Ordering::Relaxed),
                j.control.disk.load(Ordering::Relaxed),
            )
        })
    }

    /// The title's free-space text for the current view, such as
    /// `48.2 GiB free (9%)`. It gives None for a snapshot, for the view
    /// above two or more roots, on a network file system, and when the
    /// file system can't be read.
    fn free_here(&mut self) -> Option<String> {
        if self.snapshot.is_some() || is_top_of_roots(&self.cur) {
            return None;
        }
        if self.free.as_ref().is_none_or(|(p, _)| *p != self.cur) {
            self.free = Some((self.cur.clone(), (self.free_of)(&self.cur)));
        }
        self.free
            .as_ref()
            .and_then(|(_, s)| s.as_ref())
            .map(free_text)
    }

    fn view(&self) -> &View {
        &self.cache[&self.cur]
    }

    fn rows(&self) -> &[Item] {
        &self.view().rows
    }

    fn selected(&self) -> Option<&Item> {
        self.list_state.selected().and_then(|i| self.rows().get(i))
    }

    fn ensure_scroll(&mut self) {
        let off = tui_list::scroll_offset(
            self.list_state.selected(),
            self.rows().len(),
            self.viewport_rows.max(1),
            self.list_state.offset(),
        );
        *self.list_state.offset_mut() = off;
    }

    fn select(&mut self, i: usize) {
        let n = self.rows().len();
        self.list_state
            .select(if n == 0 { None } else { Some(i.min(n - 1)) });
        self.ensure_scroll();
    }

    fn move_sel(&mut self, delta: isize) {
        let next = tui_list::move_selected(self.list_state.selected(), self.rows().len(), delta);
        self.list_state.select(next);
        self.ensure_scroll();
    }

    fn page(&mut self, down: bool) {
        let next = tui_list::page_selected(
            self.list_state.selected(),
            self.rows().len(),
            self.viewport_rows.max(1),
            down,
        );
        self.list_state.select(next);
        self.ensure_scroll();
    }

    /// Switch to `dir`, scanning it when it has no cached view.
    fn go(&mut self, dir: PathBuf, cursor_on: Option<&Path>) {
        if let Some(gone) = self.being_trashed(&dir) {
            self.status = format!("'{}' is being trashed", crate::util::term_path(&gone));
            return;
        }
        // The filter belongs to the view it was typed in.
        if !self.filter.is_empty() {
            self.filter.clear();
            self.refilter();
        }
        let cursor = self.list_state.selected().unwrap_or(0);
        if let Some(v) = self.cache.get_mut(&self.cur) {
            v.cursor = cursor;
        }
        if !self.cache.contains_key(&dir) && self.background {
            match self.quick(&dir) {
                Ok(v) => {
                    self.cache.insert(dir.clone(), v);
                    self.spawn(dir.clone(), false);
                }
                Err(e) => {
                    self.status = format!("cannot open '{}': {e}", dir.display());
                    return;
                }
            }
        }
        if !self.cache.contains_key(&dir) {
            match self.walk(&dir) {
                Ok((v, below)) => {
                    self.cache.insert(dir.clone(), v);
                    self.take_below(below);
                }
                Err(e) => {
                    self.status = format!("cannot scan '{}': {e}", dir.display());
                    return;
                }
            }
        }
        self.stash_marks();
        self.cur = dir;
        // A view left while `.` hid its dot names may need them back.
        if self.hide_dots || !self.view().stash.is_empty() {
            self.refilter();
        }
        self.sel.clear();
        self.restore_marks();
        let at = cursor_on
            .and_then(|p| self.rows().iter().position(|r| r.path == p))
            .unwrap_or(self.view().cursor);
        *self.list_state.offset_mut() = 0;
        self.select(at);
        self.status = format!("{} entries", self.rows().len());
    }

    fn open(&mut self) {
        let Some(it) = self.selected() else {
            return;
        };
        if !it.is_dir {
            self.status = "not a directory".into();
            return;
        }
        if is_trash(it) {
            self.status = "trash directories stay closed; see rtrash list".into();
            return;
        }
        let dir = it.path.clone();
        self.go(dir, None);
    }

    fn back(&mut self) {
        let child = self.cur.clone();
        match up(&self.roots, &child) {
            Some(parent) => self.go(parent, Some(&child)),
            None => self.status = "already at the top".into(),
        }
    }

    /// Rescan the current directory and carry the size change up the chain.
    fn refresh(&mut self) {
        if self.snapshot.is_some() {
            self.status =
                "browsing a snapshot: nothing to rescan; du -i scans the directory".into();
            return;
        }
        if self.deleting.is_some() {
            self.status = "a delete is running; rescan once it is done".into();
            return;
        }
        let cur = self.cur.clone();
        if self.jobs.contains_key(&cur) {
            self.status = "still scanning this directory".into();
            return;
        }
        self.free = None;
        if self.background {
            // The parents never took a stopped view's sizes, so a rescan
            // of one replaces it instead of carrying a change upward.
            let mut partial = false;
            if let Some(v) = self.cache.get_mut(&cur) {
                v.scanning = true;
                partial = v.partial;
            }
            self.spawn(cur, !partial);
            self.status = "rescanning…".into();
            return;
        }
        let (fresh, below) = match self.walk(&cur) {
            Ok(r) => r,
            Err(e) => {
                self.status = format!("cannot scan '{}': {e}", cur.display());
                return;
            }
        };
        self.stash_marks();
        self.apply_fresh(&cur, fresh);
        self.take_below(below);
        self.prune_marks(&cur);
        if self.narrowed() {
            self.refilter();
        }
        self.sel.clear();
        self.restore_marks();
        let at = self.list_state.selected().unwrap_or(0);
        self.select(at);
        self.status = format!("rescanned · {} entries", self.rows().len());
    }

    /// Replace the view of `cur` with a rescan and carry the size change up
    /// the chain.
    fn apply_fresh(&mut self, cur: &Path, fresh: View) {
        let cur = cur.to_path_buf();
        // Children views may hold stale sizes now; drop them.
        self.cache.retain(|p, _| p == &cur || !p.starts_with(&cur));
        let to = Totals::of_view(&fresh);
        let old = self.cache.insert(cur.clone(), fresh).expect("current view");
        let from = Totals::of_view(&old);
        let Some(parent) = up(&self.roots, &cur) else {
            return;
        };
        // The fresh view has its totals; the chain above it moves by the
        // change, and the row that leads to it takes them as they are.
        carry(&mut self.cache, &self.roots, &parent, from, to);
        if let Some(pv) = self.cache.get_mut(&parent) {
            if let Some(row) = pv
                .rows
                .iter_mut()
                .chain(&mut pv.stash)
                .find(|r| r.path == cur)
            {
                put_row(row, to);
            }
        }
    }

    /// Move this view's marks into `away` before the view changes.
    fn stash_marks(&mut self) {
        for i in self.sel.marked_sorted() {
            if let Some(r) = self.rows().get(i) {
                self.away.insert(r.path.clone(), r.clone());
            }
        }
    }

    /// After a scan of `dir`: drop the marks held for entries of `dir` the
    /// scan no longer lists, since they are gone.
    fn prune_marks(&mut self, dir: &Path) {
        let Some(v) = self.cache.get(dir) else {
            return;
        };
        let listed = |p: &Path| v.rows.iter().chain(&v.stash).any(|r| r.path == p);
        let roots = &self.roots;
        self.away
            .retain(|p, _| up(roots, p).as_deref() != Some(dir) || listed(p));
    }

    /// `it` with the sizes its view now shows, when that view is cached;
    /// a mark keeps the sizes of when it was made otherwise.
    fn current(&self, it: &Item) -> Item {
        up(&self.roots, &it.path)
            .and_then(|d| self.cache.get(&d))
            .and_then(|v| v.rows.iter().chain(&v.stash).find(|r| r.path == it.path))
            .cloned()
            .unwrap_or_else(|| it.clone())
    }

    /// Take back the marks `away` holds for rows of this view.
    fn restore_marks(&mut self) {
        if self.away.is_empty() {
            return;
        }
        let here: Vec<usize> = (0..self.rows().len())
            .filter(|&i| self.away.contains_key(&self.rows()[i].path))
            .collect();
        for i in here {
            let p = self.rows()[i].path.clone();
            self.away.remove(&p);
            self.sel.mark(i);
        }
    }

    /// Every mark, from this view and the others, in path order.
    fn all_marks(&self) -> Vec<Item> {
        let mut out: Vec<Item> = self.away.values().map(|it| self.current(it)).collect();
        out.extend(
            self.sel
                .marked_sorted()
                .into_iter()
                .filter_map(|i| self.rows().get(i).cloned()),
        );
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    /// Open the marks panel (`m`).
    fn show_marks(&mut self) {
        if self.marked_count() == 0 {
            self.status = "nothing marked".into();
            return;
        }
        self.marks_at = self.marks_at.min(self.marked_count() - 1);
        self.mode = Mode::Marks;
    }

    /// Drop the mark on `path`, here or in another view.
    fn unmark(&mut self, path: &Path) {
        if self.away.remove(path).is_some() {
            return;
        }
        if let Some(i) = self.rows().iter().position(|r| r.path == path) {
            if self.sel.is_marked(i) {
                self.sel.toggle(i);
            }
        }
    }

    fn on_key_marks(&mut self, code: KeyCode, mods: KeyModifiers) {
        let marks = self.all_marks();
        let n = marks.len();
        match code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.marks_at = (self.marks_at + 1).min(n.saturating_sub(1));
            }
            KeyCode::Up | KeyCode::Char('k') => self.marks_at = self.marks_at.saturating_sub(1),
            KeyCode::Char(' ') | KeyCode::Char('x') => {
                if let Some(it) = marks.get(self.marks_at) {
                    let p = it.path.clone();
                    self.unmark(&p);
                    self.status = format!("unmarked {} · {} marked", p.display(), n - 1);
                }
                if n <= 1 {
                    self.mode = Mode::Browse;
                } else {
                    self.marks_at = self.marks_at.min(n - 2);
                }
            }
            KeyCode::Enter => {
                self.mode = Mode::Browse;
                if let Some(it) = marks.get(self.marks_at) {
                    if let Some(dir) = up(&self.roots, &it.path) {
                        let p = it.path.clone();
                        if dir != self.cur {
                            self.go(dir, Some(&p));
                        } else if let Some(i) = self.rows().iter().position(|r| r.path == p) {
                            self.select(i);
                        }
                    }
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                self.mode = Mode::Browse;
                self.ask_delete();
            }
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => self.quit = true,
            _ => self.mode = Mode::Browse,
        }
    }

    /// Marks in every view.
    fn marked_count(&self) -> usize {
        self.sel.len() + self.away.len()
    }

    /// The rows a trash, print or copy acts on: the marks of every view,
    /// else the cursor row. A mark under another marked directory goes
    /// with it and is not counted twice.
    fn batch(&self) -> Vec<Item> {
        let mut out: Vec<Item> = self.away.values().map(|it| self.current(it)).collect();
        let marked = self.sel.marked_sorted();
        let idxs = if marked.is_empty() && self.away.is_empty() {
            self.list_state.selected().into_iter().collect()
        } else {
            marked
        };
        out.extend(idxs.into_iter().filter_map(|i| self.rows().get(i).cloned()));
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out.dedup_by(|a, b| a.path == b.path);
        let all: Vec<PathBuf> = out.iter().map(|r| r.path.clone()).collect();
        out.retain(|r| !all.iter().any(|p| p != &r.path && r.path.starts_with(p)));
        out
    }

    fn targets(&self) -> Vec<Item> {
        self.batch().into_iter().filter(|r| !is_trash(r)).collect()
    }

    /// Why `t` cannot be trashed: a directory in it holds entries the
    /// filters left out, which its trash would take unseen. Nothing goes
    /// then, unless `--allow-filtered` was given.
    fn filtered_refusal(&self, t: &[Item]) -> Option<String> {
        if self.allow_filtered {
            return None;
        }
        let held: Vec<&Item> = t.iter().filter(|r| r.filtered_out > 0).collect();
        let first = held.first()?;
        let name = first.path.file_name().map_or_else(
            || first.path.display().to_string(),
            |n| n.to_string_lossy().into(),
        );
        let others = match held.len() {
            1 => String::new(),
            n => format!(", as do {} more", n - 1),
        };
        Some(format!(
            "'{name}' holds {} the filters hide{others}; nothing was trashed \
             (--allow-filtered trashes them too)",
            crate::util::plural(first.filtered_out, "entry", "entries")
        ))
    }

    fn ask_delete(&mut self) {
        if self.read_only {
            self.status = "read-only (--no-delete): nothing gets trashed".into();
            return;
        }
        if self.deleting.is_some() {
            self.status = "a delete is running; wait for it to finish".into();
            return;
        }
        if self.view().scanning {
            self.status = "still scanning; trash rows once the sizes are in".into();
            return;
        }
        let t = self.targets();
        if t.is_empty() {
            self.status = "nothing to trash".into();
            return;
        }
        if let Some(why) = self.filtered_refusal(&t) {
            self.status = why;
            return;
        }
        let bytes = du::size_sum(t.iter().map(|r| r.disk));
        let risky = t
            .iter()
            .filter(|r| r.git.as_ref().is_some_and(|(s, _)| s.is_risky()))
            .count();
        let dirs: std::collections::BTreeSet<Option<PathBuf>> =
            t.iter().map(|r| up(&self.roots, &r.path)).collect();
        let place = if dirs.len() > 1 {
            format!(" in {} directories", dirs.len())
        } else {
            String::new()
        };
        self.mode = Mode::Confirm;
        self.status = if risky > 0 {
            format!(
                "trash {} item(s){place}, {}, {risky} with git work at risk (!)?  y confirm · n cancel",
                t.len(),
                format_bytes(bytes)
            )
        } else {
            format!(
                "trash {} item(s){place}, {}?  y confirm · n cancel",
                t.len(),
                format_bytes(bytes)
            )
        };
    }

    /// Start trashing the confirmed rows. In the terminal session they go
    /// on a thread of their own while the explorer keeps drawing; tests
    /// trash them inline. [`App::poll_delete`] takes in each one.
    fn delete(&mut self) {
        self.mode = Mode::Browse;
        if self.deleting.is_some() {
            self.status = "a delete is running; wait for it to finish".into();
            return;
        }
        let t = self.targets();
        if let Some(why) = self.filtered_refusal(&t) {
            self.status = why;
            return;
        }
        // RTRASH_ALLOW_ROOTS: every row is checked before the first one
        // goes, so a refused delete trashes nothing.
        if let Some(roots) = &self.scope {
            let outside = t
                .iter()
                .filter(|i| !crate::allow_roots::allowed_in(roots, &i.path))
                .count();
            if outside > 0 {
                self.status =
                    format!("{outside} item(s) outside RTRASH_ALLOW_ROOTS; nothing was trashed");
                return;
            }
        }
        self.sel.clear();
        self.away.clear();
        let (tx, rx) = mpsc::channel();
        let watch = Watch::default();
        self.deleting = Some(Deleting {
            rx,
            watch: watch.clone(),
            pending: t.iter().map(|i| (i.path.clone(), i.apparent)).collect(),
            total: t.len(),
            left: du::size_sum(t.iter().map(|i| i.disk)),
            group: Vec::new(),
            failed: Vec::new(),
            shown: (0, 0),
        });
        self.failures.clear();
        let (prog, pin) = (self.prog.clone(), self.trash_pin.clone());
        let work = move || trash_rows(&prog, t, pin.as_ref(), &watch, &tx);
        if self.background {
            std::thread::spawn(work);
            self.show_delete_progress();
        } else {
            work();
            self.poll_delete();
        }
    }

    /// The queued row `dir` is, or lies under, while a delete runs.
    fn being_trashed(&self, dir: &Path) -> Option<PathBuf> {
        let d = self.deleting.as_ref()?;
        d.pending
            .iter()
            .find(|(p, _)| dir.starts_with(p))
            .map(|(p, _)| p.clone())
    }

    /// Take in every row the delete has done since the last call: each
    /// trashed row leaves its view and shrinks the views above. Once the
    /// thread is done, the rows become one undo step. Returns whether
    /// anything came in.
    fn poll_delete(&mut self) -> bool {
        let Some(mut d) = self.deleting.take() else {
            return false;
        };
        let mut came = Vec::new();
        let finished = loop {
            match d.rx.try_recv() {
                Ok(t) => came.push(t),
                Err(mpsc::TryRecvError::Empty) => break false,
                Err(mpsc::TryRecvError::Disconnected) => break true,
            }
        };
        let any = !came.is_empty();
        for (item, done) in came {
            d.pending.retain(|(p, _)| *p != item.path);
            d.left = d.left.saturating_sub(item.disk);
            match done {
                Ok(ticket) => {
                    self.drop_row(&item);
                    d.group.push((ticket, item));
                }
                Err(why) => d.failed.push((item.path, why)),
            }
        }
        if any {
            let at = self.list_state.selected().unwrap_or(0);
            self.select(at);
            self.free = None;
        }
        if finished {
            self.finish_delete(d);
            return true;
        }
        // A new row done or a new chunk copied moves the status line;
        // otherwise a refusal shown meanwhile stays readable.
        let now = (
            d.group.len() + d.failed.len(),
            d.watch.copied.load(Ordering::Relaxed),
        );
        let moved = now != d.shown;
        self.deleting = Some(d);
        if moved {
            self.show_delete_progress();
        }
        any || moved
    }

    /// The trashed `it` leaves its view, and the views above it shrink;
    /// views below it describe paths that are gone.
    fn drop_row(&mut self, it: &Item) {
        // The view on screen lay inside the row (marked in a view above,
        // then opened): show the row's parent, which stays.
        if self.cur.starts_with(&it.path) {
            if let Some(parent) = up(&self.roots, &it.path) {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.refilter();
                }
                self.cur = parent;
            }
        }
        if let Some(parent) = up(&self.roots, &it.path) {
            if let Some(v) = self.cache.get_mut(&parent) {
                v.rows.retain(|r| r.path != it.path);
                v.stash.retain(|r| r.path != it.path);
            }
            let gone = Totals::of_row(it);
            carry(
                &mut self.cache,
                &self.roots,
                &parent,
                gone,
                Totals::default(),
            );
        }
        self.cache.retain(|p, _| !p.starts_with(&it.path));
    }

    /// The status line while a delete runs.
    fn show_delete_progress(&mut self) {
        let Some(d) = &mut self.deleting else {
            return;
        };
        let done = d.group.len() + d.failed.len();
        let copied = d.watch.copied.load(Ordering::Relaxed);
        d.shown = (done, copied);
        // The row in hand is copying across file systems.
        let copying = match d.pending.first() {
            Some((path, size)) if copied > 0 => format!(
                "; copying {}: {} of {}",
                crate::util::term_path(Path::new(path.file_name().unwrap_or_default())),
                format_bytes(copied),
                format_bytes(*size)
            ),
            _ => String::new(),
        };
        let failing = match d.failed.len() {
            0 => String::new(),
            f => format!("; {f} failed (e shows why)"),
        };
        self.status = if d.watch.stop.load(Ordering::Relaxed) {
            format!(
                "stopping: {done} of {} done; a copy in progress is undone",
                d.total
            )
        } else {
            format!(
                "trashing: {done} of {} done, {} left{copying}{failing} · q stops",
                d.total,
                format_bytes(d.left)
            )
        };
    }

    /// Ask the running delete to stop (on quit): the thread starts no
    /// new row, and a copy in progress ends and removes its part copied.
    fn stop_delete(&mut self) {
        if let Some(d) = &self.deleting {
            d.watch.stop.store(true, Ordering::Relaxed);
        }
        self.show_delete_progress();
    }

    fn finish_delete(&mut self, d: Deleting) {
        let disk = du::size_sum(d.group.iter().map(|(_, it)| it.disk));
        let n = d.group.len();
        let left = d.pending.len();
        if n > 0 {
            self.undo.push(d.group);
        }
        if n == 0 && d.failed.is_empty() && left > 0 {
            self.status = format!("stopped; nothing trashed, {left} left in place");
            return;
        }
        let stopped = if left > 0 {
            format!(", {left} left in place")
        } else {
            String::new()
        };
        self.status = match d.failed.first() {
            None => format!(
                "trashed {n} item(s), {}{stopped} · u undo",
                format_bytes(disk)
            ),
            Some((path, why)) => format!(
                "trashed {n}, {} failed ({}: {why}){stopped} · e why · u undo",
                d.failed.len(),
                crate::util::term_path(Path::new(path.file_name().unwrap_or_default()))
            ),
        };
        self.failures = d.failed;
    }

    /// List the rows the delete left in place and why (`e`), in the info
    /// panel: the running delete's so far, else the last one's.
    fn show_failures(&mut self) {
        let failed = self.deleting.as_ref().map_or(&self.failures, |d| &d.failed);
        if failed.is_empty() {
            self.status = "no row failed in the last delete".into();
            return;
        }
        self.info = failed
            .iter()
            .map(|(path, why)| format!("{}: {why}", crate::util::term_path(path)))
            .collect();
        self.mode = Mode::Info;
    }

    /// Restore the most recent delete of this session.
    fn undo_last(&mut self) {
        if self.read_only {
            self.status = "read-only (--no-delete): nothing to undo".into();
            return;
        }
        if self.deleting.is_some() {
            self.status = "a delete is running; undo once it is done".into();
            return;
        }
        let Some(group) = self.undo.pop() else {
            self.status = "nothing to undo".into();
            return;
        };
        let mut back = 0usize;
        let mut failed = 0usize;
        for (t, item) in group {
            if !bin::untrash(&t) {
                failed += 1;
                continue;
            }
            back += 1;
            let Some(parent) = up(&self.roots, &item.path) else {
                continue;
            };
            let back_in = Totals::of_row(&item);
            if let Some(v) = self.cache.get_mut(&parent) {
                if !v.rows.iter().any(|r| r.path == item.path) {
                    insert_in_order(&mut v.rows, item, self.sorting);
                }
            }
            carry(
                &mut self.cache,
                &self.roots,
                &parent,
                Totals::default(),
                back_in,
            );
        }
        if self.narrowed() {
            self.refilter();
        }
        let at = self.list_state.selected().unwrap_or(0);
        self.select(at);
        self.free = None;
        self.status = match failed {
            0 => format!("restored {back} item(s)"),
            f => format!("restored {back}, {f} failed (path taken again?)"),
        };
    }

    /// The order key rows carry badges for (items, age): the sort key, or
    /// the size kind when sorted by name.
    fn shown(&self) -> SortKey {
        match self.sorting.order {
            Order::Key(k) => k,
            Order::Name => self.size_key,
        }
    }

    /// Order every view by `order`, or reverse the order when it is the
    /// current one. The cursor and marks stay on the same rows.
    fn sort_by(&mut self, order: Order) {
        let reverse = self.sorting.order == order && !self.sorting.reverse;
        self.resort(Sorting { order, reverse });
        self.status = format!("sorted by {}", self.sorting.label());
    }

    /// Switch rows, bars and totals between disk use and apparent size
    /// (`S`). A size order follows the switch and keeps its direction.
    fn toggle_size(&mut self) {
        self.size_key = match self.size_key {
            SortKey::Apparent => SortKey::Disk,
            _ => SortKey::Apparent,
        };
        if let Order::Key(SortKey::Disk | SortKey::Apparent) = self.sorting.order {
            let reverse = self.sorting.reverse;
            self.resort(Sorting {
                order: Order::Key(self.size_key),
                reverse,
            });
        }
        self.status = match self.size_key {
            SortKey::Apparent => "showing apparent sizes (S for disk use)".into(),
            _ => "showing disk use (S for apparent sizes)".into(),
        };
    }

    /// Put every view in the order of `sorting`.
    fn resort(&mut self, sorting: Sorting) {
        self.sorting = sorting;
        let keep = self.selected().map(|r| r.path.clone());
        let marked: Vec<PathBuf> = self
            .sel
            .marked_sorted()
            .into_iter()
            .filter_map(|i| self.rows().get(i).map(|r| r.path.clone()))
            .collect();
        let cur = self.cur.clone();
        for (dir, v) in self.cache.iter_mut() {
            sort_rows(&mut v.rows, self.sorting);
            // A saved cursor would point at whatever row moved there.
            if *dir != cur {
                v.cursor = 0;
            }
        }
        let pos = |a: &App, p: &Path| a.rows().iter().position(|r| r.path == p);
        self.sel.clear();
        for p in &marked {
            if let Some(i) = pos(self, p) {
                self.sel.mark(i);
            }
        }
        let at = keep.and_then(|p| pos(self, &p)).unwrap_or(0);
        self.select(at);
    }

    /// Whether the filter or `.` keeps some rows of a view out of sight.
    fn narrowed(&self) -> bool {
        self.hide_dots || !self.filter.is_empty()
    }

    /// Show the next of bar, percent, both and neither on the rows, as
    /// ncdu's `g` does.
    fn cycle_share(&mut self) {
        self.share = self.share.next();
        self.status = format!("rows show {} · % changes it", self.share.label());
    }

    /// Draw the bars on a log scale, or back on a linear one (ncdu #75).
    fn toggle_log_bars(&mut self) {
        self.log_bars = !self.log_bars;
        self.status = if self.log_bars {
            "bars use a log scale: two cells fewer is a tenth of the share · L changes it".into()
        } else {
            "bars use a linear scale · L changes it".into()
        };
    }

    /// Hide names that start with a dot, or show them again.
    fn toggle_dots(&mut self) {
        self.hide_dots = !self.hide_dots;
        self.refilter();
        let hidden = self.view().stash.len();
        self.status = if self.hide_dots {
            format!("hiding names that start with a dot ({hidden} here) · . shows them")
        } else {
            "showing names that start with a dot".into()
        };
    }

    /// Show only the rows of the current view whose name matches the
    /// filter, leaving out dot names while `.` hides them; the rest wait
    /// in the view's stash. The cursor and marks stay on their rows when
    /// those stay visible.
    fn refilter(&mut self) {
        let keep = self.selected().map(|r| r.path.clone());
        let marked: Vec<PathBuf> = self
            .sel
            .marked_sorted()
            .into_iter()
            .filter_map(|i| self.rows().get(i).map(|r| r.path.clone()))
            .collect();
        let pat = self.filter.to_lowercase();
        let hide = self.hide_dots;
        let sorting = self.sorting;
        let Some(v) = self.cache.get_mut(&self.cur) else {
            return;
        };
        // Rows that stay shown keep their order, which a delete below may
        // have left unsorted (ncdu #237); rows back from the stash slot in.
        let show = |it: &Item| name_matches(it, &pat) && !(hide && is_dot_name(it));
        let (mut shown, mut hidden): (Vec<Item>, Vec<Item>) =
            std::mem::take(&mut v.rows).into_iter().partition(show);
        let (back, still): (Vec<Item>, Vec<Item>) =
            std::mem::take(&mut v.stash).into_iter().partition(show);
        hidden.extend(still);
        for it in back {
            insert_in_order(&mut shown, it, sorting);
        }
        v.rows = shown;
        v.stash = hidden;
        let pos = |a: &App, p: &Path| a.rows().iter().position(|r| r.path == p);
        self.sel.clear();
        for p in &marked {
            if let Some(i) = pos(self, p) {
                self.sel.mark(i);
            }
        }
        let at = keep.and_then(|p| pos(self, &p)).unwrap_or(0);
        *self.list_state.offset_mut() = 0;
        self.select(at);
    }

    fn filter_status(&self) -> String {
        format!(
            "/{} · {} shown · Enter keeps · Esc clears",
            self.filter,
            self.rows().len()
        )
    }

    fn on_key_filter(&mut self, code: KeyCode, mods: KeyModifiers) {
        match self.keys.resolve_filter(code, mods) {
            Some(Action::FilterCommit) => {
                self.mode = Mode::Browse;
                self.status = if self.filter.is_empty() {
                    "filter cleared".into()
                } else {
                    format!(
                        "{} matching {:?} · Esc clears",
                        self.rows().len(),
                        self.filter
                    )
                };
            }
            Some(Action::FilterCancel) => {
                self.filter.clear();
                self.refilter();
                self.mode = Mode::Browse;
                self.status = "filter cleared".into();
            }
            Some(Action::MoveDown) => self.move_sel(1),
            Some(Action::MoveUp) => self.move_sel(-1),
            Some(Action::QuitHard) => self.quit = true,
            _ => {
                if code == KeyCode::Backspace {
                    self.filter.pop();
                } else if let KeyCode::Char(c) = code {
                    if c.is_control() || mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    {
                        return;
                    }
                    self.filter.push(c);
                } else {
                    return;
                }
                self.refilter();
                self.status = self.filter_status();
            }
        }
    }

    /// The marked rows, or the cursor row when none is marked.
    fn chosen_paths(&self) -> Vec<PathBuf> {
        self.batch().into_iter().map(|r| r.path).collect()
    }

    /// Quit and print the chosen paths (`p`).
    fn print_and_quit(&mut self) {
        let paths = self.chosen_paths();
        if paths.is_empty() {
            self.status = "nothing to print".into();
            return;
        }
        self.print_on_exit = paths;
        self.quit = true;
    }

    /// Why `E` cannot write the current view, if it cannot.
    fn export_refusal(&self) -> Option<&'static str> {
        let v = self.view();
        if self.snapshot.is_some() {
            Some("browsing a snapshot: du -f reads it as it is; nothing to write")
        } else if v.scanning {
            Some("still scanning this directory; E writes it once the scan is done")
        } else if v.partial {
            Some("the scan was stopped; r rescans in full, then E writes it")
        } else if self.export_report().is_none() {
            Some("sizes changed since the scan; r rescans, then E writes it")
        } else {
            None
        }
    }

    /// What `E` writes of the current view: its own scan, or above
    /// several roots the scans their views keep, joined as one report.
    /// `None` once a delete, an undo or a rescan has dropped one.
    fn export_report(&self) -> Option<Arc<du::Report>> {
        if !is_top_of_roots(&self.cur) {
            return self.view().scan.clone();
        }
        let parts: Option<Vec<Arc<du::Report>>> = self
            .roots
            .iter()
            .map(|r| self.cache.get(r).and_then(|v| v.scan.clone()))
            .collect();
        parts.map(|p| Arc::new(join_reports(&p)))
    }

    /// Ask for the file to write this view's snapshot to (`E`).
    fn ask_export(&mut self) {
        if let Some(why) = self.export_refusal() {
            self.status = why.into();
            return;
        }
        self.export_to.clear();
        self.mode = Mode::Export;
        self.status = self.export_status();
    }

    fn export_status(&self) -> String {
        let what = if is_top_of_roots(&self.cur) {
            format!("these {} directories", self.roots.len())
        } else {
            "this directory".to_string()
        };
        format!(
            "write a du snapshot of {what} to: {} · Enter writes · Esc cancels",
            self.export_to
        )
    }

    fn on_key_export(&mut self, code: KeyCode, mods: KeyModifiers) {
        match self.keys.resolve_filter(code, mods) {
            Some(Action::FilterCommit) => {
                self.mode = Mode::Browse;
                self.status = self.export();
            }
            Some(Action::FilterCancel) => {
                self.mode = Mode::Browse;
                self.status = "nothing written".into();
            }
            Some(Action::QuitHard) => self.quit = true,
            _ => {
                if code == KeyCode::Backspace {
                    self.export_to.pop();
                } else if let KeyCode::Char(c) = code {
                    if c.is_control() || mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    {
                        return;
                    }
                    self.export_to.push(c);
                } else {
                    return;
                }
                self.status = self.export_status();
            }
        }
    }

    /// Write the current view as the snapshot `du -o FILE --max-depth=1`
    /// would write of its directory (above several roots, of all of
    /// them), from the scans in memory: no second walk. An existing file
    /// is left alone. Returns the status line.
    fn export(&self) -> String {
        if let Some(why) = self.export_refusal() {
            return why.into();
        }
        let Some(report) = self.export_report() else {
            return "nothing to write".into();
        };
        if self.export_to.is_empty() {
            return "no file name; nothing written".into();
        }
        let path = crate::util::arg_path(&self.export_to);
        let shown = crate::util::term_path(&path);
        if std::fs::symlink_metadata(&path).is_ok() {
            return format!("'{shown}' exists; nothing written (give a new name)");
        }
        let opts = ScanOpts {
            max_depth: Some(1),
            ..self.opts.clone()
        };
        let now = crate::util::now_epoch();
        match crate::du_snap::write_file(&path, &report, &opts, now) {
            Ok(()) => format!(
                "wrote '{shown}' · {} · du -f or du diff reads it",
                crate::util::plural(
                    report.dirs.iter().filter(|d| d.is_dir).count() as u64,
                    "directory",
                    "directories"
                )
            ),
            Err(e) => format!("cannot write '{shown}': {e}"),
        }
    }

    /// Copy the chosen paths, one per line, to the terminal clipboard (`y`).
    fn copy_paths(&mut self) {
        let paths = self.chosen_paths();
        if paths.is_empty() {
            self.status = "nothing to copy".into();
            return;
        }
        let text: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        self.status = format!(
            "copied {} to the clipboard (OSC 52; the terminal must allow it)",
            crate::util::plural(paths.len() as u64, "path", "paths")
        );
        self.clipboard = Some(text.join("\n"));
    }

    /// Ask the loop for a shell in the current directory (`b`, as in ncdu
    /// and gdu). Read-only mode offers none, since a shell can delete.
    fn ask_shell(&mut self) {
        if self.read_only {
            self.status = "read-only (--no-delete): no shell".into();
            return;
        }
        if is_top_of_roots(&self.cur) {
            self.status = "open one of the directories first; b opens a shell there".into();
            return;
        }
        self.hand_off = Some(HandOff::Shell(self.cur.clone()));
    }

    /// Show the cursor file in the pager (`v`, as in gdu).
    fn view_file(&mut self) {
        if let Some(gone) = self.selected().and_then(|it| self.being_trashed(&it.path)) {
            self.status = format!("'{}' is being trashed", crate::util::term_path(&gone));
            return;
        }
        match self.selected() {
            Some(it) if !it.is_dir => self.hand_off = Some(HandOff::Pager(it.path.clone())),
            Some(_) => self.status = "v shows files; o opens a directory".into(),
            None => self.status = "nothing to view".into(),
        }
    }

    /// Back from the pager: say so, or why it could not start.
    fn after_pager(&mut self, status: io::Result<std::process::ExitStatus>) {
        self.status = match status {
            Ok(s) if s.success() => "pager closed".into(),
            Ok(s) => format!("pager closed ({s})"),
            Err(e) => format!("cannot start the pager: {e} (set PAGER to choose one)"),
        };
    }

    /// Open the cursor row with its default application (`o`, as in gdu):
    /// a file in its program, a directory in the file manager. The
    /// opener runs on its own, so the explorer keeps the screen.
    fn open_row(&mut self) {
        let Some(path) = self.selected().map(|it| it.path.clone()) else {
            self.status = "nothing to open".into();
            return;
        };
        if let Some(gone) = self.being_trashed(&path) {
            self.status = format!("'{}' is being trashed", crate::util::term_path(&gone));
            return;
        }
        let name = self.opener[0].to_string_lossy().into_owned();
        let spawned = std::process::Command::new(&self.opener[0])
            .args(&self.opener[1..])
            .arg(&path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        self.status = match spawned {
            Ok(mut child) => {
                // Reap it whenever it ends; its result is not ours to show.
                std::thread::spawn(move || child.wait());
                format!("opening {} with {name}", crate::util::term_path(&path))
            }
            Err(e) => format!("cannot run {name}: {e}"),
        };
    }

    /// Back from the shell: say how it ended and rescan the directory,
    /// whose contents may have changed.
    fn after_shell(&mut self, status: io::Result<std::process::ExitStatus>) {
        let said = match status {
            Ok(s) if s.success() => "shell closed".to_string(),
            Ok(s) => format!("shell closed ({s})"),
            Err(e) => format!(
                "cannot start {}: {e}",
                tui_term::shell_program().to_string_lossy()
            ),
        };
        self.refresh();
        self.status = format!("{said} · {}", self.status);
    }

    /// Open the info panel on the cursor row.
    fn show_info(&mut self) {
        let Some(it) = self.selected() else {
            self.status = "nothing to show".into();
            return;
        };
        let v = self.view();
        let pending = v.scanning && it.is_dir;
        self.info = info_lines(it, v.disk, pending, crate::util::now_epoch());
        self.mode = Mode::Info;
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        match self.mode {
            Mode::Filter => self.on_key_filter(code, mods),
            Mode::Marks => self.on_key_marks(code, mods),
            Mode::Export => self.on_key_export(code, mods),
            Mode::Info => {
                if self.keys.resolve_help(code, mods) == Some(Action::QuitHard) {
                    self.quit = true;
                }
                // Any other key closes the panel.
                self.mode = Mode::Browse;
            }
            Mode::Help => match self.keys.resolve_help(code, mods) {
                Some(Action::Help | Action::Quit | Action::FilterCancel) => {
                    self.mode = Mode::Browse;
                    self.status = "help closed".into();
                }
                Some(Action::QuitHard) => self.quit = true,
                _ => {}
            },
            Mode::Confirm => match self.keys.resolve_confirm(code, mods) {
                Some(Action::ConfirmYes) => self.delete(),
                Some(Action::ConfirmNo) => {
                    self.mode = Mode::Browse;
                    self.status = "cancelled".into();
                }
                Some(Action::QuitHard) => self.quit = true,
                _ => {}
            },
            Mode::Browse => {
                // Explorer keys first; they are not in the shared keymap.
                if mods.is_empty() || mods == KeyModifiers::SHIFT {
                    match code {
                        KeyCode::Right | KeyCode::Char('l') => return self.open(),
                        KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
                            return self.back()
                        }
                        KeyCode::Char('d') | KeyCode::Delete => return self.ask_delete(),
                        KeyCode::Char('u') => return self.undo_last(),
                        KeyCode::Char('e') => return self.show_failures(),
                        KeyCode::Char('r') => return self.refresh(),
                        KeyCode::Char('s') => return self.sort_by(Order::Key(self.size_key)),
                        KeyCode::Char('S') => return self.toggle_size(),
                        KeyCode::Char('n') => return self.sort_by(Order::Name),
                        KeyCode::Char('C') => return self.sort_by(Order::Key(SortKey::Items)),
                        KeyCode::Char('M') => return self.sort_by(Order::Key(SortKey::Mtime)),
                        KeyCode::Char('i') => return self.show_info(),
                        KeyCode::Char('m') => return self.show_marks(),
                        KeyCode::Char('p') => return self.print_and_quit(),
                        KeyCode::Char('y') => return self.copy_paths(),
                        KeyCode::Char('E') => return self.ask_export(),
                        KeyCode::Char('b') => return self.ask_shell(),
                        KeyCode::Char('v') => return self.view_file(),
                        KeyCode::Char('o') => return self.open_row(),
                        KeyCode::Char('.') => return self.toggle_dots(),
                        KeyCode::Char('%') => return self.cycle_share(),
                        KeyCode::Char('L') => return self.toggle_log_bars(),
                        _ => {}
                    }
                }
                match self.keys.resolve_browse(code, mods) {
                    Some(Action::QuitHard) => self.quit = true,
                    // A running scan stops first, then the filter clears.
                    Some(Action::Quit) if !self.stop_scan() => {
                        if self.filter.is_empty() {
                            self.quit = true;
                        } else {
                            self.filter.clear();
                            self.refilter();
                            self.status = "filter cleared · press again to quit".into();
                        }
                    }
                    Some(Action::OpenFilter) => {
                        self.mode = Mode::Filter;
                        self.status = self.filter_status();
                    }
                    Some(Action::MoveDown) => self.move_sel(1),
                    Some(Action::MoveUp) => self.move_sel(-1),
                    Some(Action::PageDown) => self.page(true),
                    Some(Action::PageUp) => self.page(false),
                    Some(Action::First) => self.select(0),
                    Some(Action::Last) => self.select(usize::MAX),
                    // Marks wait for a running delete: its rows leave the
                    // views as it goes, and a mark must not land on another.
                    Some(Action::ToggleMark | Action::MarkAll) if self.deleting.is_some() => {
                        self.status = "a delete is running; mark once it is done".into();
                    }
                    Some(Action::ToggleMark) => {
                        if let Some(i) = self.list_state.selected() {
                            self.sel.toggle(i);
                            self.status = format!("{} marked", self.marked_count());
                        }
                    }
                    Some(Action::MarkAll) => {
                        self.sel.mark_all(0..self.rows().len());
                        self.status = format!("{} marked", self.marked_count());
                    }
                    Some(Action::ClearMarks) => {
                        self.sel.clear();
                        self.away.clear();
                        self.status = "marks cleared".into();
                    }
                    Some(Action::Help) => self.mode = Mode::Help,
                    Some(Action::Action) => self.open(),
                    _ => {}
                }
            }
        }
    }
}

/// The room left on the file system of `dir`, for [`App::free_here`].
/// A network file system is left out: its statvfs can hang on a dead
/// server, and a probe's warning would write over the explorer.
#[cfg(unix)]
fn free_space(dir: &Path) -> Option<crate::space::Space> {
    if crate::mount_probe::is_network_fs(&crate::mount_probe::fs_type_of(dir)) {
        return None;
    }
    crate::space::query(dir).ok()
}

/// The room left on the volume of `dir`, for [`App::free_here`].
#[cfg(not(unix))]
fn free_space(dir: &Path) -> Option<crate::space::Space> {
    crate::space::query(dir).ok()
}

/// `48.2 GiB free (9%)`: what this user may still write, and its share
/// of the file system.
fn free_text(s: &crate::space::Space) -> String {
    format!("{} free ({}%)", format_bytes(s.avail), s.avail_percent())
}

/// The explorer's title in a box `width` columns wide: `place`, then
/// `tail` with the sizes, counts and markers. The tail always fits; a
/// long path gives way from the left, as in ncdu. `free` ranks lowest:
/// it comes last, and only when the whole path fits beside it, so a
/// narrow screen drops it before it cuts the path.
fn title_text(width: u16, place: &str, tail: &str, free: Option<&str>) -> String {
    // The box's corners take two columns and the leading space one.
    let room = |tail: &str| {
        (width as usize)
            .saturating_sub(2)
            .saturating_sub(cols(tail) + 1)
    };
    if let Some(free) = free {
        let with = format!("{tail}· {free} ");
        if cols(place) <= room(&with) {
            return format!(" {place}{with}");
        }
    }
    format!(" {}{tail}", fit_left(place, room(tail)))
}

/// Whether the file name of `it` starts with a dot.
fn is_dot_name(it: &Item) -> bool {
    it.path
        .file_name()
        .is_some_and(|n| n.as_encoded_bytes().first() == Some(&b'.'))
}

/// Whether the file name of `it` matches `pat` (lowercase): a glob when
/// it holds `*`, `?` or `[`, else a part of the name. Case is ignored.
fn name_matches(it: &Item, pat: &str) -> bool {
    if pat.is_empty() {
        return true;
    }
    let name = it
        .path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    if pat.contains(['*', '?', '[']) {
        crate::util::fnmatch(pat, &name)
    } else {
        name.contains(pat)
    }
}

/// `n` bytes in the explorer's units, with the exact count.
fn bytes_line(n: u64) -> String {
    format!(
        "{} ({})",
        format_bytes(n),
        crate::util::plural(n, "byte", "bytes")
    )
}

/// The info panel for `it`: path, kind, sizes, share of `whole`, items,
/// newest change, git state and, on Unix, mode, owner and links.
/// `pending` marks a directory whose size is still coming.
fn info_lines(it: &Item, whole: u64, pending: bool, now: i64) -> Vec<String> {
    let mut out = Vec::new();
    let mut path = crate::util::term_path(&it.path);
    if it.is_dir {
        path.push(std::path::MAIN_SEPARATOR);
    }
    out.push(format!("path      {path}"));
    let kind = if it.is_dir {
        "directory"
    } else if is_link(it) {
        "symbolic link"
    } else {
        "file"
    };
    out.push(match it.artefact.filter(|_| !is_link(it)) {
        Some(a) => format!("kind      {kind}, {a}"),
        None => format!("kind      {kind}"),
    });
    if is_link(it) {
        if let Ok(to) = std::fs::read_link(&it.path) {
            out.push(format!("target    {}", crate::util::term_path(&to)));
        }
    }
    if pending {
        out.push("disk use  … (scan running)".into());
    } else {
        let share = if whole == 0 {
            String::new()
        } else {
            let pct = it.disk as f64 * 100.0 / whole as f64;
            format!(" · {pct:.1}% of this directory")
        };
        out.push(format!("disk use  {}{share}", bytes_line(it.disk)));
        out.push(format!("apparent  {}", bytes_line(it.apparent)));
        if let Some(u) = it.unique {
            out.push(format!("unique    {}", bytes_line(u)));
        }
        if it.is_dir {
            out.push(format!(
                "items     {} below",
                crate::util::plural(it.items, "file or directory", "files and directories")
            ));
        }
    }
    if it.mtime > 0 {
        let newest = if it.is_dir && !pending {
            " (newest in the tree)"
        } else {
            ""
        };
        out.push(format!(
            "changed   {} ago, {} UTC{newest}",
            du::format_age(it.mtime, now),
            crate::platform::epoch_to_iso_utc(it.mtime).replace('T', " ")
        ));
    }
    if let Some((s, repo)) = &it.git {
        out.push(format!(
            "git       {} in {}",
            s.as_str(),
            crate::util::term_path(repo)
        ));
    }
    #[cfg(unix)]
    if let Ok(m) = std::fs::symlink_metadata(&it.path) {
        use std::os::unix::fs::MetadataExt;
        out.push(format!(
            "mode      {} ({:04o}) · owner {} · group {} · {}",
            mode_string(m.mode()),
            m.mode() & 0o7777,
            crate::trashdir::uid_name(m.uid()),
            m.gid(),
            crate::util::plural(m.nlink(), "link", "links")
        ));
    }
    #[cfg(windows)]
    if let Ok(m) = std::fs::symlink_metadata(&it.path) {
        if m.permissions().readonly() {
            out.push("mode      read-only".into());
        }
    }
    out
}

/// Terminal columns `s` takes as ratatui draws it: two for a wide
/// character (CJK, most emoji), none for a combining mark (dua #415).
fn cols(s: &str) -> usize {
    Span::raw(s).width()
}

/// [`cols`] of one character.
fn char_cols(c: char) -> usize {
    cols(c.encode_utf8(&mut [0u8; 4]))
}

/// `lines` broken into rows of at most `width` columns. A character
/// wider than a whole row gets a row of its own.
fn hard_wrap(lines: &[String], width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for l in lines {
        let (mut row, mut used) = (String::new(), 0);
        for c in l.chars() {
            let w = char_cols(c);
            if used + w > width && !row.is_empty() {
                out.push(std::mem::take(&mut row));
                used = 0;
            }
            row.push(c);
            used += w;
        }
        if !row.is_empty() || l.is_empty() {
            out.push(row);
        }
    }
    out
}

/// `ls -l` style type and permission letters for `mode`.
#[cfg(unix)]
fn mode_string(mode: u32) -> String {
    let kind = match mode & 0o170000 {
        0o040000 => 'd',
        0o120000 => 'l',
        0o010000 => 'p',
        0o140000 => 's',
        0o020000 => 'c',
        0o060000 => 'b',
        _ => '-',
    };
    let mut s = String::from(kind);
    for (shift, special, lower, upper) in [
        (6, 0o4000, 's', 'S'),
        (3, 0o2000, 's', 'S'),
        (0, 0o1000, 't', 'T'),
    ] {
        let bits = (mode >> shift) & 7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        let x = bits & 1 != 0;
        s.push(match (mode & special != 0, x) {
            (true, true) => lower,
            (true, false) => upper,
            (false, true) => 'x',
            (false, false) => '-',
        });
    }
    s
}

/// Standard base64 with padding.
fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(ABC[((n >> shift) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The OSC 52 sequence that puts `text` on the system clipboard in
/// terminals that allow it (xterm, kitty, iTerm2, WezTerm, Windows
/// Terminal; tmux with set-clipboard on).
fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

/// `s` cut to at most `max` columns by dropping its start, marked with
/// `…`, so the end of a path stays readable. Columns, not characters:
/// a wide character takes two, so a CJK or emoji path still fits.
fn fit_left(s: &str, max: usize) -> String {
    if cols(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    // From the end, as much as fits beside the `…`.
    let mut keep = Vec::new();
    let mut used = 1;
    for c in s.chars().rev() {
        let w = char_cols(c);
        if used + w > max {
            break;
        }
        used += w;
        keep.push(c);
    }
    // A combining mark whose base was cut would sit on the `…`.
    while keep.last().is_some_and(|&c| char_cols(c) == 0) {
        keep.pop();
    }
    let keep: String = keep.iter().rev().collect();
    format!("…{keep}")
}

/// How a row shows its share of the view's total: ncdu's graph,
/// percentage, both, or neither (its `g` key; here `%`, as `g` jumps to
/// the first row).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Share {
    Bar,
    Percent,
    Both,
    Neither,
}

impl Share {
    fn next(self) -> Share {
        match self {
            Share::Bar => Share::Percent,
            Share::Percent => Share::Both,
            Share::Both => Share::Neither,
            Share::Neither => Share::Bar,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Share::Bar => "the share bar",
            Share::Percent => "the share in percent",
            Share::Both => "the share in percent and as a bar",
            Share::Neither => "no share",
        }
    }

    /// The share columns of a row, each with the space after it: a
    /// six-cell percent such as ` 12.5%`, then a bar in brackets, on a
    /// log scale with `log`. With `blank` (a size still coming), the same
    /// width of spaces.
    fn cols(self, part: u64, whole: u64, blank: bool, log: bool) -> String {
        let mut out = String::new();
        if matches!(self, Share::Percent | Share::Both) {
            if blank {
                out.push_str(&" ".repeat(7));
            } else {
                out.push_str(&format!("{} ", percent(part, whole)));
            }
        }
        if matches!(self, Share::Bar | Share::Both) {
            let cells = if blank {
                " ".repeat(10)
            } else if log {
                log_bar(part, whole, 10)
            } else {
                bar(part, whole, 10)
            };
            out.push_str(&format!("[{cells}] "));
        }
        out
    }
}

/// `part` of `whole` in six cells, one decimal: `  0.0%` up to `100.0%`.
/// An empty view gives `  0.0%`.
fn percent(part: u64, whole: u64) -> String {
    let pct = if whole == 0 {
        0.0
    } else {
        (part as f64 * 100.0 / whole as f64).min(100.0)
    };
    format!("{pct:>5.1}%")
}

/// Bar of `width` cells for `part` of `whole`.
fn bar(part: u64, whole: u64, width: usize) -> String {
    let filled = if whole == 0 {
        0
    } else {
        ((part as u128 * width as u128 + whole as u128 / 2) / whole as u128) as usize
    };
    let filled = filled.min(width);
    format!("{}{}", "#".repeat(filled), " ".repeat(width - filled))
}

/// Cells a log-scale bar loses for each tenth of the share.
const LOG_CELLS_PER_DECADE: f64 = 2.0;

/// Bar of `width` cells for `part` of `whole` on a log scale (ncdu #75):
/// the whole fills it, and [`LOG_CELLS_PER_DECADE`] cells fewer stand for
/// a share ten times smaller, so a row with 1% of a 10-cell view still
/// gets 6 cells. A part above 0 gets at least one cell.
fn log_bar(part: u64, whole: u64, width: usize) -> String {
    let filled = if part == 0 || whole == 0 {
        0
    } else {
        let share = (part as f64 / whole as f64).min(1.0);
        let cells = width as f64 + LOG_CELLS_PER_DECADE * share.log10();
        (cells.round().max(1.0) as usize).min(width)
    };
    format!("{}{}", "#".repeat(filled), " ".repeat(width - filled))
}

#[cfg(test)]
fn row_text(it: &Item, marked: bool, whole: u64, sort: SortKey) -> String {
    let ctx = RowCtx {
        apparent: sort == SortKey::Apparent,
        ..RowCtx::plain(whole, sort)
    };
    row_text_in(it, marked, &ctx)
}

/// What every row of one view shares when it is drawn.
struct RowCtx<'a> {
    /// The view's total, the bar's full width: its bytes, or its items
    /// ([`row_items`] summed) when `sort` is by items.
    whole: u64,
    sort: SortKey,
    /// Apparent sizes instead of disk use.
    apparent: bool,
    /// Directory sizes are still coming.
    pending: bool,
    /// Name each row by its path (the rows of several roots).
    whole_path: bool,
    /// The view's unreadable entries, for the `unreadable` and
    /// `incomplete` badges.
    unread: &'a du::Unread,
    /// What each row shows of its share of `whole`.
    share: Share,
    /// Bars on a log scale.
    log: bool,
}

#[cfg(test)]
impl RowCtx<'_> {
    /// The context tests start from: disk use, sizes in, rows named by
    /// file name, nothing unreadable, a linear bar. A test changes what it
    /// needs with `RowCtx { share, ..RowCtx::plain(whole, sort) }`, so a
    /// field added later is set here once, not at every test.
    fn plain(whole: u64, sort: SortKey) -> Self {
        RowCtx {
            whole,
            sort,
            apparent: false,
            pending: false,
            whole_path: false,
            unread: &[],
            share: Share::Bar,
            log: false,
        }
    }
}

/// [`row_text`] with the view's [`RowCtx`].
fn row_text_in(it: &Item, marked: bool, ctx: &RowCtx) -> String {
    let &RowCtx {
        whole,
        sort,
        apparent,
        pending,
        whole_path,
        unread,
        share,
        log,
    } = ctx;
    let name = if whole_path {
        it.path.to_string_lossy()
    } else {
        it.path.file_name().unwrap_or_default().to_string_lossy()
    };
    if pending && it.is_dir {
        let mark = if marked { "[x]" } else { "[ ]" };
        return format!("{mark} {:>10} {}{name}/", "…", share.cols(0, 0, true, log));
    }
    let mark = if marked { "[x]" } else { "[ ]" };
    let risky = it.git.as_ref().is_some_and(|(s, _)| s.is_risky());
    // ncdu's flag column: `@` for a symlink, unless git has a warning.
    let warn = if risky {
        "!"
    } else if is_link(it) {
        "@"
    } else {
        " "
    };
    let slash = if it.is_dir { "/" } else { "" };
    let mut badges = Vec::new();
    if let Some(a) = it.artefact.filter(|_| !is_link(it)) {
        badges.push(a.to_string());
    }
    // The same marks, in the same place, as the table's NOTE column.
    if let Some(m) = du::read_mark(it, unread) {
        badges.push(m.to_string());
    }
    if let Some((s, _)) = &it.git {
        badges.push(format!("git:{}", s.as_str()));
    }
    if sort == SortKey::Items && it.is_dir {
        badges.push(crate::util::plural(it.items, "item", "items"));
    }
    if sort == SortKey::Mtime {
        badges.push(du::format_age(it.mtime, crate::util::now_epoch()));
    }
    let badges = if badges.is_empty() {
        String::new()
    } else {
        format!("  [{}]", badges.join(" "))
    };
    let part = if sort == SortKey::Items {
        row_items(it)
    } else {
        key_of(it, apparent)
    };
    format!(
        "{mark}{warn}{:>10} {}{name}{slash}{badges}",
        format_bytes(key_of(it, apparent)),
        share.cols(part, whole, false, log)
    )
}

fn ui(f: &mut ratatui::Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(tui_term::FOOTER_ROWS),
        ])
        .split(f.area());
    app.viewport_rows = chunks[1].height.saturating_sub(2) as usize;
    app.ensure_scroll();
    let free = app.free_here();
    let sort = app.shown();
    let apparent = app.size_key == SortKey::Apparent;
    let view = app.view();
    let whole = if apparent { view.apparent } else { view.disk };
    let stopped = if view.partial { " (partial)" } else { "" };
    let size = match app.progress() {
        Some((files, disk)) => format!(
            "scanning: {} so far, {}",
            crate::util::plural(files, "file", "files"),
            format_bytes(disk)
        ),
        None => format!("{}{stopped}", format_bytes(whole)),
    };
    let entries = if !app.narrowed() {
        format!("{} entries", view.rows.len())
    } else {
        format!(
            "{} of {} entries",
            view.rows.len(),
            view.rows.len() + view.stash.len()
        )
    };
    let below = view_items(view);
    let items = crate::util::plural(below, "item", "items");
    let marked = app.marked_count();
    let tail = format!(" · {size} · {items} · {entries} · {marked} marked ");
    let place = if is_top_of_roots(&app.cur) {
        crate::util::plural(app.roots.len() as u64, "directory", "directories")
    } else {
        app.cur.display().to_string()
    };
    let title = title_text(chunks[0].width, &place, &tail, free.as_deref());
    let ctx = RowCtx {
        // By items, the share columns measure items, as gdu's bars do.
        whole: if sort == SortKey::Items { below } else { whole },
        sort,
        apparent,
        pending: view.scanning,
        whole_path: is_top_of_roots(&app.cur),
        unread: &view.unread,
        share: app.share,
        log: app.log_bars,
    };
    let items: Vec<ListItem> = view
        .rows
        .iter()
        .enumerate()
        .map(|(i, it)| ListItem::new(row_text_in(it, app.sel.is_marked(i), &ctx)))
        .collect();
    f.render_widget(
        Paragraph::new(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        ))
        .block(Block::default().borders(Borders::ALL).title(format!(
            "rtrash du{} · by {}{}{}{}",
            if app.snapshot.is_some() {
                " (snapshot, read-only)"
            } else if app.read_only {
                " (read-only)"
            } else {
                ""
            },
            app.sorting.label(),
            if apparent && app.sorting.order != Order::Key(SortKey::Apparent) {
                " · apparent sizes"
            } else {
                ""
            },
            if app.filter.is_empty() {
                String::new()
            } else {
                format!(" · matching {:?}", app.filter)
            },
            if app.hide_dots {
                " · dot names hidden"
            } else {
                ""
            }
        ))),
        chunks[0],
    );
    f.render_stateful_widget(
        List::new(items)
            .block(Block::default().borders(Borders::ALL))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD))
            .highlight_symbol("▸ "),
        chunks[1],
        &mut app.list_state,
    );
    let keys = match app.mode {
        Mode::Confirm => format!(
            "{} confirm  {} cancel",
            app.keys.display_chords(Action::ConfirmYes),
            app.keys.display_chords(Action::ConfirmNo)
        ),
        Mode::Filter => format!(
            "type a name part or a glob (* ? [)  {} keep  {} clear",
            app.keys.display_chords(Action::FilterCommit),
            app.keys.display_chords(Action::FilterCancel)
        ),
        Mode::Export => format!(
            "type a file name (a new one)  {} write  {} cancel",
            app.keys.display_chords(Action::FilterCommit),
            app.keys.display_chords(Action::FilterCancel)
        ),
        _ => format!(
            "{}/{} move  {}/→ open  ←/h up  {}{} mark  {} help  {} quit",
            app.keys.display_chords(Action::MoveUp),
            app.keys.display_chords(Action::MoveDown),
            app.keys.display_chords(Action::Action),
            if app.snapshot.is_some() {
                ""
            } else if app.read_only {
                "r rescan  "
            } else {
                "d trash  u undo  r rescan  "
            },
            app.keys.display_chords(Action::ToggleMark),
            app.keys.display_chords(Action::Help),
            app.keys.display_chords(Action::Quit),
        ),
    };
    f.render_widget(
        Paragraph::new(vec![Line::from(keys), Line::from(app.status.as_str())])
            .block(Block::default().borders(Borders::ALL).title("keys")),
        chunks[2],
    );
    if app.mode == Mode::Info {
        let r = f.area();
        let w = r.width.saturating_sub(4).min(100);
        // Long paths break by character, so the height counts every row.
        let lines = hard_wrap(&app.info, w.saturating_sub(2) as usize);
        let h = (lines.len() as u16 + 2).min(r.height);
        let area = ratatui::layout::Rect {
            x: r.x + (r.width - w) / 2,
            y: r.y + (r.height - h) / 2,
            width: w,
            height: h,
        };
        f.render_widget(ratatui::widgets::Clear, area);
        let lines: Vec<Line> = lines.into_iter().map(Line::from).collect();
        f.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("info · any key closes"),
            ),
            area,
        );
    }
    if app.mode == Mode::Marks {
        let r = f.area();
        let w = r.width.saturating_sub(4).min(100);
        let marks = app.all_marks();
        let room = r.height.saturating_sub(4).max(1) as usize;
        let first = app.marks_at.saturating_sub(room.saturating_sub(1));
        let lines: Vec<Line> = marks
            .iter()
            .enumerate()
            .skip(first)
            .take(room)
            .map(|(i, it)| {
                let head = format!(
                    "{} {:>9}  ",
                    if i == app.marks_at { ">" } else { " " },
                    format_bytes(it.disk)
                );
                let room = (w.saturating_sub(2) as usize).saturating_sub(cols(&head));
                let line = Line::from(format!(
                    "{head}{}",
                    fit_left(&crate::util::term_path(&it.path), room)
                ));
                if i == app.marks_at {
                    line.style(Style::default().add_modifier(Modifier::REVERSED))
                } else {
                    line
                }
            })
            .collect();
        let h = (lines.len() as u16 + 2).min(r.height);
        let area = ratatui::layout::Rect {
            x: r.x + (r.width - w) / 2,
            y: r.y + (r.height - h) / 2,
            width: w,
            height: h,
        };
        let total = du::size_sum(app.targets().iter().map(|t| t.disk));
        f.render_widget(ratatui::widgets::Clear, area);
        f.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(format!(
                "{} marked, {} · space unmark · enter go · d trash · other keys close",
                marks.len(),
                format_bytes(total)
            ))),
            area,
        );
    }
    if app.mode == Mode::Help {
        crate::tui_term::draw_help_overlay(
            f,
            &app.keys,
            &[
                "→ l enter — open directory · ← h backspace — up",
                if app.read_only {
                    "read-only (--no-delete): d and u are off"
                } else {
                    "d delete — move to the trash (asks first; runs in the background)"
                },
                if app.snapshot.is_some() {
                    "r E — off: a snapshot is browsed as it is, with no walk"
                } else if app.read_only {
                    "r — rescan this directory"
                } else {
                    "u — undo the last delete · e — why rows of it stayed · r — rescan"
                },
                "s n C M — sort by size, name, items or newest change; again reverses",
                "S — show apparent sizes instead of disk use, or back",
                "i — details of the cursor row (any key closes them)",
                "m — every mark, from all directories: space unmarks, enter goes there",
                "p — quit and print the marked paths (or the cursor row) · y — copy them",
                "E — write this directory as a du snapshot file, for du -f and du diff",
                if app.read_only {
                    "b — no shell in read-only mode"
                } else {
                    "b — open a shell in this directory; exit it to come back"
                },
                "v — show the cursor file in $PAGER · o — open the row with its default program",
                ". — hide names that start with a dot, or show them again",
                "% — show each row's share as a bar, in percent, both, or neither",
                "L — draw the bars on a log scale (two cells per tenth), or linear again",
                "/ — show only rows whose name matches (a part, or a glob with * ? [); Esc clears",
                "! — git work at risk (dirty, unpushed or unknown)",
            ],
        );
    }
}

fn event_loop(terminal: &mut tui_term::Term, app: &mut App) -> io::Result<()> {
    loop {
        app.poll_jobs();
        app.poll_delete();
        terminal.draw(|f| ui(f, app))?;
        if let Some(text) = app.clipboard.take() {
            let mut out = io::stdout();
            io::Write::write_all(&mut out, osc52(&text).as_bytes())?;
            io::Write::flush(&mut out)?;
        }
        match app.hand_off.take() {
            Some(HandOff::Shell(dir)) => {
                let banner = format!(
                    "{}: shell in {}; exit it to return to the explorer",
                    app.prog,
                    crate::util::term_path(&dir)
                );
                let status = tui_term::run_shell(terminal, &dir, &banner);
                app.after_shell(status);
                continue;
            }
            Some(HandOff::Pager(file)) => {
                let cmds = tui_term::pager_commands(&file);
                // Above several roots, the pager runs where the file is.
                let cwd = match file.parent() {
                    Some(p) if is_top_of_roots(&app.cur) => p.to_path_buf(),
                    _ => app.cur.clone(),
                };
                let status = tui_term::run_in_terminal(terminal, cmds, &cwd, None);
                app.after_pager(status);
                continue;
            }
            None => {}
        }
        if app.quit && app.deleting.is_some() {
            // A row partway across devices goes back whole first: stop,
            // and quit once the thread has cleaned up and ended.
            app.stop_delete();
        } else if app.quit {
            // Let running scans wind down instead of walking on unseen.
            for job in app.jobs.values() {
                job.control.stop.store(true, Ordering::Relaxed);
            }
            return Ok(());
        }
        // Redraw often while a scan runs so its counters move.
        let tick = if app.jobs.is_empty() && app.deleting.is_none() {
            200
        } else {
            100
        };
        if let Some((code, mods)) = tui_term::next_key(terminal, Duration::from_millis(tick))? {
            app.on_key(code, mods);
        }
    }
}

/// The directories `-i` explores: each given one resolved, repeats
/// dropped, and an error for a file or for one inside another (it would
/// show its sizes twice). The error is the exit code, already reported.
fn explorer_roots(prog: &str, roots: &[PathBuf]) -> Result<Vec<PathBuf>, i32> {
    let mut seen: Vec<PathBuf> = Vec::new();
    for given in roots {
        let root = match given.canonicalize() {
            Ok(p) => crate::util::plain_path(&p),
            Err(e) => {
                eprintln!(
                    "{prog}: cannot access '{}': {e}",
                    crate::util::err_path(&given)
                );
                return Err(1);
            }
        };
        if !root.is_dir() {
            eprintln!(
                "{prog}: -i needs a directory: '{}'",
                crate::util::err_path(&root)
            );
            return Err(2);
        }
        if seen.contains(&root) {
            continue;
        }
        if let Some(outer) = seen
            .iter()
            .find(|s| root.starts_with(s) || s.starts_with(&root))
        {
            eprintln!(
                "{prog}: -i: '{}' and '{}' overlap; give one of them",
                crate::util::err_path(&outer),
                crate::util::err_path(&root)
            );
            return Err(2);
        }
        seen.push(root);
    }
    Ok(seen)
}

/// Run the explorer on `roots`: one directory, or several under a view
/// of their own. Returns the process exit code.
pub fn run(
    prog: &str,
    roots: &[PathBuf],
    opts: ScanOpts,
    git: bool,
    read_only: bool,
    allow_filtered: bool,
) -> i32 {
    let seen = match explorer_roots(prog, roots) {
        Ok(r) => r,
        Err(code) => return code,
    };
    let mut app = match App::start(prog, seen, opts, git) {
        Ok(a) => App {
            read_only,
            allow_filtered,
            ..a
        },
        Err(e) => {
            eprintln!("{prog}: cannot scan: {e}");
            return 1;
        }
    };
    drive(prog, &mut app)
}

/// Browse the snapshot or ncdu export `snap`, read from `file`, with no
/// walk (`du -f FILE -i`): read-only, and `r` and `E` explain instead of
/// acting. Returns the process exit code.
pub fn run_snapshot(
    prog: &str,
    file: &Path,
    snap: &crate::du_snap::Snapshot,
    opts: ScanOpts,
) -> i32 {
    match App::from_snapshot(prog, file, snap, opts) {
        Ok(mut app) => drive(prog, &mut app),
        Err(why) => {
            eprintln!(
                "{prog}: cannot browse '{}': {why}",
                crate::util::err_path(&file)
            );
            2
        }
    }
}

/// Run `app` in the terminal until it quits, then print what `p` asked
/// for. Returns the process exit code.
fn drive(prog: &str, app: &mut App) -> i32 {
    let mut terminal = match tui_term::enter() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{prog}: cannot start TUI: {e}");
            return 1;
        }
    };
    let result = event_loop(&mut terminal, app);
    tui_term::leave(&mut terminal);
    if let Err(e) = result {
        eprintln!("{prog}: tui error: {e}");
        return 1;
    }
    let mut out = io::stdout().lock();
    for p in &app.print_on_exit {
        let _ = io::Write::write_all(&mut out, p.as_os_str().as_encoded_bytes());
        let _ = io::Write::write_all(&mut out, b"\n");
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Tree(PathBuf);
    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn tree(tag: &str) -> Tree {
        let root = std::env::temp_dir().join(format!("rtrash-dutui-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let w = root.join("w");
        fs::create_dir_all(w.join("big/inner")).unwrap();
        fs::create_dir_all(w.join("small")).unwrap();
        fs::write(w.join("big/inner/blob"), vec![7u8; 200_000]).unwrap();
        fs::write(w.join("big/side"), vec![7u8; 30_000]).unwrap();
        fs::write(w.join("small/s"), b"s").unwrap();
        fs::write(w.join("file"), vec![1u8; 9000]).unwrap();
        let root = fs::canonicalize(&root).unwrap();
        // The scan lists plain `C:\...` paths, not verbatim ones.
        #[cfg(windows)]
        let root = PathBuf::from(
            root.to_string_lossy()
                .trim_start_matches(r"\\?\")
                .to_string(),
        );
        Tree(root)
    }

    fn app(t: &Tree) -> App {
        let mut a = App::new("rtrash", t.0.join("w"), ScanOpts::default(), false).unwrap();
        a.trash_pin = Some(t.0.join("Trash"));
        a.free_of = no_free;
        a
    }

    /// No free space to show, so a title never depends on the host's disk.
    fn no_free(_: &Path) -> Option<crate::space::Space> {
        None
    }

    /// A file system with 1 GiB of 4 GiB free, whatever the host has.
    fn quarter_free(_: &Path) -> Option<crate::space::Space> {
        Some(crate::space::Space {
            total: 4 << 30,
            avail: 1 << 30,
            ..Default::default()
        })
    }

    fn names(a: &App) -> Vec<String> {
        a.rows()
            .iter()
            .map(|r| r.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn a_background_delete_counts_down_and_blocks_what_would_clash() {
        let t = tree("bgdel");
        let mut a = app(&t);
        a.background = true;
        let top_before = a.view().disk;
        // Rows: big, file, small. Mark big and small.
        a.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        a.select(2);
        a.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        a.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        a.on_key(KeyCode::Char('y'), KeyModifiers::NONE);
        // Nothing is taken in until the next poll, so the delete is still
        // running here however fast its thread is.
        assert!(
            a.status.starts_with("trashing: 0 of 2 done, "),
            "{}",
            a.status
        );
        assert!(a.status.ends_with(" · q stops"), "{}", a.status);
        // A queued row cannot be opened, and d, u and r wait.
        a.select(0);
        a.open();
        assert!(a.status.contains("is being trashed"), "{}", a.status);
        assert_eq!(a.cur, t.0.join("w"));
        a.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(a.status.contains("a delete is running"), "{}", a.status);
        assert_eq!(a.mode, Mode::Browse);
        a.on_key(KeyCode::Char('u'), KeyModifiers::NONE);
        assert!(a.status.contains("undo once it is done"), "{}", a.status);
        a.on_key(KeyCode::Char('r'), KeyModifiers::NONE);
        assert!(a.status.contains("rescan once it is done"), "{}", a.status);
        settle(&mut a);
        assert!(a.status.starts_with("trashed 2 item(s)"), "{}", a.status);
        assert!(!t.0.join("w/big").exists() && !t.0.join("w/small").exists());
        assert_eq!(names(&a), ["file"]);
        assert!(a.view().disk < top_before);
        a.on_key(KeyCode::Char('u'), KeyModifiers::NONE);
        assert_eq!(a.status, "restored 2 item(s)");
        assert!(t.0.join("w/big/inner/blob").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_stopped_delete_trashes_nothing_more() {
        let t = tree("stopdel");
        let a = app(&t);
        let items: Vec<Item> = a.rows().to_vec();
        let (tx, rx) = mpsc::channel();
        let watch = Watch::default();
        watch.stop.store(true, Ordering::Relaxed);
        trash_rows("rtrash", items.clone(), a.trash_pin.as_ref(), &watch, &tx);
        assert!(rx.try_recv().is_err(), "a stop set first trashes no row");
        assert!(t.0.join("w/big").exists() && t.0.join("w/file").exists());
        watch.stop.store(false, Ordering::Relaxed);
        trash_rows("rtrash", items, a.trash_pin.as_ref(), &watch, &tx);
        drop(tx);
        let sent: Vec<Trashed> = rx.iter().collect();
        assert_eq!(sent.len(), 3);
        assert!(sent.iter().all(|(_, ticket)| ticket.is_ok()));
        assert!(!t.0.join("w/big").exists() && !t.0.join("w/file").exists());
    }

    /// A delete whose thread is held still: the test plays the thread,
    /// moving `watch.copied` and ending it by dropping the sender.
    fn held_delete(a: &mut App, row: &Item) -> (mpsc::Sender<Trashed>, Watch) {
        let (tx, rx) = mpsc::channel();
        let watch = Watch::default();
        a.deleting = Some(Deleting {
            rx,
            watch: watch.clone(),
            pending: vec![(row.path.clone(), row.apparent)],
            total: 1,
            left: row.disk,
            group: Vec::new(),
            failed: Vec::new(),
            shown: (0, 0),
        });
        (tx, watch)
    }

    #[test]
    fn the_status_line_counts_the_bytes_copied_of_the_row_in_hand() {
        let t = tree("copyline");
        let mut a = app(&t);
        let big = a.rows()[0].clone();
        let (tx, watch) = held_delete(&mut a, &big);
        watch.copied.store(100_000, Ordering::Relaxed);
        a.poll_delete();
        let line = format!(
            "; copying big: {} of {} · q stops",
            format_bytes(100_000),
            format_bytes(big.apparent)
        );
        assert!(a.status.ends_with(&line), "{}", a.status);
        // With nothing new, a refusal stays readable.
        a.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        a.poll_delete();
        assert!(a.status.contains("a delete is running"), "{}", a.status);
        watch.copied.store(200_000, Ordering::Relaxed);
        a.poll_delete();
        assert!(
            a.status
                .contains(&format!("copying big: {} of", format_bytes(200_000))),
            "{}",
            a.status
        );
        a.stop_delete();
        assert!(watch.stop.load(Ordering::Relaxed));
        assert_eq!(
            a.status,
            "stopping: 0 of 1 done; a copy in progress is undone"
        );
        // The thread undid the copy and ended without sending the row.
        drop(tx);
        a.poll_delete();
        assert!(a.deleting.is_none());
        assert_eq!(a.status, "stopped; nothing trashed, 1 left in place");
        assert_eq!(names(&a), ["big", "file", "small"]);
        a.on_key(KeyCode::Char('u'), KeyModifiers::NONE);
        assert_eq!(a.status, "nothing to undo");
    }

    /// The explorer's own trash call, stopped inside a copy to another
    /// file system: nothing reaches the trash and the row stays whole.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_trash_stopped_mid_copy_leaves_the_row_whole() {
        use std::os::unix::fs::MetadataExt;
        let t = tree("midcopy");
        let shm = Path::new("/dev/shm");
        let dev = fs::metadata(&t.0).unwrap().dev();
        if !fs::metadata(shm).is_ok_and(|m| m.dev() != dev) {
            eprintln!("no second file system here; skipping");
            return;
        }
        let pin = shm.join(format!("rtrash-dutui-midcopy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&pin);
        let watch = Watch::default();
        watch.stop.store(true, Ordering::Relaxed);
        let big = t.0.join("w/big");
        assert!(bin::trash("rtrash", &big, Some(&pin), &watch).is_err());
        assert_eq!(fs::read(big.join("inner/blob")).unwrap().len(), 200_000);
        assert_eq!(fs::read(big.join("side")).unwrap().len(), 30_000);
        let in_trash = fs::read_dir(pin.join("files")).map_or(0, |d| d.count());
        assert_eq!(in_trash, 0);
        // Unstopped, the same call copies it and counts the bytes.
        watch.stop.store(false, Ordering::Relaxed);
        assert!(bin::trash("rtrash", &big, Some(&pin), &watch).is_ok());
        assert!(!big.exists());
        assert_eq!(watch.copied.load(Ordering::Relaxed), 230_000);
        let _ = fs::remove_dir_all(&pin);
    }

    #[cfg(unix)]
    #[test]
    fn deleting_the_directory_on_screen_shows_its_parent() {
        let t = tree("delhere");
        let mut a = app(&t);
        // Mark big in the top view, then open it and trash the mark.
        a.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        a.select(0);
        a.open();
        assert_eq!(a.cur, t.0.join("w/big"));
        a.ask_delete();
        assert!(a.status.starts_with("trash 1 item(s)"), "{}", a.status);
        a.on_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(a.status.starts_with("trashed 1 item(s)"), "{}", a.status);
        assert!(!t.0.join("w/big").exists());
        assert_eq!(a.cur, t.0.join("w"));
        assert_eq!(names(&a), ["file", "small"]);
    }

    #[test]
    fn a_delete_outside_the_scope_trashes_nothing() {
        let t = tree("scope");
        let mut a = app(&t);
        // Rows: big, file, small. Only small is in scope, so file is out.
        a.move_sel(1);
        for roots in [vec![t.0.join("w/small")], Vec::new()] {
            a.scope = Some(roots);
            a.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
            assert!(a.status.starts_with("trash 1 item(s)"), "{}", a.status);
            a.on_key(KeyCode::Char('y'), KeyModifiers::NONE);
            assert_eq!(
                a.status,
                "1 item(s) outside RTRASH_ALLOW_ROOTS; nothing was trashed"
            );
            assert!(t.0.join("w/file").exists());
            assert_eq!(names(&a), ["big", "file", "small"]);
            assert!(a.undo.is_empty());
        }
    }

    #[test]
    fn v_pages_files_only() {
        let t = tree("view");
        let mut a = app(&t);
        // Rows: big (a directory), file, small.
        a.on_key(KeyCode::Char('v'), KeyModifiers::NONE);
        assert!(a.hand_off.is_none());
        assert!(a.status.contains("o opens a directory"), "{}", a.status);
        a.move_sel(1);
        a.on_key(KeyCode::Char('v'), KeyModifiers::NONE);
        assert_eq!(a.hand_off.take(), Some(HandOff::Pager(t.0.join("w/file"))));
        a.after_pager(Err(io::Error::from(io::ErrorKind::NotFound)));
        assert!(a.status.contains("set PAGER"), "{}", a.status);
    }

    #[cfg(unix)]
    #[test]
    fn o_opens_the_row_with_the_opener_and_keeps_the_screen() {
        let t = tree("open");
        let mut a = app(&t);
        // The opener runs detached, as a real one does, so the test cannot
        // wait on it. It copies to a temporary name and renames that into
        // place: once file.opened exists, it holds all 9000 bytes. A plain
        // cp made the name first and the bytes after, and macos-15-intel
        // read it empty in between.
        let script = "cp \"$1\" \"$1.part\" && mv \"$1.part\" \"$1.opened\"";
        a.opener = ["/bin/sh", "-c", script, "sh"]
            .map(std::ffi::OsString::from)
            .to_vec();
        a.move_sel(1); // file
        a.on_key(KeyCode::Char('o'), KeyModifiers::NONE);
        assert!(a.hand_off.is_none());
        assert!(a.status.starts_with("opening "), "{}", a.status);
        let copy = t.0.join("w/file.opened");
        for _ in 0..100 {
            if copy.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(fs::read(&copy).unwrap().len(), 9000);
        a.opener = vec!["/nonexistent/opener".into()];
        a.on_key(KeyCode::Char('o'), KeyModifiers::NONE);
        assert!(a.status.starts_with("cannot run "), "{}", a.status);
    }

    #[test]
    fn b_asks_for_a_shell_in_the_current_directory_unless_read_only() {
        let t = tree("shell");
        let mut a = app(&t);
        a.open(); // big
        a.on_key(KeyCode::Char('b'), KeyModifiers::NONE);
        assert_eq!(a.hand_off.take(), Some(HandOff::Shell(t.0.join("w/big"))));
        // Back from the shell, the directory is read again.
        fs::write(t.0.join("w/big/new"), vec![1u8; 50_000]).unwrap();
        a.after_shell(Err(io::Error::from(io::ErrorKind::NotFound)));
        assert!(a.status.starts_with("cannot start "), "{}", a.status);
        assert!(names(&a).contains(&"new".to_string()), "{:?}", names(&a));
        a.read_only = true;
        a.on_key(KeyCode::Char('b'), KeyModifiers::NONE);
        assert!(a.hand_off.is_none());
        assert!(a.status.contains("no shell"), "{}", a.status);
    }

    #[test]
    fn opens_descends_and_returns_to_the_same_row() {
        let t = tree("nav");
        let mut a = app(&t);
        assert_eq!(names(&a), ["big", "file", "small"]);
        a.open();
        assert_eq!(a.cur, t.0.join("w/big"));
        assert_eq!(names(&a), ["inner", "side"]);
        a.back();
        assert_eq!(a.cur, t.0.join("w"));
        assert_eq!(a.selected().unwrap().path, t.0.join("w/big"));
        a.back();
        assert_eq!(a.status, "already at the top");
    }

    // Unix only for the same reason as the test below.
    #[cfg(unix)]
    #[test]
    fn marks_follow_the_user_across_views_and_trash_as_one_batch() {
        let t = tree("batch");
        let mut a = app(&t);
        let top_before = a.view().disk;
        // Rows: big, file, small. Mark file and small here, side in big.
        a.select(1);
        a.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        a.select(2);
        a.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        a.select(0);
        a.open();
        assert_eq!(names(&a), ["inner", "side"]);
        assert_eq!(a.marked_count(), 2);
        a.select(1);
        a.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(a.status, "3 marked");
        let text = tui_term::render_text(100, 14, |f| ui(f, &mut a));
        assert!(text.contains("3 marked"), "{text}");
        // Back up, the earlier marks are still on their rows.
        a.back();
        assert!(a.sel.is_marked(1) && a.sel.is_marked(2) && a.sel.len() == 2);
        // p prints every mark, sorted, the subtree kept once.
        let mut paths = a.chosen_paths();
        paths.sort();
        assert_eq!(
            paths,
            [
                t.0.join("w/big/side"),
                t.0.join("w/file"),
                t.0.join("w/small")
            ]
        );
        a.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(
            a.status.starts_with("trash 3 item(s) in 2 directories"),
            "{}",
            a.status
        );
        a.on_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(a.status.starts_with("trashed 3 item(s)"), "{}", a.status);
        for gone in ["w/big/side", "w/file", "w/small"] {
            assert!(!t.0.join(gone).exists(), "{gone}");
        }
        assert_eq!(names(&a), ["big"]);
        assert_eq!(a.marked_count(), 0);
        assert!(a.view().disk < top_before);
        a.open();
        assert_eq!(names(&a), ["inner"]);
        a.back();
        a.on_key(KeyCode::Char('u'), KeyModifiers::NONE);
        assert_eq!(a.status, "restored 3 item(s)");
        for back in ["w/big/side", "w/file", "w/small"] {
            assert!(t.0.join(back).exists(), "{back}");
        }
        assert_eq!(a.view().disk, top_before);
    }

    #[test]
    fn m_lists_every_mark_unmarks_and_goes_to_one() {
        let t = tree("markpanel");
        let mut a = app(&t);
        a.on_key(KeyCode::Char('m'), KeyModifiers::NONE);
        assert_eq!(a.mode, Mode::Browse);
        assert_eq!(a.status, "nothing marked");
        // Mark file here and side inside big, then come back up.
        a.select(1);
        a.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        a.select(0);
        a.open();
        a.select(1);
        a.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        a.back();
        a.on_key(KeyCode::Char('m'), KeyModifiers::NONE);
        assert_eq!(a.mode, Mode::Marks);
        let text = tui_term::render_text(100, 14, |f| ui(f, &mut a));
        assert!(text.contains("2 marked"), "{text}");
        assert!(text.contains("side") && text.contains("file"), "{text}");
        // Paths sort big/side before file; enter goes to big on side.
        a.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(a.mode, Mode::Browse);
        assert_eq!(a.cur, t.0.join("w/big"));
        assert_eq!(a.selected().unwrap().path, t.0.join("w/big/side"));
        assert!(a.sel.is_marked(1));
        // Unmark file, which sits in another view, from the panel.
        a.on_key(KeyCode::Char('m'), KeyModifiers::NONE);
        a.on_key(KeyCode::Char('j'), KeyModifiers::NONE);
        a.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(a.mode, Mode::Marks);
        assert_eq!(a.marked_count(), 1);
        assert_eq!(a.chosen_paths(), [t.0.join("w/big/side")]);
        // The last unmark closes the panel.
        a.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(a.mode, Mode::Browse);
        assert_eq!(a.marked_count(), 0);
    }

    #[test]
    fn marks_survive_a_rescan_and_drop_entries_that_went() {
        let t = tree("markrescan");
        let mut a = app(&t);
        // Rows: big, file, small. Mark file and small, and side in big.
        a.select(0);
        a.open();
        a.select(1);
        a.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        a.back();
        a.select(1);
        a.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        a.select(2);
        a.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        fs::write(t.0.join("w/file"), vec![1u8; 90_000]).unwrap();
        fs::remove_dir_all(t.0.join("w/small")).unwrap();
        a.on_key(KeyCode::Char('r'), KeyModifiers::NONE);
        // small went with the rescan; file keeps its mark and new size.
        let marks = a.all_marks();
        let names: Vec<_> = marks.iter().map(|m| m.path.clone()).collect();
        assert_eq!(names, [t.0.join("w/big/side"), t.0.join("w/file")]);
        let file = a
            .rows()
            .iter()
            .position(|r| r.path == t.0.join("w/file"))
            .unwrap();
        assert!(a.sel.is_marked(file) && a.sel.len() == 1);
        assert!(marks[1].apparent >= 90_000, "{:?}", marks[1].apparent);
    }

    /// A mark under a marked directory goes with the directory.
    #[test]
    fn a_mark_inside_a_marked_directory_is_not_counted_twice() {
        let t = tree("nested");
        let mut a = app(&t);
        a.open(); // big
        a.select(1);
        a.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        a.back();
        a.select(0);
        a.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(a.marked_count(), 2);
        assert_eq!(a.chosen_paths(), [t.0.join("w/big")]);
        a.on_key(KeyCode::Char('A'), KeyModifiers::SHIFT);
        assert_eq!(a.marked_count(), 0);
    }

    // Windows deletes go to the real Recycle Bin, so this stays on Unix,
    // where the pinned trash dir keeps it inside the test tree.
    #[cfg(unix)]
    #[test]
    fn delete_trashes_updates_parents_and_undo_restores() {
        let t = tree("del");
        let mut a = app(&t);
        let top_before = a.view().disk;
        a.open(); // big
        let big_before = a.view().disk;
        let inner = a.rows()[0].clone();
        assert_eq!(inner.path, t.0.join("w/big/inner"));
        a.select(0);
        a.ask_delete();
        assert_eq!(a.mode, Mode::Confirm);
        a.on_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(!t.0.join("w/big/inner").exists());
        assert!(t.0.join("Trash/files/inner").exists());
        assert_eq!(names(&a), ["side"]);
        assert_eq!(a.view().disk, big_before - inner.disk);
        let top = &a.cache[&t.0.join("w")];
        assert_eq!(top.disk, top_before - inner.disk);
        let big_row = top
            .rows
            .iter()
            .find(|r| r.path == t.0.join("w/big"))
            .unwrap();
        assert_eq!(big_row.disk, big_before - inner.disk);

        a.on_key(KeyCode::Char('u'), KeyModifiers::NONE);
        assert!(t.0.join("w/big/inner/blob").is_file(), "{}", a.status);
        assert_eq!(names(&a), ["inner", "side"]);
        assert_eq!(a.view().disk, big_before);
        assert_eq!(a.cache[&t.0.join("w")].disk, top_before);
    }

    /// A symlink is an `@` row of its own size, as in ncdu: never
    /// entered, and trashed as the link alone. Unix only, as above.
    #[cfg(unix)]
    #[test]
    fn links_are_at_rows_that_stay_closed_and_trash_alone() {
        use std::os::unix::fs::{symlink, MetadataExt};
        let t = tree("links");
        let w = t.0.join("w");
        symlink("big", w.join("to-big")).unwrap();
        symlink("file", w.join("to-file")).unwrap();
        symlink("nowhere", w.join("dangling")).unwrap();
        let quick = quick_view(&w).unwrap();
        let mut a = app(&t);
        for name in ["to-big", "to-file", "dangling"] {
            let row = |v: &[Item]| v.iter().find(|r| r.path == w.join(name)).cloned();
            let q = row(&quick.rows).expect(name);
            let it = row(a.rows()).expect(name);
            let m = fs::symlink_metadata(w.join(name)).unwrap();
            for r in [&q, &it] {
                assert!(is_link(r) && !r.is_dir, "{name}: {r:?}");
                assert_eq!((r.apparent, r.disk), (m.len(), m.blocks() * 512));
            }
            let s = row_text(&it, false, a.view().disk, SortKey::Disk);
            assert!(s.starts_with("[ ]@"), "{s}");
            assert!(s.ends_with(&format!(" {name}")), "{s}");
        }
        let at = a.rows().iter().position(|r| r.path == w.join("to-big"));
        let at = at.unwrap();
        let info = info_lines(&a.rows()[at], a.view().disk, false, 0);
        assert!(
            info.contains(&"kind      symbolic link".to_string()),
            "{info:?}"
        );
        assert!(info.contains(&"target    big".to_string()), "{info:?}");
        a.select(at);
        a.open();
        assert_eq!(a.cur, w);
        assert_eq!(a.status, "not a directory");
        a.ask_delete();
        assert_eq!(a.mode, Mode::Confirm);
        a.on_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(
            fs::symlink_metadata(w.join("to-big")).is_err(),
            "{}",
            a.status
        );
        assert!(w.join("big/inner/blob").is_file());
        let kept = fs::symlink_metadata(t.0.join("Trash/files/to-big")).unwrap();
        assert!(kept.file_type().is_symlink());
        assert!(!names(&a).contains(&"to-big".to_string()));
        a.on_key(KeyCode::Char('u'), KeyModifiers::NONE);
        let back = fs::symlink_metadata(w.join("to-big")).unwrap();
        assert!(back.file_type().is_symlink(), "{}", a.status);
    }

    #[test]
    fn rescan_picks_up_outside_changes_and_carries_them_up() {
        let t = tree("rescan");
        let mut a = app(&t);
        a.open(); // big
        fs::write(t.0.join("w/big/new"), vec![3u8; 100_000]).unwrap();
        let before = a.view().disk;
        a.refresh();
        assert!(names(&a).contains(&"new".to_string()));
        let grown = a.view().disk - before;
        assert!(grown >= 100_000, "{grown}");
        let top = &a.cache[&t.0.join("w")];
        let row = top
            .rows
            .iter()
            .find(|r| r.path == t.0.join("w/big"))
            .unwrap();
        assert_eq!(row.disk, a.cache[&t.0.join("w/big")].disk);
    }

    #[test]
    fn rows_show_size_bar_and_badges() {
        let mut it = Item {
            path: PathBuf::from("/x/target"),
            is_dir: true,
            apparent: 0,
            disk: 512,
            mtime: 0,
            items: 3,
            artefact: Some("cargo-target"),
            repos: Vec::new(),
            git: Some((crate::du_git::GitState::Dirty, PathBuf::from("/x"))),
            unique: None,
            filtered_out: 0,
        };
        let s = row_text(&it, true, 1024, SortKey::Disk);
        assert!(s.starts_with("[x]!"), "{s}");
        assert!(s.contains("[#####     ] target/"), "{s}");
        assert!(s.ends_with("[cargo-target git:dirty]"), "{s}");
        it.git = None;
        assert!(!row_text(&it, false, 1024, SortKey::Disk).contains('!'));
        let s = row_text(&it, false, 1024, SortKey::Items);
        assert!(s.ends_with("[cargo-target 3 items]"), "{s}");
    }

    #[test]
    fn rows_show_the_share_as_bar_percent_both_or_neither() {
        let it = Item {
            path: PathBuf::from("/x/half"),
            is_dir: true,
            apparent: 0,
            disk: 512,
            mtime: 0,
            items: 1,
            artefact: None,
            repos: Vec::new(),
            git: None,
            unique: None,
            filtered_out: 0,
        };
        let text = |share: Share, pending: bool| {
            let ctx = RowCtx {
                pending,
                share,
                ..RowCtx::plain(1024, SortKey::Disk)
            };
            row_text_in(&it, false, &ctx)
        };
        let size = format_bytes(512);
        assert!(text(Share::Bar, false).ends_with(&format!("{size} [#####     ] half/")));
        assert!(text(Share::Percent, false).ends_with(&format!("{size}  50.0% half/")));
        assert!(text(Share::Both, false).ends_with(&format!("{size}  50.0% [#####     ] half/")));
        assert!(text(Share::Neither, false).ends_with(&format!("{size} half/")));
        // A size still coming keeps the columns where they will be.
        for share in [Share::Bar, Share::Percent, Share::Both, Share::Neither] {
            let (done, coming) = (text(share, false), text(share, true));
            assert_eq!(
                done.chars().count(),
                coming.chars().count(),
                "{done:?} {coming:?}"
            );
            assert!(!coming.contains('%') && !coming.contains('#'), "{coming:?}");
        }
        assert_eq!(percent(0, 0), "  0.0%");
        assert_eq!(percent(5, 1000), "  0.5%");
        assert_eq!(percent(1, 3), " 33.3%");
        assert_eq!(percent(1024, 1024), "100.0%");
        assert_eq!(percent(u64::MAX, 1), "100.0%");
    }

    /// By items the share columns measure items, not bytes: the directory
    /// that holds most entries gets the longest bar even when it is the
    /// smaller one, and the percents add up to the title's item total.
    #[test]
    fn by_items_the_share_columns_count_items() {
        let row = |name: &str, is_dir: bool, disk: u64, items: u64| Item {
            path: PathBuf::from("/x").join(name),
            is_dir,
            apparent: disk,
            disk,
            mtime: 0,
            items,
            artefact: None,
            repos: Vec::new(),
            git: None,
            unique: None,
            filtered_out: 0,
        };
        // 7 + 2 + 1 = 10 items: many small files, one big folder, a file.
        let many = row("many", true, 100, 6);
        let big = row("big", true, 900, 1);
        let file = row("f.bin", false, 24, 1);
        let text = |it: &Item, sort: SortKey, whole: u64| {
            let ctx = RowCtx {
                share: Share::Both,
                ..RowCtx::plain(whole, sort)
            };
            row_text_in(it, false, &ctx)
        };
        let by_items = |it: &Item| text(it, SortKey::Items, 10);
        assert!(
            by_items(&many).contains(" 70.0% [#######   ] many/"),
            "{}",
            by_items(&many)
        );
        assert!(
            by_items(&big).contains(" 20.0% [##        ] big/"),
            "{}",
            by_items(&big)
        );
        assert!(
            by_items(&file).contains(" 10.0% [#         ] f.bin"),
            "{}",
            by_items(&file)
        );
        // The size column still shows bytes, and by size the bars do too.
        assert!(
            by_items(&big).contains(&format_bytes(900)),
            "{}",
            by_items(&big)
        );
        let by_disk = text(&big, SortKey::Disk, 1024);
        assert!(by_disk.contains(" 87.9% [#########"), "{by_disk}");
        assert_eq!(row_items(&big), 2);
        assert_eq!(row_items(&file), 1);
    }

    #[test]
    fn items_order_draws_item_bars_on_screen() {
        let t = tree("itembars");
        let mut a = app(&t);
        a.keys = Keymap::builtin();
        let screen = |a: &mut App| tui_term::render_text(100, 14, |f| ui(f, a));
        let drawn = |screen: &str, part: &dyn Fn(&Item) -> u64, whole: u64, a: &App| {
            for it in &a.view().rows {
                let name = it.path.file_name().unwrap().to_string_lossy();
                let want = format!("[{}] {name}", bar(part(it), whole, 10));
                assert!(screen.contains(&want), "{want:?} in {screen}");
            }
        };
        // By disk use, each bar is the row's bytes of the view's.
        let by_size = screen(&mut a);
        drawn(&by_size, &|it: &Item| it.disk, a.view().disk, &a);
        a.on_key(KeyCode::Char('C'), KeyModifiers::SHIFT);
        assert_eq!(a.sorting.label(), "items");
        // By items, the row's items of the title's total.
        let by_items = screen(&mut a);
        let total: u64 = a.view().rows.iter().map(row_items).sum();
        drawn(&by_items, &row_items, total, &a);
        // The tree's big folder holds most bytes but not all the items.
        assert_ne!(by_size, by_items);
    }

    #[test]
    fn log_bars_show_small_shares_and_fill_for_the_whole() {
        assert_eq!(log_bar(100, 100, 10), "#".repeat(10));
        assert_eq!(
            log_bar(1, 100, 10),
            format!("{}{}", "#".repeat(6), " ".repeat(4))
        );
        assert_eq!(log_bar(1, 1 << 40, 10), format!("#{}", " ".repeat(9)));
        assert_eq!(log_bar(0, 100, 10), " ".repeat(10));
        assert_eq!(log_bar(5, 0, 10), " ".repeat(10));
    }

    #[test]
    fn capital_l_switches_the_bars_to_a_log_scale_and_back() {
        let t = tree("logbars");
        let mut a = app(&t);
        a.keys = Keymap::builtin();
        a.on_key(KeyCode::Char('L'), KeyModifiers::SHIFT);
        assert!(a.log_bars);
        assert!(a.status.starts_with("bars use a log scale"), "{}", a.status);
        let text = tui_term::render_text(100, 14, |f| ui(f, &mut a));
        assert!(text.contains("] big/"), "{text}");
        a.on_key(KeyCode::Char('L'), KeyModifiers::SHIFT);
        assert!(!a.log_bars);
    }

    #[test]
    fn percent_key_cycles_the_share_columns() {
        let t = tree("share");
        let mut a = app(&t);
        a.keys = Keymap::builtin();
        let rows = |a: &mut App| tui_term::render_text(100, 14, |f| ui(f, a));
        assert!(rows(&mut a).contains("] big/"));
        let mut seen = Vec::new();
        for _ in 0..4 {
            a.on_key(KeyCode::Char('%'), KeyModifiers::SHIFT);
            assert!(a.status.starts_with("rows show "), "{}", a.status);
            let text = rows(&mut a);
            let percent = text.contains("% big/") || text.contains("% [");
            seen.push((percent, text.contains("] big/")));
        }
        // Percent, both, neither, then the bar again.
        assert_eq!(
            seen,
            [(true, false), (true, true), (false, false), (false, true)]
        );
        assert_eq!(a.share, Share::Bar);
    }

    #[test]
    fn row_badges_carry_the_read_marks() {
        let it = |path: &str, is_dir: bool| Item {
            path: PathBuf::from(path),
            is_dir,
            apparent: 0,
            disk: 512,
            mtime: 0,
            items: 1,
            artefact: None,
            repos: Vec::new(),
            git: None,
            unique: None,
            filtered_out: 0,
        };
        let text = |row: &Item, unread: &du::Unread| {
            let ctx = RowCtx {
                unread,
                ..RowCtx::plain(1024, SortKey::Disk)
            };
            row_text_in(row, false, &ctx)
        };
        let here = [(PathBuf::from("/x/a"), "denied".to_string())];
        let below = [(PathBuf::from("/x/a/b"), "denied".to_string())];
        let s = text(&it("/x/a", true), &here);
        assert!(s.ends_with("a/  [unreadable]"), "{s}");
        let s = text(&it("/x/a", true), &below);
        assert!(s.ends_with("a/  [incomplete]"), "{s}");
        let s = text(&it("/x/c", true), &below);
        assert!(s.ends_with("c/"), "{s}");
        // A file is never marked, as in the table.
        let s = text(&it("/x/a", false), &here);
        assert!(s.ends_with(" a"), "{s}");
    }

    /// A folder the scan cannot open shows `unreadable`, a folder with one
    /// below it `incomplete`, as the du table marks them; opening the
    /// incomplete folder shows the unreadable one inside. As root nothing
    /// is locked, so no row is marked.
    #[cfg(unix)]
    #[test]
    fn rows_mark_unreadable_and_incomplete_folders() {
        use std::os::unix::fs::PermissionsExt;
        let t = tree("unread");
        let w = t.0.join("w");
        let (locked, shut) = (w.join("ur-outer/ur-locked"), w.join("ur-shut"));
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("x"), b"x").unwrap();
        fs::create_dir_all(&shut).unwrap();
        fs::write(shut.join("y"), b"y").unwrap();
        let mode =
            |p: &Path, m: u32| fs::set_permissions(p, fs::Permissions::from_mode(m)).unwrap();
        mode(&locked, 0o000);
        mode(&shut, 0o000);
        let blocked = fs::read_dir(&locked).is_err() && fs::read_dir(&shut).is_err();
        let mut a = app(&t);
        let top = tui_term::render_text(120, 20, |f| ui(f, &mut a));
        let at = a
            .rows()
            .iter()
            .position(|r| r.path == w.join("ur-outer"))
            .unwrap();
        a.select(at);
        a.open();
        settle(&mut a);
        assert_eq!(a.cur, w.join("ur-outer"));
        let inner = tui_term::render_text(120, 20, |f| ui(f, &mut a));
        mode(&locked, 0o755);
        mode(&shut, 0o755);
        let count = |text: &str, part: &str| text.lines().filter(|l| l.contains(part)).count();
        if !blocked {
            assert_eq!(
                count(&top, "[unreadable]") + count(&top, "[incomplete]"),
                0,
                "{top}"
            );
            return;
        }
        assert_eq!(count(&top, "ur-shut/  [unreadable]"), 1, "{top}");
        assert_eq!(count(&top, "ur-outer/  [incomplete]"), 1, "{top}");
        assert_eq!(count(&top, "[unreadable]"), 1, "{top}");
        assert_eq!(count(&top, "[incomplete]"), 1, "{top}");
        assert_eq!(count(&inner, "ur-locked/  [unreadable]"), 1, "{inner}");
        assert_eq!(count(&inner, "[incomplete]"), 0, "{inner}");
    }

    #[test]
    fn screen_shows_path_total_rows_and_keys() {
        let t = tree("ui");
        let mut a = app(&t);
        let text = tui_term::render_text(100, 14, |f| ui(f, &mut a));
        // A long temp dir (macOS) gets its start cut, so the title only
        // promises the path's end, then the total and the counts.
        let tail = format!("{}w · ", std::path::MAIN_SEPARATOR);
        let title = text
            .lines()
            .find(|l| l.contains(&tail))
            .unwrap_or_else(|| panic!("{text}"));
        assert!(title.contains(" entries · 0 marked"), "{title}");
        assert!(title.contains(" items · "), "{title}");
        assert!(text.contains("big/"), "{text}");
        assert!(text.contains("d trash  u undo  r rescan"), "{text}");
    }

    /// Take in finished background scans until none is left.
    fn settle(a: &mut App) {
        let until = std::time::Instant::now() + Duration::from_secs(20);
        while !a.jobs.is_empty() || a.deleting.is_some() {
            assert!(std::time::Instant::now() < until, "scan never finished");
            a.poll_jobs();
            a.poll_delete();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn quick_view_lists_directories_pending_and_files_sized() {
        let t = tree("quick");
        let v = quick_view(&t.0.join("w")).unwrap();
        assert!(v.scanning);
        let names: Vec<_> = v
            .rows
            .iter()
            .map(|r| r.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["big", "small", "file"]);
        assert_eq!(v.rows[0].disk, 0);
        assert_eq!(v.rows[2].apparent, 9000);
        assert_eq!(v.apparent, 9000);
    }

    #[test]
    fn background_scan_opens_at_once_and_fills_in() {
        let t = tree("bg");
        let mut a = App::start("rtrash", vec![t.0.join("w")], ScanOpts::default(), false).unwrap();
        a.trash_pin = Some(t.0.join("Trash"));
        // The quick listing is up before the scan ends.
        assert_eq!(a.rows().len(), 3);
        // Browsing works meanwhile: open big, which scans on its own.
        a.select(0);
        a.open();
        assert_eq!(a.cur, t.0.join("w/big"));
        settle(&mut a);
        assert!(!a.view().scanning);
        assert_eq!(names(&a), ["inner", "side"]);
        a.back();
        assert!(!a.view().scanning);
        assert_eq!(names(&a), ["big", "file", "small"]);
        assert_eq!(a.selected().unwrap().path, t.0.join("w/big"));
        assert!(a.rows()[0].disk >= 200_000, "{:?}", a.rows()[0]);
        // A rescan runs in the background too and keeps the totals.
        let before = a.view().disk;
        a.refresh();
        settle(&mut a);
        assert_eq!(a.view().disk, before);
    }

    #[test]
    fn fit_left_keeps_the_end() {
        assert_eq!(fit_left("/a/b/c", 10), "/a/b/c");
        assert_eq!(fit_left("/very/long/path/leaf", 8), "…th/leaf");
        assert_eq!(fit_left("/x", 0), "");
    }

    #[test]
    fn long_path_leaves_room_for_sizes_and_markers() {
        let t = tree("longtitle");
        let mut a = app(&t);
        // Deeper than the screen is wide.
        let deep = t.0.join("w").join("d".repeat(60)).join("e".repeat(60));
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("f"), b"x").unwrap();
        a.go(deep.clone(), None);
        a.cache.get_mut(&deep).unwrap().partial = true;
        // 70 columns: the item count widened the tail, and the path still
        // gets the room that is left, cut from the left.
        let text = tui_term::render_text(70, 14, |f| ui(f, &mut a));
        let tail = "(partial) · 1 item · 1 entries · 0 marked";
        assert!(text.contains(tail), "{text}");
        assert!(text.contains("…eeee"), "{text}");
        let title = text.lines().find(|l| l.contains(tail)).unwrap();
        assert!(title.chars().count() <= 70, "{title}");
    }

    /// At 70 columns, a free space placed after the size pushed `0 marked`
    /// off the title (round 281). It ranks lowest now: on the same screen
    /// every other part stays and the free space goes.
    #[test]
    fn a_narrow_title_drops_the_free_space_first() {
        let t = tree("narrowfree");
        let mut a = app(&t);
        a.free_of = quarter_free;
        let deep = t.0.join("w").join("d".repeat(60)).join("e".repeat(60));
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("f"), b"x").unwrap();
        a.go(deep.clone(), None);
        a.cache.get_mut(&deep).unwrap().partial = true;
        let text = tui_term::render_text(70, 14, |f| ui(f, &mut a));
        let tail = "(partial) · 1 item · 1 entries · 0 marked";
        assert!(text.contains(tail), "{text}");
        assert!(text.contains("…eeee"), "{text}");
        assert!(!text.contains(" free ("), "{text}");
        let _ = fs::remove_dir_all(&t.0);
    }

    /// With room for the whole path, the free space follows the markers.
    #[test]
    fn a_wide_title_ends_with_the_free_space() {
        let t = tree("widefree");
        let mut a = app(&t);
        a.free_of = quarter_free;
        let text = tui_term::render_text(200, 14, |f| ui(f, &mut a));
        let free = format!("0 marked · {} free (25%) ", format_bytes(1 << 30));
        assert!(text.contains(&free), "{text}");
        let _ = fs::remove_dir_all(&t.0);
    }

    #[test]
    fn the_free_space_shows_only_beside_the_whole_path() {
        let tail = " · 9 B · 1 item · 1 entries · 0 marked ";
        let free = "1 B free (1%)";
        // " /p" + tail + "· 1 B free (1%) " + two corners.
        let fits = (3 + cols(tail) + 2 + cols(free) + 1 + 2) as u16;
        let wide = title_text(fits, "/p", tail, Some(free));
        assert_eq!(wide, format!(" /p{tail}· {free} "));
        let narrow = title_text(fits - 1, "/p", tail, Some(free));
        assert_eq!(narrow, format!(" /p{tail}"));
    }

    #[test]
    fn q_stops_a_scan_first_and_quits_next() {
        let t = tree("stopkey");
        let mut a = app(&t);
        // A scan that never reports, so the test owns the timing.
        let (_tx, rx) = mpsc::channel();
        let control = Arc::new(ScanControl::default());
        a.jobs.insert(
            t.0.join("w"),
            Job {
                control: control.clone(),
                rx,
                refresh: false,
            },
        );
        a.on_key(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(control.stopped());
        assert!(!a.quit, "{}", a.status);
        a.on_key(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(a.quit);
    }

    #[test]
    fn stopped_scan_keeps_partial_sizes_and_every_entry() {
        let t = tree("stopped");
        let mut a = app(&t);
        a.background = true;
        let w = t.0.join("w");
        a.cache.insert(w.clone(), quick_view(&w).unwrap());
        let control = Arc::new(ScanControl::default());
        control.stop.store(true, Ordering::Relaxed);
        a.spawn_with(w.clone(), false, control);
        settle(&mut a);
        let v = a.view();
        assert!(v.partial && !v.scanning);
        // The stop skipped big/ and small/; their quick rows stay listed.
        assert_eq!(names(&a), ["file", "big", "small"]);
        assert!(a.status.contains("scan stopped"), "{}", a.status);
        let text = tui_term::render_text(100, 14, |f| ui(f, &mut a));
        assert!(text.contains("(partial)"), "{text}");
        // A rescan replaces the partial view with a whole one.
        a.refresh();
        settle(&mut a);
        assert!(!a.view().partial);
        assert_eq!(names(&a), ["big", "file", "small"]);
    }

    #[test]
    fn scanning_view_shows_pending_rows_and_refuses_deletes() {
        let t = tree("pending");
        let mut a = app(&t);
        let quick = quick_view(&t.0.join("w")).unwrap();
        a.cache.insert(t.0.join("w"), quick);
        a.select(0);
        a.ask_delete();
        assert_eq!(a.mode, Mode::Browse);
        assert!(a.status.contains("still scanning"), "{}", a.status);
        assert!(t.0.join("w/big").exists());
        let text = tui_term::render_text(100, 14, |f| ui(f, &mut a));
        assert!(text.contains("… [          ] big/"), "{text}");
    }

    #[test]
    fn read_only_keeps_files_and_hides_delete_keys() {
        let t = tree("ro");
        let mut a = app(&t);
        a.read_only = true;
        a.select(0);
        a.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(a.mode, Mode::Browse);
        assert!(a.status.contains("read-only"), "{}", a.status);
        a.on_key(KeyCode::Delete, KeyModifiers::NONE);
        a.on_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(t.0.join("w/big").exists());
        assert!(!t.0.join("Trash").exists());
        a.on_key(KeyCode::Char('u'), KeyModifiers::NONE);
        assert!(a.status.contains("nothing to undo"), "{}", a.status);
        let text = tui_term::render_text(100, 14, |f| ui(f, &mut a));
        assert!(text.contains("rtrash du (read-only)"), "{text}");
        assert!(!text.contains("d trash"), "{text}");
        assert!(text.contains("r rescan"), "{text}");
    }

    #[test]
    fn sort_keys_reorder_rows_and_keep_cursor_and_marks() {
        let t = tree("sortkeys");
        let mut a = app(&t);
        assert_eq!(names(&a), ["big", "file", "small"]);
        a.select(1);
        a.sel.toggle(2);
        // `s` again on the current key reverses it.
        a.on_key(KeyCode::Char('s'), KeyModifiers::NONE);
        assert_eq!(names(&a), ["small", "file", "big"]);
        assert_eq!(a.status, "sorted by disk use, reversed");
        assert_eq!(a.selected().unwrap().path, t.0.join("w/file"));
        assert!(a.sel.is_marked(0) && a.sel.len() == 1);
        a.on_key(KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(names(&a), ["big", "file", "small"]);
        a.on_key(KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(names(&a), ["small", "file", "big"]);
        a.on_key(KeyCode::Char('C'), KeyModifiers::SHIFT);
        assert_eq!(a.status, "sorted by items");
        assert_eq!(names(&a)[0], "big");
        // Views opened later come in the new order.
        a.on_key(KeyCode::Char('n'), KeyModifiers::NONE);
        a.on_key(KeyCode::Char('n'), KeyModifiers::NONE);
        a.select(usize::MAX);
        a.open();
        assert_eq!(names(&a), ["side", "inner"]);
        a.on_key(KeyCode::Char('M'), KeyModifiers::SHIFT);
        let text = tui_term::render_text(100, 14, |f| ui(f, &mut a));
        assert!(text.contains("rtrash du · by newest change"), "{text}");
    }

    #[test]
    fn name_and_newest_change_orders() {
        let row = |name: &str, mtime: i64| Item {
            path: PathBuf::from(name),
            is_dir: false,
            apparent: 1,
            disk: 1,
            mtime,
            items: 1,
            artefact: None,
            repos: Vec::new(),
            git: None,
            unique: None,
            filtered_out: 0,
        };
        let mut rows = vec![row("b", 5), row("C", 9), row("a", 1)];
        let order = |rows: &[Item]| -> Vec<String> {
            rows.iter().map(|r| r.path.display().to_string()).collect()
        };
        sort_rows(&mut rows, Sorting::of(SortKey::Mtime));
        assert_eq!(order(&rows), ["C", "b", "a"]);
        let by_name = Sorting {
            order: Order::Name,
            reverse: false,
        };
        sort_rows(&mut rows, by_name);
        assert_eq!(order(&rows), ["a", "b", "C"]);
        let now = crate::util::now_epoch();
        let s = row_text(&row("x", now - 3 * 86_400), false, 1, SortKey::Mtime);
        assert!(s.ends_with("[3d]"), "{s}");
    }

    #[test]
    fn launched_by_items_still_lists_files() {
        let t = tree("byitems");
        let opts = ScanOpts {
            sort: SortKey::Items,
            ..ScanOpts::default()
        };
        let a = App::new("rtrash", t.0.join("w"), opts, false).unwrap();
        assert_eq!(names(&a), ["big", "file", "small"]);
        assert_eq!(a.sorting.label(), "items");
    }

    #[test]
    fn info_lists_sizes_share_items_age_and_git() {
        let now = crate::util::now_epoch();
        let it = Item {
            path: PathBuf::from("/no/such/target"),
            is_dir: true,
            apparent: 300,
            disk: 512,
            mtime: now - 3 * 86_400,
            items: 3,
            artefact: Some("cargo-target"),
            repos: Vec::new(),
            git: Some((crate::du_git::GitState::Dirty, PathBuf::from("/no/such"))),
            unique: Some(100),
            filtered_out: 0,
        };
        let text = info_lines(&it, 1024, false, now).join("\n");
        assert!(text.contains("kind      directory, cargo-target"), "{text}");
        assert!(
            text.contains("(512 bytes) · 50.0% of this directory"),
            "{text}"
        );
        assert!(text.contains("apparent  "), "{text}");
        assert!(text.contains("(300 bytes)"), "{text}");
        assert!(text.contains("unique    "), "{text}");
        assert!(text.contains("3 files and directories below"), "{text}");
        assert!(text.contains("changed   3d ago, "), "{text}");
        assert!(text.contains("(newest in the tree)"), "{text}");
        assert!(text.contains("git       dirty in "), "{text}");
        // The path is gone, so there is no mode line.
        assert!(!text.contains("mode "), "{text}");
        let pending = info_lines(&it, 1024, true, now).join("\n");
        assert!(pending.contains("disk use  … (scan running)"), "{pending}");
        assert!(!pending.contains("items "), "{pending}");
    }

    #[test]
    fn i_opens_the_info_panel_and_any_key_closes_it() {
        let t = tree("info");
        let mut a = app(&t);
        a.select(1);
        assert_eq!(a.selected().unwrap().path, t.0.join("w/file"));
        a.on_key(KeyCode::Char('i'), KeyModifiers::NONE);
        assert_eq!(a.mode, Mode::Info);
        let text = tui_term::render_text(120, 20, |f| ui(f, &mut a));
        assert!(text.contains("info · any key closes"), "{text}");
        assert!(text.contains("(9000 bytes)"), "{text}");
        assert!(text.contains("kind      file"), "{text}");
        #[cfg(unix)]
        assert!(text.contains("mode      -rw"), "{text}");
        // A key closes the panel and does nothing else.
        a.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(a.mode, Mode::Browse);
        assert!(t.0.join("w/file").exists());
        let text = tui_term::render_text(120, 20, |f| ui(f, &mut a));
        assert!(!text.contains("info · any key closes"), "{text}");
    }

    #[test]
    fn hard_wrap_breaks_by_character() {
        let l = vec!["abcdefg".to_string(), String::new(), "xy".to_string()];
        assert_eq!(hard_wrap(&l, 3), ["abc", "def", "g", "", "xy"]);
    }

    #[test]
    fn wide_characters_take_two_columns_when_cut_and_wrapped() {
        // Four CJK characters take eight columns: cut to five, the end
        // that fits beside the mark stays.
        assert_eq!(fit_left("漢字漢字", 5), "…漢字");
        assert_eq!(fit_left("ab漢字", 4), "…字");
        assert_eq!(fit_left("漢字", 4), "漢字");
        assert_eq!(fit_left("/p/📦box", 6), "…📦box");
        // A combining mark never sits alone after the mark.
        assert_eq!(fit_left("ab\u{301}", 1), "…");
        let l = vec!["漢字漢".to_string(), "a漢b".to_string()];
        assert_eq!(hard_wrap(&l, 4), ["漢字", "漢", "a漢b"]);
        assert_eq!(hard_wrap(&["漢".to_string()], 1), ["漢"]);
    }

    #[cfg(unix)]
    #[test]
    fn mode_letters_match_ls() {
        assert_eq!(mode_string(0o040755), "drwxr-xr-x");
        assert_eq!(mode_string(0o100644), "-rw-r--r--");
        assert_eq!(mode_string(0o104755), "-rwsr-xr-x");
        assert_eq!(mode_string(0o041777), "drwxrwxrwt");
        assert_eq!(mode_string(0o102644), "-rw-r-Sr--");
        assert_eq!(mode_string(0o120777), "lrwxrwxrwx");
    }

    fn typ(a: &mut App, text: &str) {
        for c in text.chars() {
            let mods = if c.is_uppercase() {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            };
            a.on_key(KeyCode::Char(c), mods);
        }
    }

    #[test]
    fn filter_shows_matching_rows_and_clears_on_leave() {
        let t = tree("filter");
        let mut a = app(&t);
        a.keys = Keymap::builtin();
        a.on_key(KeyCode::Char('/'), KeyModifiers::NONE);
        assert_eq!(a.mode, Mode::Filter);
        // Case is ignored.
        typ(&mut a, "Fi");
        assert_eq!(names(&a), ["file"]);
        assert!(a.status.contains("1 shown"), "{}", a.status);
        a.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(a.mode, Mode::Browse);
        let text = tui_term::render_text(100, 14, |f| ui(f, &mut a));
        assert!(text.contains("matching \"Fi\""), "{text}");
        assert!(text.contains("1 of 3 entries"), "{text}");
        // Marking all marks only what the filter shows.
        a.on_key(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(a.targets().len(), 1);
        // q clears the filter before it quits.
        a.on_key(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(!a.quit);
        assert_eq!(names(&a), ["big", "file", "small"]);
        a.on_key(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(a.quit);
    }

    #[test]
    fn dot_hides_dot_names_in_every_view_until_pressed_again() {
        let t = tree("dots");
        fs::write(t.0.join("w/.env"), b"x").unwrap();
        fs::create_dir_all(t.0.join("w/big/.cache")).unwrap();
        fs::write(t.0.join("w/big/.cache/c"), vec![3u8; 40_000]).unwrap();
        let mut a = app(&t);
        a.keys = Keymap::builtin();
        // Sizes on disk differ by platform, so compare names in order.
        let sorted = |a: &App| {
            let mut n = names(a);
            n.sort();
            n
        };
        assert_eq!(sorted(&a), [".env", "big", "file", "small"]);
        a.on_key(KeyCode::Char('.'), KeyModifiers::NONE);
        assert_eq!(sorted(&a), ["big", "file", "small"]);
        assert!(a.status.contains("(1 here)"), "{}", a.status);
        let text = tui_term::render_text(100, 14, |f| ui(f, &mut a));
        assert!(text.contains("dot names hidden"), "{text}");
        assert!(text.contains("3 of 4 entries"), "{text}");
        // The setting follows into other views, and the filter works
        // on what is left.
        let big = names(&a).iter().position(|n| n == "big").unwrap();
        a.select(big);
        a.open();
        assert_eq!(sorted(&a), ["inner", "side"]);
        a.on_key(KeyCode::Char('/'), KeyModifiers::NONE);
        typ(&mut a, "c");
        assert!(a.rows().is_empty());
        a.on_key(KeyCode::Esc, KeyModifiers::NONE);
        a.on_key(KeyCode::Char('.'), KeyModifiers::NONE);
        assert_eq!(sorted(&a), [".cache", "inner", "side"]);
        a.back();
        assert_eq!(sorted(&a), [".env", "big", "file", "small"]);
        // q quits at once; the dot setting is not a filter to clear.
        a.on_key(KeyCode::Char('.'), KeyModifiers::NONE);
        a.on_key(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(a.quit);
    }

    #[test]
    fn hidden_rows_follow_size_changes_below_them() {
        let t = tree("dotchain");
        fs::create_dir_all(t.0.join("w/.cache")).unwrap();
        fs::write(t.0.join("w/.cache/c"), vec![3u8; 40_000]).unwrap();
        let mut a = app(&t);
        a.on_key(KeyCode::Char('.'), KeyModifiers::NONE);
        let dot = a.cur.join(".cache");
        let before = a.view().stash.iter().find(|r| r.path == dot).unwrap().disk;
        let roots = a.roots.clone();
        let gone = Totals {
            disk: 1000,
            apparent: 1000,
            items: 0,
        };
        carry(&mut a.cache, &roots, &dot, gone, Totals::default());
        let after = a.view().stash.iter().find(|r| r.path == dot).unwrap().disk;
        assert_eq!(after, before - 1000);
    }

    #[cfg(unix)]
    #[test]
    fn a_rescan_moves_each_total_up_on_its_own() {
        // Below big, the disk use shrinks while the apparent size grows: a
        // 30000-byte file goes, and a sparse 1 MiB file with no blocks
        // comes. Each total of the views above must follow its own change.
        let t = tree("carryeach");
        let mut a = app(&t);
        let w = t.0.join("w");
        let big = w.join("big");
        let row = |a: &App| -> Totals {
            let v = &a.cache[&w];
            Totals::of_row(v.rows.iter().find(|r| r.path == big).unwrap())
        };
        let (was_w, was_big) = (Totals::of_view(&a.cache[&w]), row(&a));
        a.select(0);
        a.open();
        assert_eq!(a.cur, big);
        let was = Totals::of_view(a.view());
        fs::remove_file(big.join("side")).unwrap();
        fs::File::create(big.join("sparse"))
            .unwrap()
            .set_len(1 << 20)
            .unwrap();
        a.on_key(KeyCode::Char('r'), KeyModifiers::NONE);
        let fresh = Totals::of_view(&a.cache[&big]);
        assert!(fresh.disk < was.disk, "{fresh:?} {was:?}");
        assert!(fresh.apparent > was.apparent, "{fresh:?} {was:?}");
        let now_big = row(&a);
        assert_eq!(now_big.disk, fresh.disk);
        assert_eq!(now_big.apparent, fresh.apparent);
        let now_w = Totals::of_view(&a.cache[&w]);
        assert_eq!(now_w.disk, was_w.disk - was.disk + fresh.disk);
        assert_eq!(
            now_w.apparent,
            was_w.apparent - was.apparent + fresh.apparent
        );
        // side went and sparse came: big still holds three entries.
        assert_eq!(now_big.items, was_big.items);
        let _ = fs::remove_dir_all(&t.0);
    }

    #[test]
    fn filter_takes_globs_and_letters_bound_to_moves() {
        let t = tree("filterglob");
        let mut a = app(&t);
        a.keys = Keymap::builtin();
        a.on_key(KeyCode::Char('/'), KeyModifiers::NONE);
        typ(&mut a, "*a*");
        assert_eq!(names(&a), ["small"]);
        a.on_key(KeyCode::Backspace, KeyModifiers::NONE);
        a.on_key(KeyCode::Backspace, KeyModifiers::NONE);
        a.on_key(KeyCode::Backspace, KeyModifiers::NONE);
        typ(&mut a, "jk");
        assert_eq!(a.filter, "jk");
        assert!(a.rows().is_empty());
        a.on_key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(a.mode, Mode::Browse);
        assert!(a.filter.is_empty());
        assert_eq!(names(&a), ["big", "file", "small"]);
    }

    #[test]
    fn filter_stays_through_rescans_and_ends_on_open() {
        let t = tree("filterscan");
        let mut a = app(&t);
        a.keys = Keymap::builtin();
        a.on_key(KeyCode::Char('/'), KeyModifiers::NONE);
        typ(&mut a, "i");
        a.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(names(&a), ["big", "file"]);
        fs::write(t.0.join("w/fig"), vec![2u8; 50_000]).unwrap();
        fs::write(t.0.join("w/other"), vec![2u8; 50_000]).unwrap();
        a.refresh();
        assert_eq!(names(&a), ["big", "fig", "file"]);
        // Hidden rows wait in the stash with their fresh sizes.
        let hidden: Vec<_> = a.view().stash.iter().map(|r| r.disk).collect();
        assert_eq!(hidden.len(), 2);
        assert!(hidden.iter().any(|&d| d >= 50_000), "{hidden:?}");
        a.select(0);
        a.open();
        assert!(a.filter.is_empty());
        a.back();
        assert_eq!(names(&a), ["big", "fig", "other", "file", "small"]);
    }

    // Deletes go to the real Recycle Bin on Windows.
    #[cfg(unix)]
    #[test]
    fn delete_while_filtered_trashes_only_shown_rows() {
        let t = tree("filterdel");
        let mut a = app(&t);
        a.keys = Keymap::builtin();
        a.on_key(KeyCode::Char('/'), KeyModifiers::NONE);
        typ(&mut a, "sma");
        a.on_key(KeyCode::Enter, KeyModifiers::NONE);
        a.on_key(KeyCode::Char('a'), KeyModifiers::NONE);
        a.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        a.on_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(!t.0.join("w/small").exists());
        assert!(t.0.join("w/big").exists() && t.0.join("w/file").exists());
        // Undo brings it back into the filtered view.
        a.on_key(KeyCode::Char('u'), KeyModifiers::NONE);
        assert_eq!(names(&a), ["small"]);
        a.on_key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(names(&a), ["big", "file", "small"]);
    }

    /// [`ScanOpts`] that leave out every entry named `side`.
    fn without_side() -> ScanOpts {
        let spec = crate::du_filter::Spec {
            patterns: vec!["side".into()],
            ..Default::default()
        };
        ScanOpts {
            filter: Some(Arc::new(spec)),
            ..ScanOpts::default()
        }
    }

    const BIG_REFUSED: &str = "'big' holds 1 entry the filters hide; nothing was trashed \
                               (--allow-filtered trashes them too)";

    // Deletes go to the real Recycle Bin on Windows.
    #[cfg(unix)]
    #[test]
    fn filtered_entries_block_a_trash_unless_allowed() {
        let t = tree("hiddenbelow");
        let mut a = App::new("rtrash", t.0.join("w"), without_side(), false).unwrap();
        a.trash_pin = Some(t.0.join("Trash"));
        assert_eq!(names(&a), ["big", "file", "small"]);
        // big holds `side`, which the filter hides: d explains and asks
        // nothing.
        a.select(0);
        a.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(a.mode, Mode::Browse);
        assert_eq!(a.status, BIG_REFUSED);
        assert!(t.0.join("w/big/side").exists());
        // small hides nothing and goes as before.
        a.select(2);
        a.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        a.on_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(!t.0.join("w/small").exists());
        // --allow-filtered: big asks, then goes with `side`.
        a.allow_filtered = true;
        a.select(0);
        a.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(a.mode, Mode::Confirm);
        a.on_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(!t.0.join("w/big").exists());
    }

    #[test]
    fn a_root_the_filters_hid_entries_in_is_refused_too() {
        let t = tree("hiddenroot");
        let roots = vec![t.0.join("w/big"), t.0.join("w/small")];
        let mut a = App::new_in("rtrash", roots, without_side(), false).unwrap();
        a.trash_pin = Some(t.0.join("Trash"));
        assert_eq!(names(&a), ["big", "small"]);
        a.select(0);
        a.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(a.status, BIG_REFUSED);
        assert_eq!(a.mode, Mode::Browse);
    }

    #[test]
    fn shift_s_shows_apparent_sizes_and_back() {
        let t = tree("sizekey");
        let mut a = app(&t);
        a.on_key(KeyCode::Char('n'), KeyModifiers::NONE);
        a.on_key(KeyCode::Char('S'), KeyModifiers::SHIFT);
        assert_eq!(a.size_key, SortKey::Apparent);
        assert!(a.status.contains("apparent sizes"), "{}", a.status);
        let text = tui_term::render_text(100, 14, |f| ui(f, &mut a));
        assert!(text.contains("by name · apparent sizes"), "{text}");
        let file = text.lines().find(|l| l.contains(" file")).unwrap();
        assert!(file.contains(&format_bytes(9000)), "{file}");
        // The name order stays through the switch.
        assert_eq!(names(&a), ["big", "file", "small"]);
        a.on_key(KeyCode::Char('S'), KeyModifiers::SHIFT);
        assert_eq!(a.size_key, SortKey::Disk);
        let text = tui_term::render_text(100, 14, |f| ui(f, &mut a));
        assert!(!text.contains("· apparent sizes"), "{text}");
    }

    // A sparse file is big by length and takes no blocks; Windows only
    // makes one on request.
    #[cfg(unix)]
    #[test]
    fn shift_s_reorders_a_size_sort() {
        let t = tree("sizesort");
        let f = fs::File::create(t.0.join("w/sparse")).unwrap();
        f.set_len(10_000_000).unwrap();
        drop(f);
        let mut a = app(&t);
        assert_eq!(names(&a)[0], "big");
        a.on_key(KeyCode::Char('S'), KeyModifiers::SHIFT);
        assert_eq!(names(&a)[0], "sparse");
        let text = tui_term::render_text(100, 14, |f| ui(f, &mut a));
        assert!(text.contains("rtrash du · by apparent size─"), "{text}");
        // A reversed size order stays reversed.
        a.on_key(KeyCode::Char('s'), KeyModifiers::NONE);
        assert_eq!(names(&a).last().unwrap(), "sparse");
        a.on_key(KeyCode::Char('S'), KeyModifiers::SHIFT);
        assert_eq!(a.sorting.label(), "disk use, reversed");
        assert_eq!(names(&a).last().unwrap(), "big");
    }

    #[test]
    fn p_quits_with_the_marked_paths_and_y_copies_them() {
        let t = tree("printcopy");
        let mut a = app(&t);
        a.on_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert_eq!(
            a.clipboard.take().unwrap(),
            t.0.join("w").join("big").to_string_lossy()
        );
        assert!(a.status.contains("copied 1 path"), "{}", a.status);
        a.sel.toggle(1);
        a.sel.toggle(2);
        a.on_key(KeyCode::Char('y'), KeyModifiers::NONE);
        let text = a.clipboard.take().unwrap();
        assert_eq!(text.lines().count(), 2, "{text}");
        assert!(!a.quit);
        a.on_key(KeyCode::Char('p'), KeyModifiers::NONE);
        assert!(a.quit);
        assert_eq!(a.print_on_exit, [t.0.join("w/file"), t.0.join("w/small")]);
    }

    #[test]
    fn osc52_wraps_base64() {
        for (raw, enc) in [
            ("", ""),
            ("a", "YQ=="),
            ("ab", "YWI="),
            ("abc", "YWJj"),
            ("abcd", "YWJjZA=="),
            ("abcde", "YWJjZGU="),
            ("abcdef", "YWJjZGVm"),
        ] {
            assert_eq!(base64(raw.as_bytes()), enc);
        }
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn trash_rows_stay_closed_and_are_never_targets() {
        let t = tree("trashrow");
        let mut a = app(&t);
        let v = a.cache.get_mut(&t.0.join("w")).unwrap();
        v.rows[0].artefact = Some("trash");
        a.select(0);
        a.open();
        assert_eq!(a.cur, t.0.join("w"));
        assert!(a.targets().is_empty());
    }

    fn app_on(t: &Tree, dirs: &[&str]) -> App {
        let roots = dirs.iter().map(|d| t.0.join(d)).collect();
        let mut a = App::new_in("rtrash", roots, ScanOpts::default(), false).unwrap();
        a.trash_pin = Some(t.0.join("Trash"));
        a.free_of = no_free;
        a
    }

    #[test]
    fn several_roots_open_on_a_view_of_their_own() {
        let t = tree("roots");
        let (big, small) = (t.0.join("w/big"), t.0.join("w/small"));
        let mut a = app_on(&t, &["w/small", "w/big"]);
        assert!(is_top_of_roots(&a.cur));
        let paths: Vec<&Path> = a.rows().iter().map(|r| r.path.as_path()).collect();
        assert_eq!(paths, [big.as_path(), small.as_path()]);
        let sum: u64 = a.rows().iter().map(|r| r.disk).sum();
        assert_eq!(a.view().disk, sum);
        assert!(a.rows()[0].disk >= 230_000, "{:?}", a.rows()[0]);
        let text = tui_term::render_text(160, 14, |f| ui(f, &mut a));
        assert!(text.contains(" 2 directories · "), "{text}");
        // Rows of roots show their paths, since their names can repeat.
        assert!(text.contains(&format!("{}/", big.display())), "{text}");

        // Each root's view came with the scan, so opening one walks
        // nothing again.
        assert!(a.cache.get(&small).is_some_and(|v| !v.scanning));
        assert_eq!(a.cache[&big].disk, a.rows()[0].disk);
        // A root opens like any directory, and going up from it returns
        // to the roots, on its row.
        a.select(1);
        a.on_key(KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(a.cur, small);
        a.on_key(KeyCode::Left, KeyModifiers::NONE);
        assert!(is_top_of_roots(&a.cur));
        assert_eq!(a.selected().unwrap().path, small);
        a.on_key(KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(a.status, "already at the top");
        // No shell for the view above the roots, which is no directory.
        a.on_key(KeyCode::Char('b'), KeyModifiers::NONE);
        assert!(a.hand_off.is_none());
        assert!(
            a.status.contains("open one of the directories"),
            "{}",
            a.status
        );

        // A rescan of the roots view counts them again.
        a.on_key(KeyCode::Char('r'), KeyModifiers::NONE);
        assert_eq!(a.view().disk, sum);
        assert_eq!(a.rows().len(), 2);
    }

    #[test]
    fn several_roots_load_in_the_background_like_one() {
        let t = tree("roots-bg");
        let roots = vec![t.0.join("w/big"), t.0.join("w/small")];
        let mut a = App::start("rtrash", roots, ScanOpts::default(), false).unwrap();
        assert!(a.view().scanning);
        assert_eq!(a.rows().len(), 2);
        // Open a root before the roots view has its sizes.
        a.select(0);
        a.on_key(KeyCode::Right, KeyModifiers::NONE);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !a.jobs.is_empty() && std::time::Instant::now() < deadline {
            a.poll_jobs();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(a.jobs.is_empty(), "scans still running");
        // The roots view kept its result while off screen.
        let top = &a.cache[&PathBuf::new()];
        assert!(!top.scanning);
        assert!(top.disk >= 230_000, "{}", top.disk);
    }

    #[test]
    fn explorer_roots_drop_repeats_and_refuse_overlaps_and_files() {
        let t = tree("roots-check");
        let w = t.0.join("w");
        let got = explorer_roots("rtrash", &[w.join("big"), w.join("small"), w.join("big")]);
        assert_eq!(got.unwrap().len(), 2);
        assert_eq!(
            explorer_roots("rtrash", &[w.clone(), w.join("big")]),
            Err(2)
        );
        assert_eq!(
            explorer_roots("rtrash", &[w.join("big/inner"), w.join("big")]),
            Err(2)
        );
        assert_eq!(
            explorer_roots("rtrash", &[w.join("big"), w.join("file")]),
            Err(2)
        );
        assert_eq!(explorer_roots("rtrash", &[w.join("missing")]), Err(1));
    }

    // Deletes go to the real Recycle Bin on Windows.
    #[cfg(unix)]
    #[test]
    fn trashing_below_several_roots_carries_up_to_their_view() {
        let t = tree("roots-del");
        let mut a = app_on(&t, &["w/big", "w/small"]);
        a.keys = Keymap::builtin();
        let top_before = a.view().disk;
        // In a root: the file goes, and the roots view follows.
        a.select(0);
        a.on_key(KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(a.cur, t.0.join("w/big"));
        let side = a
            .rows()
            .iter()
            .position(|r| r.path.ends_with("side"))
            .unwrap();
        let side_disk = a.rows()[side].disk;
        a.select(side);
        a.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        a.on_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(!t.0.join("w/big/side").exists());
        let top = &a.cache[&PathBuf::new()];
        assert_eq!(top.disk, top_before - side_disk);
        assert_eq!(top.rows.iter().map(|r| r.disk).sum::<u64>(), top.disk);
        // A root itself can go from the roots view, and come back.
        a.on_key(KeyCode::Left, KeyModifiers::NONE);
        let small = a
            .rows()
            .iter()
            .position(|r| r.path.ends_with("small"))
            .unwrap();
        a.select(small);
        a.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        a.on_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(!t.0.join("w/small").exists());
        assert_eq!(a.rows().len(), 1);
        a.on_key(KeyCode::Char('u'), KeyModifiers::NONE);
        assert!(t.0.join("w/small").exists());
        assert_eq!(a.rows().len(), 2);
        assert_eq!(a.view().disk, top_before - side_disk);
    }

    #[test]
    fn a_row_that_stays_says_why_and_e_lists_it() {
        let t = tree("whyfail");
        let mut a = app(&t);
        let big = a.rows()[0].clone();
        let (tx, _watch) = held_delete(&mut a, &big);
        let why = "cannot trash 'big': Permission denied".to_string();
        tx.send((big.clone(), Err(why))).unwrap();
        a.poll_delete();
        assert!(
            a.status.ends_with("; 1 failed (e shows why) · q stops"),
            "{}",
            a.status
        );
        a.on_key(KeyCode::Char('e'), KeyModifiers::NONE);
        assert_eq!(a.mode, Mode::Info);
        assert!(a.info[0].ends_with("big: cannot trash 'big': Permission denied"));
        a.on_key(KeyCode::Esc, KeyModifiers::NONE);
        drop(tx);
        a.poll_delete();
        assert!(a.deleting.is_none());
        assert_eq!(
            a.status,
            "trashed 0, 1 failed (big: cannot trash 'big': Permission denied) · e why · u undo"
        );
        assert_eq!(names(&a), ["big", "file", "small"]);
        // The list stays for the last delete.
        a.on_key(KeyCode::Char('e'), KeyModifiers::NONE);
        assert_eq!(a.info.len(), 1);
    }

    #[test]
    fn marks_and_the_keys_on_a_queued_row_wait_for_the_delete() {
        let t = tree("waitdel");
        let mut a = app(&t);
        // Rows: big, file, small. The delete holds file.
        let file = a.rows()[1].clone();
        let (_tx, _watch) = held_delete(&mut a, &file);
        a.select(1);
        a.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(a.status, "a delete is running; mark once it is done");
        assert_eq!(a.marked_count(), 0);
        a.on_key(KeyCode::Char('v'), KeyModifiers::NONE);
        assert!(a.status.ends_with("' is being trashed"), "{}", a.status);
        assert_eq!(a.hand_off, None);
        a.opener = vec!["/nonexistent/rtrash-opener".into()];
        a.on_key(KeyCode::Char('o'), KeyModifiers::NONE);
        assert!(a.status.ends_with("' is being trashed"), "{}", a.status);
        // No row has failed yet, so e has nothing to list.
        a.on_key(KeyCode::Char('e'), KeyModifiers::NONE);
        assert_eq!(a.status, "no row failed in the last delete");
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_trash_shows_what_put_said() {
        let t = tree("putsaid");
        let mut a = app(&t);
        // A file where the trash should be: put cannot make files/ there.
        fs::write(t.0.join("Trash"), b"not a folder").unwrap();
        a.select(1);
        a.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        a.on_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(
            a.status
                .starts_with("trashed 0, 1 failed (file: cannot trash '"),
            "{}",
            a.status
        );
        assert!(t.0.join("w/file").is_file());
        assert_eq!(a.failures.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn why_skips_warnings_and_the_program_name() {
        let said = [
            "rtrash: warning: 'x' is in use by vim".to_string(),
            "rtrash: cannot trash 'x': in use by vim".to_string(),
            "rtrash: use --in-use=warn to trash it anyway".to_string(),
        ];
        assert_eq!(
            crate::put::why("rtrash", &said),
            "cannot trash 'x': in use by vim"
        );
        assert_eq!(crate::put::why("rtrash", &[]), "not trashed");
    }

    /// gdu #688: an online-only OneDrive file has its full length and no
    /// clusters. A sparse file with nothing written has the same shape.
    /// Its row shows the allocated size in the quick view as in the scan,
    /// so the size does not jump once the scan is in. Windows only.
    #[cfg(windows)]
    #[test]
    fn quick_view_rows_show_the_allocated_size_on_windows() {
        use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
        let t = tree("hollow");
        let w = t.0.join("w");
        let f = w.join("hollow.bin");
        let file = fs::File::create(&f).unwrap();
        crate::du_dupes::fsctl(&file, FSCTL_SET_SPARSE, &[], &mut []).unwrap();
        file.set_len(8 << 20).unwrap();
        drop(file);
        let allocated = crate::du_fs::lstat(&f).unwrap().sizes().1;
        assert!(allocated < 1 << 20, "a sparse file took {allocated} bytes");
        let quick = quick_view(&w).unwrap();
        let q = quick.rows.iter().find(|r| r.path == f).cloned();
        let q = q.expect("hollow.bin in the quick view");
        assert_eq!(q.apparent, 8 << 20, "{q:?}");
        assert_eq!(q.disk, allocated, "{q:?}");
        let a = app(&t);
        let it = a.rows().iter().find(|r| r.path == f).cloned();
        let it = it.expect("hollow.bin in the scan");
        assert_eq!(it.disk, q.disk, "{it:?}");
        let _ = fs::remove_dir_all(&t.0);
    }

    /// ncdu #237: a delete deep in the tree leaves the views above it in
    /// the order the user saw. Back up, the row walked out of is still in
    /// its place with the cursor on it, though it is now smaller than the
    /// row below. A sort key puts the view in order again. With `.`
    /// hiding dot names, going back refilters the view, which used to
    /// sort it. Unix only: the delete uses the pinned trash.
    #[cfg(unix)]
    #[test]
    fn rows_above_a_delete_keep_their_order_and_the_cursor() {
        let t = tree("keep");
        fs::write(t.0.join("w/mid"), vec![3u8; 100_000]).unwrap();
        let mut a = app(&t);
        assert_eq!(names(&a), ["big", "mid", "file", "small"]);
        a.on_key(KeyCode::Char('.'), KeyModifiers::NONE);
        a.select(0);
        a.open(); // big
        a.select(0); // inner, 200 kB of big's 230 kB
        a.ask_delete();
        a.on_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(!t.0.join("w/big/inner").exists());
        a.back();
        assert_eq!(names(&a), ["big", "mid", "file", "small"]);
        assert_eq!(a.list_state.selected(), Some(0));
        let big = &a.rows()[0];
        assert!(big.disk < a.rows()[1].disk, "{big:?}");
        // s on the size order already in use reverses it; n then s
        // sorts by size afresh.
        a.on_key(KeyCode::Char('n'), KeyModifiers::NONE);
        a.on_key(KeyCode::Char('s'), KeyModifiers::NONE);
        assert_eq!(names(&a)[0], "mid");
        let _ = fs::remove_dir_all(&t.0);
    }

    /// ncdu #237 in the view itself: trashing the row under the cursor
    /// takes it out, the rows below move up one place in the same order,
    /// and the cursor stays at its index, now on the next row. Unix only:
    /// the delete uses the pinned trash.
    #[cfg(unix)]
    #[test]
    fn a_deleted_row_leaves_the_others_in_order_and_the_cursor_in_place() {
        let t = tree("inplace");
        fs::write(t.0.join("w/mid"), vec![3u8; 100_000]).unwrap();
        let mut a = app(&t);
        assert_eq!(names(&a), ["big", "mid", "file", "small"]);
        a.select(1);
        a.ask_delete();
        a.on_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(!t.0.join("w/mid").exists());
        assert_eq!(names(&a), ["big", "file", "small"]);
        assert_eq!(a.list_state.selected(), Some(1));
        let _ = fs::remove_dir_all(&t.0);
    }

    /// ncdu #77: the title names the room left on the view's file
    /// system. It is read once per view, kept while the view stays, and
    /// read again after a delete. Unix only, as above.
    #[cfg(unix)]
    #[test]
    fn the_title_shows_the_free_space_of_the_view() {
        let t = tree("free");
        let mut a = app(&t);
        a.free_of = quarter_free;
        let line = a.free_here().unwrap();
        assert_eq!(line, format!("{} free (25%)", format_bytes(1 << 30)));
        let kept = a.free.as_ref().map(|(p, _)| p.clone());
        assert_eq!(kept, Some(t.0.join("w")));
        a.select(1); // file
        a.ask_delete();
        a.on_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(a.free.is_none());
        assert!(a.free_here().is_some());
        let _ = fs::remove_dir_all(&t.0);
    }

    #[test]
    fn free_text_gives_the_bytes_left_and_their_share() {
        let s = crate::space::Space {
            total: 1000,
            avail: 250,
            ..Default::default()
        };
        assert_eq!(free_text(&s), format!("{} free (25%)", format_bytes(250)));
    }

    #[test]
    fn a_row_back_from_the_stash_slots_in_without_moving_the_rest() {
        let row = |name: &str, disk: u64| Item {
            path: PathBuf::from("/v").join(name),
            is_dir: false,
            apparent: disk,
            disk,
            mtime: 0,
            items: 1,
            artefact: None,
            repos: Vec::new(),
            git: None,
            unique: None,
            filtered_out: 0,
        };
        let s = Sorting::of(SortKey::Disk);
        // Out of order on purpose, as a delete below "a" leaves it.
        let mut rows = vec![row("a", 10), row("b", 50), row("c", 5)];
        insert_in_order(&mut rows, row("d", 20), s);
        let names: Vec<String> = rows
            .iter()
            .map(|r| r.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["d", "a", "b", "c"]);
    }

    /// Press `E`, type `name` and press `key` (Enter writes, Esc cancels).
    fn export_as(a: &mut App, name: &Path, key: KeyCode) {
        a.on_key(KeyCode::Char('E'), KeyModifiers::SHIFT);
        assert_eq!(a.mode, Mode::Export, "{}", a.status);
        for c in name.to_string_lossy().chars() {
            a.on_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        a.on_key(key, KeyModifiers::NONE);
        assert_eq!(a.mode, Mode::Browse, "{}", a.status);
    }

    #[test]
    fn e_writes_the_view_as_the_snapshot_du_o_writes() {
        let t = tree("export");
        let mut a = app(&t);
        let out = t.0.join("w.snap");
        export_as(&mut a, &out, KeyCode::Enter);
        assert!(a.status.starts_with("wrote '"), "{}", a.status);
        let mut got = crate::du_snap::read(&out).unwrap();
        // The same directory as du -o FILE --max-depth=1 scans it.
        let opts = ScanOpts {
            max_depth: Some(1),
            ..crate::du_snap::snapshot_opts(ScanOpts::default())
        };
        let report = du::scan(&[t.0.join("w")], &opts).unwrap();
        let mut text = Vec::new();
        crate::du_snap::write(&report, &opts, got.created, &mut text).unwrap();
        let mut want = crate::du_snap::parse(&text[..]).unwrap();
        for snap in [&mut got, &mut want] {
            snap.dirs.sort_by(|x, y| x.path.cmp(&y.path));
        }
        assert_eq!(got.max_depth, Some(1));
        assert_eq!(got.unreadable, Some(0));
        assert_eq!(got.roots, want.roots);
        assert_eq!(got.dirs, want.dirs);
        let big = t.0.join("w").join("big");
        assert!(got.dirs.iter().any(|d| d.path == big), "{:?}", got.dirs);
        let _ = fs::remove_dir_all(&t.0);
    }

    #[test]
    fn e_leaves_an_existing_file_alone_and_esc_writes_nothing() {
        let t = tree("export-refuse");
        let mut a = app(&t);
        let taken = t.0.join("taken");
        fs::write(&taken, b"keep").unwrap();
        export_as(&mut a, &taken, KeyCode::Enter);
        assert!(a.status.contains("exists; nothing written"), "{}", a.status);
        assert_eq!(fs::read(&taken).unwrap(), b"keep");
        let none = t.0.join("none.snap");
        export_as(&mut a, &none, KeyCode::Esc);
        assert_eq!(a.status, "nothing written");
        assert!(!none.exists());
        export_as(&mut a, Path::new(""), KeyCode::Enter);
        assert_eq!(a.status, "no file name; nothing written");
        let _ = fs::remove_dir_all(&t.0);
    }

    #[test]
    fn e_above_several_roots_writes_the_snapshot_du_o_writes_of_them() {
        let t = tree("export-roots");
        let mut a = app_on(&t, &["w/small", "w/big"]);
        assert!(is_top_of_roots(&a.cur));
        a.on_key(KeyCode::Char('E'), KeyModifiers::SHIFT);
        assert!(a.status.contains("of these 2 directories"), "{}", a.status);
        a.on_key(KeyCode::Esc, KeyModifiers::NONE);
        let out = t.0.join("roots.snap");
        export_as(&mut a, &out, KeyCode::Enter);
        assert!(a.status.starts_with("wrote '"), "{}", a.status);
        let mut got = crate::du_snap::read(&out).unwrap();
        // What du -o FILE --max-depth=1 w/small w/big writes.
        let opts = ScanOpts {
            max_depth: Some(1),
            ..crate::du_snap::snapshot_opts(ScanOpts::default())
        };
        let dirs = [t.0.join("w/small"), t.0.join("w/big")];
        let report = du::scan(&dirs, &opts).unwrap();
        let mut text = Vec::new();
        crate::du_snap::write(&report, &opts, got.created, &mut text).unwrap();
        let mut want = crate::du_snap::parse(&text[..]).unwrap();
        for snap in [&mut got, &mut want] {
            snap.dirs.sort_by(|x, y| x.path.cmp(&y.path));
        }
        assert_eq!(got.max_depth, Some(1));
        assert_eq!(got.unreadable, Some(0));
        assert_eq!(got.roots, want.roots);
        assert_eq!(got.dirs, want.dirs);
        let roots: Vec<&Path> = got.roots.iter().map(|r| r.path.as_path()).collect();
        assert_eq!(roots, [dirs[0].as_path(), dirs[1].as_path()]);
        let inner = t.0.join("w/big/inner");
        assert!(got.dirs.iter().any(|d| d.path == inner), "{:?}", got.dirs);
        let _ = fs::remove_dir_all(&t.0);
    }

    #[cfg(unix)]
    #[test]
    fn e_above_several_roots_waits_for_a_rescan_after_a_delete_below() {
        let t = tree("export-roots-stale");
        let mut a = app_on(&t, &["w/small", "w/big"]);
        let stale = "sizes changed since the scan; r rescans, then E writes it";
        a.select(0);
        a.open(); // big, the larger root
        assert_eq!(a.cur, t.0.join("w/big"));
        a.select(0); // inner
        a.ask_delete();
        a.on_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(!t.0.join("w/big/inner").exists());
        a.back();
        assert!(is_top_of_roots(&a.cur));
        a.on_key(KeyCode::Char('E'), KeyModifiers::SHIFT);
        assert_eq!(a.status, stale);
        assert_eq!(a.mode, Mode::Browse);
        // A rescan of the top view scans every root again.
        a.on_key(KeyCode::Char('r'), KeyModifiers::NONE);
        let out = t.0.join("roots.snap");
        export_as(&mut a, &out, KeyCode::Enter);
        assert!(a.status.starts_with("wrote '"), "{}", a.status);
        let snap = crate::du_snap::read(&out).unwrap();
        let inner = t.0.join("w/big/inner");
        assert!(
            !snap.dirs.iter().any(|d| d.path == inner),
            "{:?}",
            snap.dirs
        );
        assert_eq!(snap.roots.len(), 2);
        let _ = fs::remove_dir_all(&t.0);
    }

    #[cfg(unix)]
    #[test]
    fn e_waits_for_a_rescan_once_a_delete_changed_the_sizes() {
        let t = tree("export-stale");
        let mut a = app(&t);
        let stale = "sizes changed since the scan; r rescans, then E writes it";
        a.select(0);
        a.open(); // big
        a.select(0); // inner
        a.ask_delete();
        a.on_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(!t.0.join("w/big/inner").exists());
        a.on_key(KeyCode::Char('E'), KeyModifiers::SHIFT);
        assert_eq!(a.status, stale);
        a.back();
        a.on_key(KeyCode::Char('E'), KeyModifiers::SHIFT);
        assert_eq!(a.status, stale);
        assert_eq!(a.mode, Mode::Browse);
        a.select(0);
        a.open(); // big again, from the cache
        assert_eq!(a.cur, t.0.join("w/big"));
        a.on_key(KeyCode::Char('r'), KeyModifiers::NONE);
        let out = t.0.join("big.snap");
        export_as(&mut a, &out, KeyCode::Enter);
        assert!(a.status.starts_with("wrote '"), "{}", a.status);
        let snap = crate::du_snap::read(&out).unwrap();
        assert_eq!(snap.roots[0].path, t.0.join("w/big"));
        let _ = fs::remove_dir_all(&t.0);
    }

    #[cfg(unix)]
    #[test]
    fn item_counts_follow_deletes_undo_and_hidden_rows() {
        let t = tree("items");
        fs::write(t.0.join("w/.env"), b"e").unwrap();
        let mut a = app(&t);
        let count = |a: &App, name: &str| -> u64 {
            let p = t.0.join("w").join(name);
            let v = &a.cache[&t.0.join("w")];
            v.rows
                .iter()
                .chain(&v.stash)
                .find(|r| r.path == p)
                .unwrap()
                .items
        };
        // big holds inner, inner/blob and side; small holds s.
        assert_eq!(count(&a, "big"), 3);
        assert_eq!(view_items(a.view()), 8);
        // Hidden dot names still count, as they do in the size.
        a.on_key(KeyCode::Char('.'), KeyModifiers::NONE);
        assert_eq!(view_items(a.view()), 8);
        a.select(0);
        a.open(); // big
        a.select(0); // inner and its blob
        a.ask_delete();
        a.on_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(!t.0.join("w/big/inner").exists());
        assert_eq!(view_items(a.view()), 1);
        a.back();
        assert_eq!(count(&a, "big"), 1);
        assert_eq!(view_items(a.view()), 6);
        // A rescan of w counts what the explorer kept.
        a.on_key(KeyCode::Char('r'), KeyModifiers::NONE);
        assert_eq!(count(&a, "big"), 1);
        assert_eq!(view_items(a.view()), 6);
        a.on_key(KeyCode::Char('u'), KeyModifiers::NONE);
        assert_eq!(a.status, "restored 1 item(s)");
        assert_eq!(count(&a, "big"), 3);
        assert_eq!(view_items(a.view()), 8);
        let _ = fs::remove_dir_all(&t.0);
    }

    /// `du -o FILE` of `dirs` written to `file`, then read back.
    fn snapshot_of(dirs: &[PathBuf], file: &Path) -> crate::du_snap::Snapshot {
        let opts = crate::du_snap::snapshot_opts(ScanOpts::default());
        let report = du::scan(dirs, &opts).unwrap();
        crate::du_snap::write_file(file, &report, &opts, 7).unwrap();
        crate::du_snap::read(file).unwrap()
    }

    #[test]
    fn a_snapshot_is_browsed_read_only_with_no_walk() {
        let t = tree("snapbrowse");
        let file = t.0.join("w.snap");
        let snap = snapshot_of(&[t.0.join("w")], &file);
        // The tree goes away: every view has to come from the file.
        fs::remove_dir_all(t.0.join("w")).unwrap();
        let mut a = App::from_snapshot("rtrash", &file, &snap, ScanOpts::default()).unwrap();
        assert!(a.read_only);
        assert!(a.status.starts_with("browsing '"), "{}", a.status);
        // Directories only, as the snapshot records them, largest first.
        assert_eq!(names(&a), ["big", "small"]);
        assert_eq!(a.view().disk, snap.roots[0].disk);
        a.select(0);
        a.open();
        assert_eq!(a.cur, t.0.join("w/big"));
        assert_eq!(names(&a), ["inner"]);
        a.select(0);
        a.open(); // inner holds no directory
        assert_eq!(a.cur, t.0.join("w/big/inner"));
        assert!(a.rows().is_empty());
        a.back();
        a.back();
        assert_eq!(a.cur, t.0.join("w"));
        a.on_key(KeyCode::Char('r'), KeyModifiers::NONE);
        assert!(a.status.contains("nothing to rescan"), "{}", a.status);
        a.on_key(KeyCode::Char('E'), KeyModifiers::SHIFT);
        assert_eq!(a.mode, Mode::Browse);
        assert!(a.status.starts_with("browsing a snapshot"), "{}", a.status);
        a.select(0);
        a.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(a.mode, Mode::Browse);
        assert!(a.status.contains("read-only"), "{}", a.status);
        assert!(a.jobs.is_empty());
        assert!(a.walk(&t.0.join("w")).is_err());
        let text = tui_term::render_text(100, 14, |f| ui(f, &mut a));
        assert!(text.contains("rtrash du (snapshot, read-only)"), "{text}");
        assert!(!text.contains("r rescan"), "{text}");
        let _ = fs::remove_dir_all(&t.0);
    }

    #[test]
    fn a_snapshot_of_two_roots_opens_on_a_view_of_both() {
        let t = tree("snaproots");
        let (big, small) = (t.0.join("w/big"), t.0.join("w/small"));
        let file = t.0.join("roots.snap");
        let snap = snapshot_of(&[small, big.clone()], &file);
        let mut a = App::from_snapshot("rtrash", &file, &snap, ScanOpts::default()).unwrap();
        assert!(is_top_of_roots(&a.cur));
        assert_eq!(names(&a), ["big", "small"]);
        // big holds side, inner and inner/blob, as a scan counts them.
        assert_eq!(a.rows()[0].items, 3);
        let total = du::size_sum(snap.roots.iter().map(|r| r.disk));
        assert_eq!(a.view().disk, total);
        a.select(0);
        a.open();
        assert_eq!(a.cur, big);
        assert_eq!(names(&a), ["inner"]);
        a.back();
        assert!(is_top_of_roots(&a.cur));
        // Roots one inside the other cannot share a top view.
        let nested = snapshot_of(&[t.0.join("w"), big], &t.0.join("nested.snap"));
        let why = App::from_snapshot("rtrash", &file, &nested, ScanOpts::default())
            .err()
            .unwrap();
        assert!(why.contains("one inside the other"), "{why}");
        let _ = fs::remove_dir_all(&t.0);
    }
}
