//! `rtrash du -i`: an interactive disk-usage explorer in the spirit of
//! ncdu. Each directory view comes from a one-level `du` scan, kept in a
//! cache while you move around. Deleting moves rows to the trash, and `u`
//! brings the last deletion back.
//!
//! Scans run in the background: a directory opens at once with a quick
//! listing of its entries (files with their sizes, directories pending),
//! and the full view replaces it when the scan finishes. You can move,
//! open other directories and go back meanwhile.

use std::collections::HashMap;
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
    rx: mpsc::Receiver<io::Result<View>>,
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
        let Ok(items) = trash::os_limited::list() else {
            return false;
        };
        let Some(item) = items
            .into_iter()
            .filter(|i| i.original_path() == *original)
            .max_by_key(|i| i.time_deleted)
        else {
            return false;
        };
        trash::os_limited::restore_all(vec![item]).is_ok()
    }
}

struct App {
    prog: String,
    root: PathBuf,
    cur: PathBuf,
    opts: ScanOpts,
    git: bool,
    /// Trash root for deletes (tests pin one; `None` uses put's choice).
    trash_pin: Option<PathBuf>,
    cache: HashMap<PathBuf, View>,
    list_state: ListState,
    sel: Selection,
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

/// Add (`grow`) or remove sizes along the chain from `dir` up to `root`:
/// each view's totals, and the row that leads to `dir` in its parent.
fn adjust_chain(
    cache: &mut HashMap<PathBuf, View>,
    root: &Path,
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
        if d == root {
            break;
        }
        let Some(parent) = d.parent().map(Path::to_path_buf) else {
            break;
        };
        if let Some(v) = cache.get_mut(&parent) {
            if let Some(row) = v.rows.iter_mut().find(|r| r.path == d) {
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
        let view = scan_view(&root, &opts, git, Sorting::of(opts.sort))?;
        Ok(Self::with_view(prog, root, opts, git, view))
    }

    /// An explorer that opens on a quick listing and scans in the background.
    fn start(prog: &str, root: PathBuf, opts: ScanOpts, git: bool) -> io::Result<Self> {
        let view = quick_view(&root)?;
        let mut app = Self::with_view(prog, root.clone(), opts, git, view);
        app.background = true;
        app.spawn(root, false);
        Ok(app)
    }

    fn with_view(prog: &str, root: PathBuf, opts: ScanOpts, git: bool, view: View) -> Self {
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
            prog: prog.to_string(),
            cur: root.clone(),
            root,
            opts,
            git,
            trash_pin: None,
            cache,
            list_state,
            sel: Selection::new(),
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
        std::thread::spawn(move || {
            let _ = tx.send(scan_view(&d, &opts, git, sorting));
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
        let done: Vec<(PathBuf, io::Result<View>)> = self
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
                Ok(v) => self.take_view(dir, v, refresh),
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
        if dir != self.cur && !dir.exists() {
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
        if refresh {
            self.apply_fresh(&dir, fresh);
        } else {
            self.cache.insert(dir.clone(), fresh);
        }
        if on_screen && !self.filter.is_empty() {
            self.refilter();
        }
        if on_screen {
            self.sel.clear();
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
            match quick_view(&dir) {
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
            match scan_view(&dir, &self.opts, self.git, self.sorting) {
                Ok(v) => {
                    self.cache.insert(dir.clone(), v);
                }
                Err(e) => {
                    self.status = format!("cannot scan '{}': {e}", dir.display());
                    return;
                }
            }
        }
        self.cur = dir;
        self.sel.clear();
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
        if self.cur == self.root {
            self.status = "already at the top".into();
            return;
        }
        let child = self.cur.clone();
        if let Some(parent) = child.parent() {
            self.go(parent.to_path_buf(), Some(&child));
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
        let fresh = match scan_view(&cur, &self.opts, self.git, self.sorting) {
            Ok(v) => v,
            Err(e) => {
                self.status = format!("cannot scan '{}': {e}", cur.display());
                return;
            }
        };
        self.apply_fresh(&cur, fresh);
        if !self.filter.is_empty() {
            self.refilter();
        }
        self.sel.clear();
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
        if cur != self.root {
            if let Some(parent) = cur.parent() {
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
                adjust_chain(&mut self.cache, &self.root, parent, d, a, grow);
                if let Some(pv) = self.cache.get_mut(parent) {
                    if let Some(row) = pv.rows.iter_mut().find(|r| r.path == cur) {
                        row.disk = fresh.disk;
                        row.apparent = fresh.apparent;
                    }
                }
            }
        }
    }

    fn targets(&self) -> Vec<usize> {
        let marked = self.sel.marked_sorted();
        let idxs = if marked.is_empty() {
            self.list_state.selected().into_iter().collect()
        } else {
            marked
        };
        idxs.into_iter()
            .filter(|&i| self.rows().get(i).is_some_and(|r| !is_trash(r)))
            .collect()
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
        let rows = self.rows();
        let bytes: u64 = t.iter().map(|&i| rows[i].disk).sum();
        let risky = t
            .iter()
            .filter(|&&i| rows[i].git.as_ref().is_some_and(|(s, _)| s.is_risky()))
            .count();
        self.mode = Mode::Confirm;
        self.status = if risky > 0 {
            format!(
                "trash {} item(s), {}, {risky} with git work at risk (!)?  y confirm · n cancel",
                t.len(),
                format_bytes(bytes)
            )
        } else {
            format!(
                "trash {} item(s), {}?  y confirm · n cancel",
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
        for &i in &t {
            let item = self.rows()[i].clone();
            match bin::trash(&self.prog, &item.path, self.trash_pin.as_ref()) {
                Some(done) => group.push((done, item)),
                None => failed += 1,
            }
        }
        let gone: Vec<PathBuf> = group.iter().map(|(_, it)| it.path.clone()).collect();
        let (disk, apparent) = group.iter().fold((0u64, 0u64), |(d, a), (_, it)| {
            (d + it.disk, a + it.apparent)
        });
        let cur = self.cur.clone();
        if let Some(v) = self.cache.get_mut(&cur) {
            v.rows.retain(|r| !gone.contains(&r.path));
        }
        // Views below a trashed directory describe paths that are gone.
        self.cache
            .retain(|p, _| !gone.iter().any(|g| p.starts_with(g)));
        adjust_chain(&mut self.cache, &self.root, &cur, disk, apparent, false);
        self.sel.clear();
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
            let Some(parent) = item.path.parent().map(Path::to_path_buf) else {
                continue;
            };
            let (d, a) = (item.disk, item.apparent);
            if let Some(v) = self.cache.get_mut(&parent) {
                if !v.rows.iter().any(|r| r.path == item.path) {
                    v.rows.push(item);
                    sort_rows(&mut v.rows, self.sorting);
                }
            }
            adjust_chain(&mut self.cache, &self.root, &parent, d, a, true);
        }
        if !self.filter.is_empty() {
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

    /// Show only the rows of the current view whose name matches the
    /// filter; the rest wait in the view's stash. The cursor and marks
    /// stay on their rows when those stay visible.
    fn refilter(&mut self) {
        let keep = self.selected().map(|r| r.path.clone());
        let marked: Vec<PathBuf> = self
            .sel
            .marked_sorted()
            .into_iter()
            .filter_map(|i| self.rows().get(i).map(|r| r.path.clone()))
            .collect();
        let pat = self.filter.to_lowercase();
        let sorting = self.sorting;
        let Some(v) = self.cache.get_mut(&self.cur) else {
            return;
        };
        let mut all = std::mem::take(&mut v.rows);
        all.append(&mut v.stash);
        let (shown, hidden): (Vec<Item>, Vec<Item>) =
            all.into_iter().partition(|it| name_matches(it, &pat));
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
                            self.status = format!("{} marked", self.sel.len());
                        }
                    }
                    Some(Action::MarkAll) => {
                        self.sel.mark_all(0..self.rows().len());
                        self.status = format!("{} marked", self.sel.len());
                    }
                    Some(Action::ClearMarks) => {
                        self.sel.clear();
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
    row_text_in(it, marked, whole, sort, sort == SortKey::Apparent, false)
}

/// [`row_text`]; `apparent` shows apparent sizes instead of disk use, and
/// `pending` shows a directory whose size is still coming.
fn row_text_in(
    it: &Item,
    marked: bool,
    whole: u64,
    sort: SortKey,
    apparent: bool,
    pending: bool,
) -> String {
    if pending && it.is_dir {
        let mark = if marked { "[x]" } else { "[ ]" };
        let name = it.path.file_name().unwrap_or_default().to_string_lossy();
        return format!("{mark} {:>10} [{}] {name}/", "…", " ".repeat(10));
    }
    let mark = if marked { "[x]" } else { "[ ]" };
    let risky = it.git.as_ref().is_some_and(|(s, _)| s.is_risky());
    let warn = if risky { "!" } else { " " };
    let name = it.path.file_name().unwrap_or_default().to_string_lossy();
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
    let entries = if app.filter.is_empty() {
        format!("{} entries", view.rows.len())
    } else {
        format!(
            "{} of {} entries",
            view.rows.len(),
            view.rows.len() + view.stash.len()
        )
    };
    let tail = format!(" · {size} · {entries} · {} marked ", app.sel.len());
    let room = (chunks[0].width as usize)
        .saturating_sub(2)
        .saturating_sub(tail.chars().count() + 1);
    let title = format!(" {}{tail}", fit_left(&app.cur.display().to_string(), room));
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
            ))
        })
        .collect();
    f.render_widget(
        Paragraph::new(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        ))
        .block(Block::default().borders(Borders::ALL).title(format!(
            "rtrash du{} · by {}{}{}",
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

/// Run the explorer on `root`. Returns the process exit code.
pub fn run(prog: &str, root: &Path, opts: ScanOpts, git: bool, read_only: bool) -> i32 {
    let root = match root.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{prog}: cannot access '{}': {e}", root.display());
            return 1;
        }
    };
    if !root.is_dir() {
        eprintln!("{prog}: -i needs a directory: '{}'", root.display());
        return 2;
    }
    let mut app = match App::start(prog, root, opts, git) {
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
        let mut a = App::start("rtrash", t.0.join("w"), ScanOpts::default(), false).unwrap();
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
}
