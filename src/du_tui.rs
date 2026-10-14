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
//! Given several directories (`du -i a b`, as `dua i a b`), the explorer
//! opens on a view of its own above them, one row per directory, kept in
//! the cache under the empty path, which names no directory.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
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

/// Trash and untrash for the browser: the FreeDesktop trash on Unix, the
/// Recycle Bin on Windows.
#[cfg(unix)]
mod bin {
    use std::path::{Path, PathBuf};

    use crate::put;

    /// What a delete left behind, enough to undo it.
    pub type Ticket = put::Trashed;

    pub fn trash(prog: &str, path: &Path, pin: Option<&PathBuf>) -> Option<Ticket> {
        let opts = put::Opts {
            recursive: true,
            force: true,
            quiet: true,
            trash_pin: pin.cloned(),
            ..put::Opts::default()
        };
        put::put_one(prog, path, &opts).ok().flatten()
    }

    pub fn untrash(t: &Ticket) -> bool {
        let Ok(dir) = crate::trashdir::TrashDir::from_root(&t.trash_dir) else {
            return false;
        };
        let entry = crate::list::Entry {
            date: None,
            epoch: 0,
            original: t.original.clone(),
            name: t.name.clone(),
            dir,
            batch: None,
        };
        crate::restore::restore_one(&entry, false).is_ok()
    }
}

#[cfg(windows)]
mod bin {
    use std::path::{Path, PathBuf};

    /// The original path; undo restores the newest Recycle Bin item from it.
    pub type Ticket = PathBuf;

    pub fn trash(_prog: &str, path: &Path, _pin: Option<&PathBuf>) -> Option<Ticket> {
        trash::delete(path).ok().map(|()| path.to_path_buf())
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
        crate::win_recycle::restore_payload(std::path::Path::new(&item.id), original).is_ok()
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
    /// Scan in the background (the terminal session); tests scan inline.
    background: bool,
    jobs: HashMap<PathBuf, Job>,
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
    /// Paths `p` prints on stdout once the explorer has closed.
    print_on_exit: Vec<PathBuf>,
    /// Text `y` sends to the terminal clipboard on the next draw.
    clipboard: Option<String>,
    /// What gets the terminal on the next turn of the loop (`b`, `v`).
    hand_off: Option<HandOff>,
    /// Program and arguments `o` opens a row with (tests set their own).
    opener: Vec<std::ffi::OsString>,
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

/// Rows in the order of `s`: largest, most items or newest change first,
/// or by name from A; `reverse` flips it.
fn sort_rows(rows: &mut [Item], s: Sorting) {
    let name = |it: &Item| {
        it.path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase()
    };
    rows.sort_by(|a, b| {
        let o = match s.order {
            Order::Key(k) => b.key(k).cmp(&a.key(k)),
            Order::Name => name(a).cmp(&name(b)),
        }
        .then_with(|| a.path.cmp(&b.path));
        if s.reverse {
            o.reverse()
        } else {
            o
        }
    });
}

fn is_trash(it: &Item) -> bool {
    it.artefact == Some("trash")
}

/// Scan `dir` one level deep: every child directory and file with its
/// total, git state and artefact kind.
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
        ..opts.clone()
    };
    let mut report = du::scan(&[dir.to_path_buf()], &o)?;
    if git {
        du::annotate_git(&mut report);
    }
    let (disk, apparent) = report
        .roots
        .first()
        .map(|r| (r.disk, r.apparent))
        .unwrap_or((0, 0));
    let mut rows: Vec<Item> = report
        .dirs
        .into_iter()
        .chain(report.files)
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
        let disk = std::os::unix::fs::MetadataExt::blocks(&m) * 512;
        #[cfg(not(unix))]
        let disk = m.len();
        let it = Item {
            path: e.path(),
            is_dir,
            apparent: if is_dir { 0 } else { m.len() },
            disk: if is_dir { 0 } else { disk },
            mtime,
            items: u64::from(!is_dir),
            artefact: None,
            repos: Vec::new(),
            git: None,
            unique: None,
        };
        if is_dir {
            dirs.push(it);
        } else {
            files.push(it);
        }
    }
    dirs.sort_by(|a, b| a.path.cmp(&b.path));
    sort_rows(&mut files, Sorting::of(SortKey::Disk));
    let disk = files.iter().map(|f| f.disk).sum();
    let apparent = files.iter().map(|f| f.apparent).sum();
    dirs.extend(files);
    Ok(View {
        rows: dirs,
        disk,
        apparent,
        cursor: 0,
        scanning: true,
        partial: false,
        stash: Vec::new(),
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
    for root in roots {
        let view = scan_view(root, opts, git, sorting)?;
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
            items: view
                .rows
                .iter()
                .map(|r| if r.is_dir { r.items + 1 } else { 1 })
                .sum(),
            artefact: root
                .file_name()
                .and_then(|n| du::artefact_kind(&n.to_string_lossy(), |c| root.join(c).exists())),
            repos,
            git: None,
            unique: None,
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
    let disk = rows.iter().map(|r| r.disk).sum();
    let apparent = rows.iter().map(|r| r.apparent).sum();
    sort_rows(&mut rows, sorting);
    let view = View {
        rows,
        disk,
        apparent,
        cursor: 0,
        scanning: false,
        partial,
        stash: Vec::new(),
    };
    Ok((view, below))
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
    }
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

/// Add (`grow`) or remove sizes along the chain from `dir` up to the
/// top: each view's totals, and the row that leads to `dir` in the view
/// above it.
fn adjust_chain(
    cache: &mut HashMap<PathBuf, View>,
    roots: &[PathBuf],
    dir: &Path,
    disk: u64,
    apparent: u64,
    grow: bool,
) {
    let apply = |v: &mut u64, d: u64| {
        *v = if grow {
            v.saturating_add(d)
        } else {
            v.saturating_sub(d)
        }
    };
    let mut d = dir.to_path_buf();
    loop {
        if let Some(v) = cache.get_mut(&d) {
            apply(&mut v.disk, disk);
            apply(&mut v.apparent, apparent);
        }
        let Some(parent) = up(roots, &d) else {
            break;
        };
        if let Some(v) = cache.get_mut(&parent) {
            if let Some(row) = v.rows.iter_mut().chain(&mut v.stash).find(|r| r.path == d) {
                apply(&mut row.disk, disk);
                apply(&mut row.apparent, apparent);
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
            print_on_exit: Vec::new(),
            clipboard: None,
            hand_off: None,
            opener: tui_term::opener(),
            prog: prog.to_string(),
            cur: root.clone(),
            root,
            roots,
            opts,
            git,
            trash_pin: None,
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
            background: false,
            jobs: HashMap::new(),
        }
    }

    /// Start a background scan of `dir`.
    fn spawn(&mut self, dir: PathBuf, refresh: bool) {
        self.spawn_with(dir, refresh, Arc::new(ScanControl::default()));
    }

    fn spawn_with(&mut self, dir: PathBuf, refresh: bool, control: Arc<ScanControl>) {
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
            match quick_any(&dir, &self.roots) {
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
            match scan_any(&dir, &self.roots, &self.opts, self.git, self.sorting) {
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
        let cur = self.cur.clone();
        if self.jobs.contains_key(&cur) {
            self.status = "still scanning this directory".into();
            return;
        }
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
        let (fresh, below) = match scan_any(&cur, &self.roots, &self.opts, self.git, self.sorting) {
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
        let old = self
            .cache
            .insert(cur.clone(), fresh.clone())
            .expect("current view");
        let Some(parent) = up(&self.roots, &cur) else {
            return;
        };
        let parent = parent.as_path();
        let (grow, d, a) = if fresh.disk >= old.disk {
            (
                true,
                fresh.disk - old.disk,
                fresh.apparent.saturating_sub(old.apparent),
            )
        } else {
            (
                false,
                old.disk - fresh.disk,
                old.apparent.saturating_sub(fresh.apparent),
            )
        };
        // Restore the view's own totals; the chain then fixes the rest.
        adjust_chain(&mut self.cache, &self.roots, parent, d, a, grow);
        if let Some(pv) = self.cache.get_mut(parent) {
            if let Some(row) = pv
                .rows
                .iter_mut()
                .chain(&mut pv.stash)
                .find(|r| r.path == cur)
            {
                row.disk = fresh.disk;
                row.apparent = fresh.apparent;
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

    fn ask_delete(&mut self) {
        if self.read_only {
            self.status = "read-only (--no-delete): nothing gets trashed".into();
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
        let bytes: u64 = t.iter().map(|r| r.disk).sum();
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

    fn delete(&mut self) {
        self.mode = Mode::Browse;
        let t = self.targets();
        let mut group: UndoGroup = Vec::new();
        let mut failed = 0usize;
        for item in t {
            match bin::trash(&self.prog, &item.path, self.trash_pin.as_ref()) {
                Some(done) => group.push((done, item)),
                None => failed += 1,
            }
        }
        let gone: Vec<PathBuf> = group.iter().map(|(_, it)| it.path.clone()).collect();
        let disk: u64 = group.iter().map(|(_, it)| it.disk).sum();
        // Each trashed row leaves its own view and shrinks the views above.
        for (_, it) in &group {
            let Some(parent) = up(&self.roots, &it.path) else {
                continue;
            };
            if let Some(v) = self.cache.get_mut(&parent) {
                v.rows.retain(|r| r.path != it.path);
                v.stash.retain(|r| r.path != it.path);
            }
            adjust_chain(
                &mut self.cache,
                &self.roots,
                &parent,
                it.disk,
                it.apparent,
                false,
            );
        }
        // Views below a trashed directory describe paths that are gone.
        self.cache
            .retain(|p, _| !gone.iter().any(|g| p.starts_with(g)));
        self.sel.clear();
        self.away.clear();
        let at = self.list_state.selected().unwrap_or(0);
        self.select(at);
        let n = group.len();
        if n > 0 {
            self.undo.push(group);
        }
        self.status = match failed {
            0 => format!("trashed {n} item(s), {} · u undo", format_bytes(disk)),
            f => format!("trashed {n}, {f} failed · u undo"),
        };
    }

    /// Restore the most recent delete of this session.
    fn undo_last(&mut self) {
        if self.read_only {
            self.status = "read-only (--no-delete): nothing to undo".into();
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
            let (d, a) = (item.disk, item.apparent);
            if let Some(v) = self.cache.get_mut(&parent) {
                if !v.rows.iter().any(|r| r.path == item.path) {
                    v.rows.push(item);
                    sort_rows(&mut v.rows, self.sorting);
                }
            }
            adjust_chain(&mut self.cache, &self.roots, &parent, d, a, true);
        }
        if self.narrowed() {
            self.refilter();
        }
        let at = self.list_state.selected().unwrap_or(0);
        self.select(at);
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
        let mut all = std::mem::take(&mut v.rows);
        all.append(&mut v.stash);
        let (shown, hidden): (Vec<Item>, Vec<Item>) = all
            .into_iter()
            .partition(|it| name_matches(it, &pat) && !(hide && is_dot_name(it)));
        v.rows = shown;
        v.stash = hidden;
        sort_rows(&mut v.rows, sorting);
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
                        KeyCode::Char('b') => return self.ask_shell(),
                        KeyCode::Char('v') => return self.view_file(),
                        KeyCode::Char('o') => return self.open_row(),
                        KeyCode::Char('.') => return self.toggle_dots(),
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
    let kind = if it.is_dir { "directory" } else { "file" };
    out.push(match it.artefact {
        Some(a) => format!("kind      {kind}, {a}"),
        None => format!("kind      {kind}"),
    });
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

/// `lines` broken into rows of at most `width` characters.
fn hard_wrap(lines: &[String], width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for l in lines {
        let chars: Vec<char> = l.chars().collect();
        if chars.is_empty() {
            out.push(String::new());
        }
        for row in chars.chunks(width) {
            out.push(row.iter().collect());
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

/// `s` cut to at most `max` characters by dropping its start, marked
/// with `…`, so the end of a path stays readable.
fn fit_left(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let keep: String = s.chars().skip(n - (max - 1)).collect();
    format!("…{keep}")
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

#[cfg(test)]
fn row_text(it: &Item, marked: bool, whole: u64, sort: SortKey) -> String {
    row_text_in(
        it,
        marked,
        whole,
        sort,
        sort == SortKey::Apparent,
        false,
        false,
    )
}

/// [`row_text`]; `apparent` shows apparent sizes instead of disk use,
/// `pending` shows a directory whose size is still coming, and `whole_path`
/// names the row by its path (the rows of several roots) instead of its
/// last part.
fn row_text_in(
    it: &Item,
    marked: bool,
    whole: u64,
    sort: SortKey,
    apparent: bool,
    pending: bool,
    whole_path: bool,
) -> String {
    let name = if whole_path {
        it.path.to_string_lossy()
    } else {
        it.path.file_name().unwrap_or_default().to_string_lossy()
    };
    if pending && it.is_dir {
        let mark = if marked { "[x]" } else { "[ ]" };
        return format!("{mark} {:>10} [{}] {name}/", "…", " ".repeat(10));
    }
    let mark = if marked { "[x]" } else { "[ ]" };
    let risky = it.git.as_ref().is_some_and(|(s, _)| s.is_risky());
    let warn = if risky { "!" } else { " " };
    let slash = if it.is_dir { "/" } else { "" };
    let mut badges = Vec::new();
    if let Some(a) = it.artefact {
        badges.push(a.to_string());
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
    format!(
        "{mark}{warn}{:>10} [{}] {name}{slash}{badges}",
        format_bytes(key_of(it, apparent)),
        bar(key_of(it, apparent), whole, 10)
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
    // Sizes, counts and markers always fit; a long path gives way from
    // the left, as in ncdu.
    let entries = if !app.narrowed() {
        format!("{} entries", view.rows.len())
    } else {
        format!(
            "{} of {} entries",
            view.rows.len(),
            view.rows.len() + view.stash.len()
        )
    };
    let tail = format!(" · {size} · {entries} · {} marked ", app.marked_count());
    let room = (chunks[0].width as usize)
        .saturating_sub(2)
        .saturating_sub(tail.chars().count() + 1);
    let place = if is_top_of_roots(&app.cur) {
        crate::util::plural(app.roots.len() as u64, "directory", "directories")
    } else {
        app.cur.display().to_string()
    };
    let title = format!(" {}{tail}", fit_left(&place, room));
    let pending = view.scanning;
    let items: Vec<ListItem> = view
        .rows
        .iter()
        .enumerate()
        .map(|(i, it)| {
            ListItem::new(row_text_in(
                it,
                app.sel.is_marked(i),
                whole,
                sort,
                apparent,
                pending,
                is_top_of_roots(&app.cur),
            ))
        })
        .collect();
    f.render_widget(
        Paragraph::new(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        ))
        .block(Block::default().borders(Borders::ALL).title(format!(
            "rtrash du{} · by {}{}{}{}",
            if app.read_only { " (read-only)" } else { "" },
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
        _ => format!(
            "{}/{} move  {}/→ open  ←/h up  {}r rescan  {} mark  {} help  {} quit",
            app.keys.display_chords(Action::MoveUp),
            app.keys.display_chords(Action::MoveDown),
            app.keys.display_chords(Action::Action),
            if app.read_only {
                ""
            } else {
                "d trash  u undo  "
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
                let room = (w.saturating_sub(2) as usize).saturating_sub(head.chars().count());
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
        let total: u64 = app.targets().iter().map(|t| t.disk).sum();
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
                    "d delete — move to the trash (asks first)"
                },
                if app.read_only {
                    "r — rescan this directory"
                } else {
                    "u — undo the last delete · r — rescan this directory"
                },
                "s n C M — sort by size, name, items or newest change; again reverses",
                "S — show apparent sizes instead of disk use, or back",
                "i — details of the cursor row (any key closes them)",
                "m — every mark, from all directories: space unmarks, enter goes there",
                "p — quit and print the marked paths (or the cursor row) · y — copy them",
                if app.read_only {
                    "b — no shell in read-only mode"
                } else {
                    "b — open a shell in this directory; exit it to come back"
                },
                "v — show the cursor file in $PAGER · o — open the row with its default program",
                ". — hide names that start with a dot, or show them again",
                "/ — show only rows whose name matches (a part, or a glob with * ? [); Esc clears",
                "! — git work at risk (dirty, unpushed or unknown)",
            ],
        );
    }
}

fn event_loop(terminal: &mut tui_term::Term, app: &mut App) -> io::Result<()> {
    loop {
        app.poll_jobs();
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
        if app.quit {
            // Let running scans wind down instead of walking on unseen.
            for job in app.jobs.values() {
                job.control.stop.store(true, Ordering::Relaxed);
            }
            return Ok(());
        }
        // Redraw often while a scan runs so its counters move.
        let tick = if app.jobs.is_empty() { 200 } else { 100 };
        if event::poll(Duration::from_millis(tick))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.on_key(key.code, key.modifiers);
                }
            }
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
            Ok(p) => p,
            Err(e) => {
                eprintln!("{prog}: cannot access '{}': {e}", given.display());
                return Err(1);
            }
        };
        if !root.is_dir() {
            eprintln!("{prog}: -i needs a directory: '{}'", root.display());
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
                outer.display(),
                root.display()
            );
            return Err(2);
        }
        seen.push(root);
    }
    Ok(seen)
}

/// Run the explorer on `roots`: one directory, or several under a view
/// of their own. Returns the process exit code.
pub fn run(prog: &str, roots: &[PathBuf], opts: ScanOpts, git: bool, read_only: bool) -> i32 {
    let seen = match explorer_roots(prog, roots) {
        Ok(r) => r,
        Err(code) => return code,
    };
    let mut app = match App::start(prog, seen, opts, git) {
        Ok(a) => App { read_only, ..a },
        Err(e) => {
            eprintln!("{prog}: cannot scan: {e}");
            return 1;
        }
    };
    let mut terminal = match tui_term::enter() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{prog}: cannot start TUI: {e}");
            return 1;
        }
    };
    let result = event_loop(&mut terminal, &mut app);
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
        a
    }

    fn names(a: &App) -> Vec<String> {
        a.rows()
            .iter()
            .map(|r| r.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
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
        a.opener = ["/bin/sh", "-c", "cp \"$1\" \"$1.opened\"", "sh"]
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
        assert!(text.contains("big/"), "{text}");
        assert!(text.contains("d trash  u undo  r rescan"), "{text}");
    }

    /// Take in finished background scans until none is left.
    fn settle(a: &mut App) {
        let until = std::time::Instant::now() + Duration::from_secs(20);
        while !a.jobs.is_empty() {
            assert!(std::time::Instant::now() < until, "scan never finished");
            a.poll_jobs();
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
        let text = tui_term::render_text(60, 14, |f| ui(f, &mut a));
        assert!(text.contains("(partial) · 1 entries · 0 marked"), "{text}");
        assert!(text.contains("…eeee"), "{text}");
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
        adjust_chain(&mut a.cache, &roots, &dot, 1000, 1000, false);
        let after = a.view().stash.iter().find(|r| r.path == dot).unwrap().disk;
        assert_eq!(after, before - 1000);
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
}
