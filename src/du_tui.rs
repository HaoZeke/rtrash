//! `rtrash du -i`: an interactive disk-usage explorer in the spirit of
//! ncdu. Each directory view comes from a one-level `du` scan, kept in a
//! cache while you move around. Deleting moves rows to the trash, and `u`
//! brings the last deletion back.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::du::{self, Item, ScanOpts, SortKey};
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
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Browse,
    Confirm,
    Help,
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
}

/// The size a row shows and its bar measures: apparent size under
/// `--apparent`, disk use otherwise.
fn key_of(it: &Item, sort: SortKey) -> u64 {
    match sort {
        SortKey::Apparent => it.apparent,
        _ => it.disk,
    }
}

/// Rows in the order of `sort`: by size, item count or newest change.
fn sort_rows(rows: &mut [Item], sort: SortKey) {
    rows.sort_by(|a, b| {
        b.key(sort)
            .cmp(&a.key(sort))
            .then_with(|| a.path.cmp(&b.path))
    });
}

fn is_trash(it: &Item) -> bool {
    it.artefact == Some("trash")
}

/// Scan `dir` one level deep: every child directory and file with its
/// total, git state and artefact kind.
fn scan_view(dir: &Path, opts: &ScanOpts, git: bool) -> io::Result<View> {
    let o = ScanOpts {
        top: usize::MAX,
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
    sort_rows(&mut rows, opts.sort);
    Ok(View {
        rows,
        disk,
        apparent,
        cursor: 0,
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
    fn new(prog: &str, root: PathBuf, opts: ScanOpts, git: bool) -> io::Result<Self> {
        let view = scan_view(&root, &opts, git)?;
        let mut cache = HashMap::new();
        let n = view.rows.len();
        cache.insert(root.clone(), view);
        let mut list_state = ListState::default();
        if n > 0 {
            list_state.select(Some(0));
        }
        let keys = Keymap::load();
        Ok(App {
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
        let cursor = self.list_state.selected().unwrap_or(0);
        if let Some(v) = self.cache.get_mut(&self.cur) {
            v.cursor = cursor;
        }
        if !self.cache.contains_key(&dir) {
            match scan_view(&dir, &self.opts, self.git) {
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
        let fresh = match scan_view(&cur, &self.opts, self.git) {
            Ok(v) => v,
            Err(e) => {
                self.status = format!("cannot scan '{}': {e}", cur.display());
                return;
            }
        };
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
        self.sel.clear();
        let at = self.list_state.selected().unwrap_or(0);
        self.select(at);
        self.status = format!("rescanned · {} entries", self.rows().len());
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
                    sort_rows(&mut v.rows, self.opts.sort);
                }
            }
            adjust_chain(&mut self.cache, &self.root, &parent, d, a, true);
        }
        let at = self.list_state.selected().unwrap_or(0);
        self.select(at);
        self.status = match failed {
            0 => format!("restored {back} item(s)"),
            f => format!("restored {back}, {f} failed (path taken again?)"),
        };
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        match self.mode {
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
                        _ => {}
                    }
                }
                match self.keys.resolve_browse(code, mods) {
                    Some(Action::Quit | Action::QuitHard) => self.quit = true,
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

fn row_text(it: &Item, marked: bool, whole: u64, sort: SortKey) -> String {
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
    let badges = if badges.is_empty() {
        String::new()
    } else {
        format!("  [{}]", badges.join(" "))
    };
    format!(
        "{mark}{warn}{:>10} [{}] {name}{slash}{badges}",
        format_bytes(key_of(it, sort)),
        bar(key_of(it, sort), whole, 10)
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
    let sort = app.opts.sort;
    let view = app.view();
    let whole = match sort {
        SortKey::Apparent => view.apparent,
        _ => view.disk,
    };
    let title = format!(
        " {} · {} · {} entries · {} marked ",
        app.cur.display(),
        format_bytes(whole),
        view.rows.len(),
        app.sel.len()
    );
    let items: Vec<ListItem> = view
        .rows
        .iter()
        .enumerate()
        .map(|(i, it)| ListItem::new(row_text(it, app.sel.is_marked(i), whole, sort)))
        .collect();
    f.render_widget(
        Paragraph::new(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        ))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(if app.read_only {
                    "rtrash du (read-only)"
                } else {
                    "rtrash du"
                }),
        ),
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
                "! — git work at risk (dirty, unpushed or unknown)",
            ],
        );
    }
}

fn event_loop(terminal: &mut tui_term::Term, app: &mut App) -> io::Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;
        if app.quit {
            return Ok(());
        }
        if event::poll(Duration::from_millis(200))? {
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
    let mut app = match App::new(prog, root, opts, git) {
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
        assert!(
            text.contains(&format!("{} · ", t.0.join("w").display())),
            "{text}"
        );
        assert!(text.contains("big/"), "{text}");
        assert!(text.contains("d trash  u undo  r rescan"), "{text}");
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
