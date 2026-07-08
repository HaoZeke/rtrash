//! Interactive empty browser: multi-select permanent delete with confirm.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::list::Entry;
use crate::trashdir;
use crate::tui_fuzzy;
use crate::tui_select::Selection;
use crate::tui_term;

pub fn filter_indices(entries: &[&Entry], query: &str) -> Vec<usize> {
    let hays: Vec<String> = entries
        .iter()
        .map(|e| e.original.to_string_lossy().into_owned())
        .collect();
    let refs: Vec<&str> = hays.iter().map(String::as_str).collect();
    tui_fuzzy::rank_indices(&refs, query)
}

/// Result of a permanent-remove batch: per-input success flags + dry-run bytes.
#[derive(Debug, Clone)]
pub struct RemoveBatchResult {
    /// `true` iff the entry at the same index in the input slice was removed
    /// (or counted in dry-run).
    pub succeeded: Vec<bool>,
    pub reclaim_bytes: u64,
}

impl RemoveBatchResult {
    pub fn ok_count(&self) -> u32 {
        self.succeeded.iter().filter(|&&s| s).count() as u32
    }
    pub fn fail_count(&self) -> u32 {
        self.succeeded.iter().filter(|&&s| !s).count() as u32
    }
}

/// Permanently remove the given trash entries (real FreeDesktop paths).
/// Does not reimplement wipe logic: uses `trashdir::remove_any_path` + info unlink.
pub fn permanently_remove_entries(
    prog: &str,
    entries: &[&Entry],
    dry_run: bool,
) -> RemoveBatchResult {
    let mut succeeded = Vec::with_capacity(entries.len());
    let mut bytes = 0u64;
    for entry in entries {
        let payload = entry.dir.files().join(&entry.name);
        let info_path = entry.dir.info().join(format!("{}.trashinfo", entry.name));
        if dry_run {
            bytes = bytes.saturating_add(trashdir::entry_reclaim_bytes(&entry.dir, &entry.name));
            succeeded.push(true);
            continue;
        }
        if let Err(e) = trashdir::remove_any_path(&payload) {
            eprintln!("{prog}: cannot remove '{}': {e}", payload.display());
            succeeded.push(false);
            continue;
        }
        if let Err(e) = std::fs::remove_file(&info_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!("{prog}: cannot remove '{}': {e}", info_path.display());
                succeeded.push(false);
                continue;
            }
        }
        trashdir::directorysizes_remove(&entry.dir, &entry.name);
        succeeded.push(true);
    }
    RemoveBatchResult {
        succeeded,
        reclaim_bytes: bytes,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Browse,
    Filter,
    Confirm,
}

struct App<'a> {
    prog: &'a str,
    entries: Vec<&'a Entry>,
    filtered: Vec<usize>,
    list_state: ListState,
    sel: Selection,
    mode: Mode,
    filter: String,
    filter_draft: String,
    status: String,
    quit: bool,
    dry_run: bool,
}

impl<'a> App<'a> {
    fn new(prog: &'a str, entries: Vec<&'a Entry>, dry_run: bool) -> Self {
        let mut list_state = ListState::default();
        if !entries.is_empty() {
            list_state.select(Some(0));
        }
        let n = entries.len();
        let mut app = Self {
            prog,
            entries,
            filtered: (0..n).collect(),
            list_state,
            sel: Selection::new(),
            mode: Mode::Browse,
            filter: String::new(),
            filter_draft: String::new(),
            status: format!(
                "{n} item(s) · Space mark · a all · Enter purge marked · e mark all · / fuzzy · q"
            ),
            quit: false,
            dry_run,
        };
        app.refilter();
        app
    }

    fn refilter(&mut self) {
        let prev = self.selected_entry_idx();
        self.filtered = filter_indices(&self.entries, &self.filter);
        let new_sel = prev
            .and_then(|ei| self.filtered.iter().position(|&i| i == ei))
            .or(if self.filtered.is_empty() {
                None
            } else {
                Some(0)
            });
        self.list_state.select(new_sel);
    }

    fn selected_entry_idx(&self) -> Option<usize> {
        self.list_state
            .selected()
            .and_then(|i| self.filtered.get(i).copied())
    }

    fn move_sel(&mut self, delta: isize) {
        let len = self.filtered.len();
        if len == 0 {
            self.list_state.select(None);
            return;
        }
        let cur = self.list_state.selected().unwrap_or(0) as isize;
        self.list_state
            .select(Some((cur + delta).rem_euclid(len as isize) as usize));
    }

    fn targets(&self) -> Vec<usize> {
        let m = self.sel.marked_visible(&self.filtered);
        if !m.is_empty() {
            m
        } else if let Some(ei) = self.selected_entry_idx() {
            vec![ei]
        } else {
            Vec::new()
        }
    }

    fn try_purge(&mut self) {
        let t = self.targets();
        if t.is_empty() {
            self.status = "nothing marked · Space to mark or e for all".into();
            return;
        }
        self.mode = Mode::Confirm;
        self.status = format!(
            "PERMANENTLY delete {} item(s){}?  y confirm · n cancel",
            t.len(),
            if self.dry_run { " (dry-run)" } else { "" }
        );
    }

    fn do_purge(&mut self) {
        let idxs = self.targets();
        let batch: Vec<&Entry> = idxs.iter().map(|&i| self.entries[i]).collect();
        let result = permanently_remove_entries(self.prog, &batch, self.dry_run);
        let ok = result.ok_count();
        let fail = result.fail_count();
        if !self.dry_run {
            // Only drop UI rows that actually succeeded (failed stay visible).
            let mut removed_entry_idxs: Vec<usize> = idxs
                .iter()
                .zip(result.succeeded.iter())
                .filter_map(|(&ei, &ok)| ok.then_some(ei))
                .collect();
            removed_entry_idxs.sort_unstable();
            for &ei in removed_entry_idxs.iter().rev() {
                if ei < self.entries.len() {
                    self.entries.remove(ei);
                }
            }
            self.sel.remap_after_removals(&removed_entry_idxs);
            self.refilter();
        }
        self.mode = Mode::Browse;
        self.status = if self.dry_run {
            format!(
                "would remove {ok} ({}) · fail {fail}",
                crate::fastdelete::format_bytes(result.reclaim_bytes)
            )
        } else {
            format!("removed {ok} · fail {fail} · {} left", self.entries.len())
        };
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        match self.mode {
            Mode::Filter => match code {
                KeyCode::Esc => {
                    self.mode = Mode::Browse;
                    self.status = "filter cancelled".into();
                }
                KeyCode::Enter => {
                    self.filter = self.filter_draft.clone();
                    self.mode = Mode::Browse;
                    self.refilter();
                }
                KeyCode::Backspace => {
                    self.filter_draft.pop();
                }
                KeyCode::Char(c) if !c.is_control() => self.filter_draft.push(c),
                _ => {}
            },
            Mode::Confirm => match code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.do_purge(),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.mode = Mode::Browse;
                    self.status = "purge cancelled".into();
                }
                _ => {}
            },
            Mode::Browse => match code {
                KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
                KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => self.quit = true,
                KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
                KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
                KeyCode::Char(' ') => {
                    if let Some(ei) = self.selected_entry_idx() {
                        self.sel.toggle(ei);
                        self.status = format!("marked {}", self.sel.len());
                    }
                }
                KeyCode::Char('a') | KeyCode::Char('e') => {
                    self.sel.mark_all(self.filtered.iter().copied());
                    self.status = format!("marked all visible ({})", self.sel.len());
                }
                KeyCode::Char('A') => {
                    self.sel.clear();
                    self.status = "cleared marks".into();
                }
                KeyCode::Char('/') => {
                    self.mode = Mode::Filter;
                    self.filter_draft = self.filter.clone();
                }
                KeyCode::Char('n') => {
                    self.dry_run = !self.dry_run;
                    self.status = format!("dry_run={}", self.dry_run);
                }
                KeyCode::Enter => self.try_purge(),
                _ => {}
            },
        }
    }
}

fn ui(f: &mut ratatui::Frame, app: &mut App<'_>) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(f.area());
    let filter_show = if app.mode == Mode::Filter {
        format!("/{}", app.filter_draft)
    } else if app.filter.is_empty() {
        String::new()
    } else {
        format!(" fuzzy:{:?}", app.filter)
    };
    let title = format!(
        " rtrash empty · {} · {} marked{}{} ",
        app.entries.len(),
        app.sel.len(),
        filter_show,
        if app.dry_run { " · DRY-RUN" } else { "" }
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        ))
        .block(Block::default().borders(Borders::ALL).title("empty")),
        chunks[0],
    );
    let items: Vec<ListItem> = app
        .filtered
        .iter()
        .map(|&ei| {
            let e = app.entries[ei];
            let mark = if app.sel.is_marked(ei) { "[x]" } else { "[ ]" };
            ListItem::new(format!(
                "{mark} {}  {}",
                e.date.as_deref().unwrap_or("?"),
                e.original.display()
            ))
        })
        .collect();
    f.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" permanent delete "),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD))
            .highlight_symbol("▸ "),
        chunks[1],
        &mut app.list_state,
    );
    let keys = match app.mode {
        Mode::Browse => "Space mark  a/e all  A none  / fuzzy  n dry-run  Enter purge  q quit",
        Mode::Filter => "fuzzy… Enter apply Esc cancel",
        Mode::Confirm => "y permanently delete  n cancel",
    };
    f.render_widget(
        Paragraph::new(vec![Line::from(keys), Line::from(app.status.as_str())])
            .block(Block::default().borders(Borders::ALL).title("keys")),
        chunks[2],
    );
    if app.mode == Mode::Confirm {
        let area = {
            let r = f.area();
            let popup = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Percentage(35),
                    Constraint::Length(5),
                    Constraint::Percentage(35),
                ])
                .split(r);
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(20),
                    Constraint::Percentage(60),
                    Constraint::Percentage(20),
                ])
                .split(popup[1])[1]
        };
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(format!(
                " PERMANENTLY delete {} item(s)?\n [y] yes  [n] cancel ",
                app.targets().len()
            ))
            .block(Block::default().borders(Borders::ALL).title(" confirm ")),
            area,
        );
    }
}

pub fn run(prog: &str, entries: Vec<&Entry>, dry_run: bool) -> i32 {
    if entries.is_empty() {
        eprintln!("{prog}: trash is empty");
        return 0;
    }
    let mut terminal = match tui_term::enter() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{prog}: cannot start TUI: {e}");
            return 1;
        }
    };
    let mut app = App::new(prog, entries, dry_run);
    let result = (|| -> io::Result<()> {
        loop {
            terminal.draw(|f| ui(f, &mut app))?;
            if app.quit {
                break;
            }
            if event::poll(Duration::from_millis(200))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        app.on_key(key.code, key.modifiers);
                    }
                }
            }
        }
        Ok(())
    })();
    tui_term::leave(&mut terminal);
    if let Err(e) = result {
        eprintln!("{prog}: tui error: {e}");
        return 1;
    }
    0
}
