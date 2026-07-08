//! Interactive restore browser (ratatui).
//!
//! First-class TTY UX for `rtrash restore` without a path. Scope is product-
//! driven (user/vault), not peer-identity: expand here when useful.
//!
//! - **Purpose:** browse, filter, and restore trashed items on a TTY. Scripts keep
//!   path restore and piped/`--plain` index selection.
//! - **Layout:** header (title + filter), scrollable table, footer (keys + status).
//! - **Keys:** ↑↓/jk navigate, PgUp/PgDn, g/G, `/` filter, Enter restore,
//!   `f` force, `y`/`n` confirm overwrite, `q`/Esc quit.
//! - **Filter:** case-insensitive substring on original path (draft after `/`;
//!   Enter applies, Esc cancels filter mode).
//! - **After restore:** drop the row and stay open for multi-restore.

use std::io::{self, stdout};
use std::path::Path;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Terminal;

use crate::list::Entry;
use crate::restore;

/// Pure filter: indices into `entries` whose original path matches `query`
/// (case-insensitive substring). Empty query matches all.
pub fn filter_indices(entries: &[&Entry], query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return (0..entries.len()).collect();
    }
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.original.to_string_lossy().to_lowercase().contains(&q))
        .map(|(i, _)| i)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Browse,
    Filter,
    ConfirmOverwrite,
}

struct App<'a> {
    prog: &'a str,
    /// Owned list of matching entries (we remove after restore).
    entries: Vec<&'a Entry>,
    filtered: Vec<usize>,
    list_state: ListState,
    force: bool,
    mode: Mode,
    filter: String,
    filter_draft: String,
    status: String,
    quit: bool,
    /// Pending restore after overwrite confirm.
    pending_idx: Option<usize>,
}

impl<'a> App<'a> {
    fn new(prog: &'a str, entries: Vec<&'a Entry>, force: bool) -> Self {
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
            force,
            mode: Mode::Browse,
            filter: String::new(),
            filter_draft: String::new(),
            status: format!("{n} item(s) · Enter restore · / filter · q quit"),
            quit: false,
            pending_idx: None,
        };
        app.refilter();
        app
    }

    fn refilter(&mut self) {
        let prev = self.selected_entry_idx();
        self.filtered = filter_indices(&self.entries, &self.filter);
        // Keep selection on same entry if still visible.
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

    fn selected_entry(&self) -> Option<&Entry> {
        self.selected_entry_idx().map(|i| self.entries[i])
    }

    fn move_sel(&mut self, delta: isize) {
        let len = self.filtered.len();
        if len == 0 {
            self.list_state.select(None);
            return;
        }
        let cur = self.list_state.selected().unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(len as isize) as usize;
        self.list_state.select(Some(next));
    }

    fn page(&mut self, down: bool) {
        let step = 10isize;
        self.move_sel(if down { step } else { -step });
    }

    fn try_restore(&mut self) {
        let Some(ei) = self.selected_entry_idx() else {
            self.status = "nothing selected".into();
            return;
        };
        let entry = self.entries[ei];
        let dest = &entry.original;
        let dest_exists = dest.symlink_metadata().is_ok();
        if dest_exists && !self.force {
            self.mode = Mode::ConfirmOverwrite;
            self.pending_idx = Some(ei);
            self.status = format!(
                "overwrite existing '{}' ?  y confirm · n cancel",
                dest.display()
            );
            return;
        }
        self.perform_restore(ei, self.force || dest_exists);
    }

    fn perform_restore(&mut self, entry_idx: usize, force: bool) {
        let entry = self.entries[entry_idx];
        let path_disp = entry.original.display().to_string();
        let code = restore::restore_entry(self.prog, entry, force);
        self.mode = Mode::Browse;
        self.pending_idx = None;
        if code == 0 {
            // Drop restored entry; fix selection.
            self.entries.remove(entry_idx);
            self.refilter();
            if self.entries.is_empty() {
                self.status = format!("restored '{path_disp}' · trash empty · q to quit");
            } else {
                self.status = format!(
                    "restored '{path_disp}' · {} left · Enter for next",
                    self.entries.len()
                );
            }
        } else {
            self.status = format!("restore failed for '{path_disp}' (exit {code})");
        }
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        match self.mode {
            Mode::Filter => self.on_key_filter(code),
            Mode::ConfirmOverwrite => self.on_key_confirm(code),
            Mode::Browse => self.on_key_browse(code, mods),
        }
    }

    fn on_key_filter(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.filter_draft.clear();
                self.mode = Mode::Browse;
                self.status = "filter cancelled".into();
            }
            KeyCode::Enter => {
                self.filter = self.filter_draft.clone();
                self.mode = Mode::Browse;
                self.refilter();
                self.status = if self.filter.is_empty() {
                    "filter cleared".into()
                } else {
                    format!(
                        "filter {:?} · {} match(es)",
                        self.filter,
                        self.filtered.len()
                    )
                };
            }
            KeyCode::Backspace => {
                self.filter_draft.pop();
            }
            KeyCode::Char(c) if !c.is_control() => {
                self.filter_draft.push(c);
            }
            _ => {}
        }
    }

    fn on_key_confirm(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                if let Some(ei) = self.pending_idx.take() {
                    self.perform_restore(ei, true);
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.pending_idx = None;
                self.mode = Mode::Browse;
                self.status = "overwrite cancelled".into();
            }
            _ => {}
        }
    }

    fn on_key_browse(&mut self, code: KeyCode, mods: KeyModifiers) {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::PageDown => self.page(true),
            KeyCode::PageUp => self.page(false),
            KeyCode::Home | KeyCode::Char('g') => {
                if !self.filtered.is_empty() {
                    self.list_state.select(Some(0));
                }
            }
            KeyCode::End | KeyCode::Char('G') => {
                if !self.filtered.is_empty() {
                    self.list_state.select(Some(self.filtered.len() - 1));
                }
            }
            KeyCode::Char('/') => {
                self.mode = Mode::Filter;
                self.filter_draft = self.filter.clone();
                self.status = "type filter · Enter apply · Esc cancel".into();
            }
            KeyCode::Char('f') => {
                self.force = !self.force;
                self.status = format!("force={}", self.force);
            }
            KeyCode::Enter => self.try_restore(),
            _ => {}
        }
    }
}

fn date_label(e: &Entry) -> String {
    e.date
        .as_deref()
        .map(|d| d.replacen('T', " ", 1))
        .unwrap_or_else(|| "????-??-?? ??:??:??".into())
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
        format!(" filter:{:?}", app.filter)
    };
    let title = format!(
        " rtrash restore · {} item(s){} ",
        app.entries.len(),
        filter_show
    );
    let header = Paragraph::new(Line::from(vec![
        Span::styled(title, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(if app.force { "  FORCE" } else { "" }),
    ]))
    .block(Block::default().borders(Borders::ALL).title("restore"));
    f.render_widget(header, chunks[0]);

    let items: Vec<ListItem> = app
        .filtered
        .iter()
        .map(|&ei| {
            let e = app.entries[ei];
            let line = format!("{:<20}  {}", date_label(e), e.original.display());
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" deleted at · original path "),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD))
        .highlight_symbol("▸ ");
    f.render_stateful_widget(list, chunks[1], &mut app.list_state);

    let keys = match app.mode {
        Mode::Browse => "↑↓/jk  / filter  Enter restore  f force  q quit",
        Mode::Filter => "typing filter…  Enter apply  Esc cancel",
        Mode::ConfirmOverwrite => "y overwrite  n cancel",
    };
    let footer = Paragraph::new(vec![Line::from(keys), Line::from(app.status.as_str())])
        .block(Block::default().borders(Borders::ALL).title("keys"));
    f.render_widget(footer, chunks[2]);

    if app.mode == Mode::ConfirmOverwrite {
        if let Some(e) = app.selected_entry() {
            draw_confirm(f, &e.original);
        }
    }
}

fn draw_confirm(f: &mut ratatui::Frame, dest: &Path) {
    let area = centered_rect(60, 5, f.area());
    let msg = format!(
        " Destination exists:\n {}\n [y] overwrite  [n] cancel ",
        dest.display()
    );
    let block = Paragraph::new(msg).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" confirm ")
            .style(Style::default().add_modifier(Modifier::BOLD)),
    );
    f.render_widget(Clear, area);
    f.render_widget(block, area);
}

fn centered_rect(percent_x: u16, height: u16, r: Rect) -> Rect {
    let popup = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height.min(90)) / 2),
            Constraint::Length(height),
            Constraint::Percentage((100 - height.min(90)) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup[1])[1]
}

/// Run the restore TUI. Returns process exit code.
pub fn run(prog: &str, entries: Vec<&Entry>, force: bool) -> i32 {
    if entries.is_empty() {
        eprintln!("{prog}: no trashed files match");
        return 1;
    }
    // Single entry: restore immediately without chrome (same as non-TUI path).
    if entries.len() == 1 {
        return restore::restore_entry(prog, entries[0], force);
    }

    if let Err(e) = enable_raw_mode() {
        eprintln!("{prog}: cannot enable raw mode: {e}");
        return 1;
    }
    let mut stdout = stdout();
    if let Err(e) = execute!(stdout, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        eprintln!("{prog}: cannot enter alternate screen: {e}");
        return 1;
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(e) => {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            eprintln!("{prog}: terminal init failed: {e}");
            return 1;
        }
    };

    let mut app = App::new(prog, entries, force);
    let mut code = 0i32;

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

    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    if let Err(e) = result {
        eprintln!("{prog}: tui error: {e}");
        code = 1;
    }
    code
}

#[cfg(test)]
mod tests {
    use super::filter_indices;
    use crate::list::Entry;
    use crate::trashdir::TrashDir;
    use std::path::PathBuf;

    fn entry(path: &str) -> Entry {
        Entry {
            date: None,
            epoch: 0,
            original: PathBuf::from(path),
            name: "n".into(),
            dir: TrashDir {
                root: PathBuf::from("/tmp/trash"),
                topdir: None,
            },
        }
    }

    #[test]
    fn filter_empty_matches_all() {
        let a = entry("/home/u/a.txt");
        let b = entry("/var/log/b.log");
        let entries = vec![&a, &b];
        assert_eq!(filter_indices(&entries, ""), vec![0, 1]);
        assert_eq!(filter_indices(&entries, "   "), vec![0, 1]);
    }

    #[test]
    fn filter_substring_case_insensitive() {
        let a = entry("/Home/User/Notes.md");
        let b = entry("/tmp/cache.bin");
        let entries = vec![&a, &b];
        assert_eq!(filter_indices(&entries, "notes"), vec![0]);
        assert_eq!(filter_indices(&entries, "CACHE"), vec![1]);
        assert_eq!(filter_indices(&entries, "nope"), Vec::<usize>::new());
    }
}
