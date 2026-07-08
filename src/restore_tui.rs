//! Interactive restore browser (ratatui): multi-select, fuzzy filter, multi-restore.

use std::io::{self};
use std::path::Path;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::list::Entry;
use crate::restore;
use crate::tui_fuzzy;
use crate::tui_keys;
use crate::tui_list;
use crate::tui_select::Selection;
use crate::tui_term;

/// Fuzzy-rank entry indices by original path. Empty query keeps original order.
pub fn filter_indices(entries: &[&Entry], query: &str) -> Vec<usize> {
    let hays: Vec<String> = entries
        .iter()
        .map(|e| e.original.to_string_lossy().into_owned())
        .collect();
    let refs: Vec<&str> = hays.iter().map(String::as_str).collect();
    tui_fuzzy::rank_indices(&refs, query)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Browse,
    Filter,
    Help,
    ConfirmOverwrite,
    ConfirmBulk,
}

struct App<'a> {
    prog: &'a str,
    entries: Vec<&'a Entry>,
    filtered: Vec<usize>,
    list_state: ListState,
    sel: Selection,
    force: bool,
    mode: Mode,
    filter: String,
    filter_draft: String,
    status: String,
    quit: bool,
    pending_idxs: Vec<usize>,
    /// Inner list rows from last draw (for PageUp/Down).
    viewport_rows: usize,
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
            sel: Selection::new(),
            force,
            mode: Mode::Browse,
            filter: String::new(),
            filter_draft: String::new(),
            status: format!(
                "{n} item(s) · {} ",
                tui_keys::browse_footer("restore", "f force")
            ),
            quit: false,
            pending_idxs: Vec::new(),
            viewport_rows: 10,
        };
        app.refilter_query(&app.filter.clone());
        app
    }

    fn refilter(&mut self) {
        let q = self.filter.clone();
        self.refilter_query(&q);
    }

    /// Live or applied filter: rebuild visible indices from `query`.
    fn refilter_query(&mut self, query: &str) {
        let prev = self.selected_entry_idx();
        self.filtered = filter_indices(&self.entries, query);
        let new_sel = tui_list::reselect_after_filter(prev, &self.filtered);
        self.list_state.select(new_sel);
        self.ensure_scroll();
    }

    fn ensure_scroll(&mut self) {
        let off = tui_list::scroll_offset(
            self.list_state.selected(),
            self.filtered.len(),
            self.viewport_rows.max(1),
            self.list_state.offset(),
        );
        *self.list_state.offset_mut() = off;
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
        let next = tui_list::move_selected(self.list_state.selected(), self.filtered.len(), delta);
        self.list_state.select(next);
        self.ensure_scroll();
    }

    fn page(&mut self, down: bool) {
        let next = tui_list::page_selected(
            self.list_state.selected(),
            self.filtered.len(),
            self.viewport_rows.max(1),
            down,
        );
        self.list_state.select(next);
        self.ensure_scroll();
    }

    fn targets_for_action(&self) -> Vec<usize> {
        let marked = self.sel.marked_visible(&self.filtered);
        if !marked.is_empty() {
            marked
        } else if let Some(ei) = self.selected_entry_idx() {
            vec![ei]
        } else {
            Vec::new()
        }
    }

    fn try_restore(&mut self) {
        let targets = self.targets_for_action();
        if targets.is_empty() {
            self.status = "nothing selected".into();
            return;
        }
        // Single target may need overwrite confirm.
        if targets.len() == 1 {
            let ei = targets[0];
            let entry = self.entries[ei];
            let dest_exists = entry.original.symlink_metadata().is_ok();
            if dest_exists && !self.force {
                self.mode = Mode::ConfirmOverwrite;
                self.pending_idxs = vec![ei];
                self.status = format!(
                    "overwrite existing '{}' ?  y confirm · n cancel",
                    entry.original.display()
                );
                return;
            }
            self.perform_restores_tracked(&[ei], self.force || dest_exists);
            return;
        }
        // Bulk: confirm when any dest exists without force.
        let need_confirm = !self.force
            && targets
                .iter()
                .any(|&ei| self.entries[ei].original.symlink_metadata().is_ok());
        if need_confirm {
            self.mode = Mode::ConfirmBulk;
            self.pending_idxs = targets;
            self.status = format!(
                "restore {} items (some destinations exist)?  y force-all · n cancel",
                self.pending_idxs.len()
            );
            return;
        }
        self.perform_restores_tracked(&targets, self.force);
    }

    fn perform_restores_tracked(&mut self, idxs: &[usize], force: bool) {
        let result = restore_selection(self.prog, &self.entries, idxs, force);
        // Only drop UI rows that actually restored (failed stay visible).
        for &ei in result.succeeded_idxs.iter().rev() {
            if ei < self.entries.len() {
                self.entries.remove(ei);
            }
        }
        self.sel.remap_after_removals(&result.succeeded_idxs);
        self.mode = Mode::Browse;
        self.pending_idxs.clear();
        self.refilter();
        let ok = result.ok_count();
        let fail = result.fail_count;
        if self.entries.is_empty() {
            self.status = format!("restored {ok} · trash empty · q quit");
        } else {
            self.status = format!(
                "restored {ok} · fail {fail} · {} left · Space mark · Enter again",
                self.entries.len()
            );
        }
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        match self.mode {
            Mode::Filter => self.on_key_filter(code),
            Mode::Help => self.on_key_help(code),
            Mode::ConfirmOverwrite | Mode::ConfirmBulk => self.on_key_confirm(code),
            Mode::Browse => self.on_key_browse(code, mods),
        }
    }

    fn on_key_help(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                self.mode = Mode::Browse;
                self.status = "help closed".into();
            }
            _ => {}
        }
    }

    fn on_key_filter(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                // Restore prior applied filter (discard draft).
                let applied = self.filter.clone();
                self.filter_draft = applied.clone();
                self.refilter_query(&applied);
                self.mode = Mode::Browse;
                self.status = tui_keys::status_filter_cancelled().into();
            }
            KeyCode::Enter => {
                self.filter = self.filter_draft.clone();
                self.mode = Mode::Browse;
                self.status =
                    tui_keys::status_filter_committed(&self.filter, self.filtered.len());
            }
            KeyCode::Backspace => {
                self.filter_draft.pop();
                let d = self.filter_draft.clone();
                self.refilter_query(&d);
                self.status = tui_keys::status_filter_live(&d, self.filtered.len());
            }
            KeyCode::Char(c) if !c.is_control() => {
                self.filter_draft.push(c);
                let d = self.filter_draft.clone();
                self.refilter_query(&d);
                self.status = tui_keys::status_filter_live(&d, self.filtered.len());
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            _ => {}
        }
    }

    fn on_key_confirm(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                let idxs = std::mem::take(&mut self.pending_idxs);
                self.perform_restores_tracked(&idxs, true);
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.pending_idxs.clear();
                self.mode = Mode::Browse;
                self.status = "cancelled".into();
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
                    self.ensure_scroll();
                }
            }
            KeyCode::End | KeyCode::Char('G') => {
                if !self.filtered.is_empty() {
                    self.list_state.select(Some(self.filtered.len() - 1));
                    self.ensure_scroll();
                }
            }
            KeyCode::Char(' ') => {
                if let Some(ei) = self.selected_entry_idx() {
                    self.sel.toggle(ei);
                    self.status = tui_keys::status_marked(self.sel.len());
                }
            }
            KeyCode::Char('a') => {
                self.sel.mark_all(self.filtered.iter().copied());
                self.status = tui_keys::status_marked_all(self.sel.len());
            }
            KeyCode::Char('A') => {
                self.sel.clear();
                self.status = tui_keys::status_cleared().into();
            }
            KeyCode::Char('/') => {
                self.mode = Mode::Filter;
                self.filter_draft = self.filter.clone();
                let d = self.filter_draft.clone();
                self.refilter_query(&d);
                self.status = tui_keys::status_filter_live(&d, self.filtered.len());
            }
            KeyCode::Char('?') => {
                self.mode = Mode::Help;
                self.status = "help · ? or Esc to close".into();
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

    // Inner list height (minus borders) drives PageUp/Down and scroll clamp.
    app.viewport_rows = chunks[1].height.saturating_sub(2) as usize;
    app.ensure_scroll();

    let filter_show = if app.mode == Mode::Filter {
        format!(" /{}", app.filter_draft)
    } else if app.filter.is_empty() {
        String::new()
    } else {
        format!(" fuzzy:{:?}", app.filter)
    };
    let title = format!(
        " rtrash restore · {} item(s) · {} marked{} ",
        app.entries.len(),
        app.sel.len(),
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
            let mark = if app.sel.is_marked(ei) { "[x]" } else { "[ ]" };
            let line = format!("{mark} {:<20}  {}", date_label(e), e.original.display());
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" mark · deleted at · original path "),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD))
        .highlight_symbol("▸ ");
    f.render_stateful_widget(list, chunks[1], &mut app.list_state);

    let keys = match app.mode {
        Mode::Browse => tui_keys::browse_footer("restore", "f force"),
        Mode::Filter => tui_keys::FILTER_HINT.to_string(),
        Mode::Help => tui_keys::HELP_HINT.to_string(),
        Mode::ConfirmOverwrite | Mode::ConfirmBulk => tui_keys::CONFIRM_HINT.to_string(),
    };
    let footer = Paragraph::new(vec![Line::from(keys), Line::from(app.status.as_str())])
        .block(Block::default().borders(Borders::ALL).title("keys · ? help"));
    f.render_widget(footer, chunks[2]);

    if app.mode == Mode::Help {
        draw_help(f, &["Browser-specific: f toggle force overwrite"]);
    }

    if matches!(app.mode, Mode::ConfirmOverwrite | Mode::ConfirmBulk) {
        if let Some(e) = app.selected_entry() {
            draw_confirm(f, &e.original, app.pending_idxs.len());
        } else if !app.pending_idxs.is_empty() {
            draw_confirm_bulk(f, app.pending_idxs.len());
        }
    }
}

fn draw_help(f: &mut ratatui::Frame, extras: &[&str]) {
    let area = {
        let r = f.area();
        let v = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(10),
                Constraint::Percentage(80),
                Constraint::Percentage(10),
            ])
            .split(r);
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(10),
                Constraint::Percentage(80),
                Constraint::Percentage(10),
            ])
            .split(v[1])[1]
    };
    f.render_widget(Clear, area);
    let mut lines: Vec<Line> = tui_keys::core_help_lines()
        .iter()
        .map(|s| Line::from(*s))
        .collect();
    if !extras.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from("This browser"));
        for e in extras {
            lines.push(Line::from(format!("  {e}")));
        }
    }
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" help ")),
        area,
    );
}

fn draw_confirm(f: &mut ratatui::Frame, dest: &Path, n: usize) {
    let area = centered_rect(70, 6, f.area());
    let msg = if n > 1 {
        format!(" Restore {n} items (overwrite as needed)?\n [y] yes  [n] cancel ")
    } else {
        format!(
            " Destination exists:\n {}\n [y] overwrite  [n] cancel ",
            dest.display()
        )
    };
    let block = Paragraph::new(msg).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" confirm ")
            .style(Style::default().add_modifier(Modifier::BOLD)),
    );
    f.render_widget(Clear, area);
    f.render_widget(block, area);
}

fn draw_confirm_bulk(f: &mut ratatui::Frame, n: usize) {
    let area = centered_rect(60, 5, f.area());
    let msg = format!(" Restore {n} marked items?\n [y] yes  [n] cancel ");
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
    if entries.len() == 1 {
        return restore::restore_entry(prog, entries[0], force);
    }

    let mut terminal = match tui_term::enter() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{prog}: cannot start TUI: {e}");
            return 1;
        }
    };

    let mut app = App::new(prog, entries, force);
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

/// Result of multi-select restore: which input indices were restored.
#[derive(Debug, Clone)]
pub struct RestoreSelectionResult {
    /// Indices into the input `entries` slice that restored successfully.
    pub succeeded_idxs: Vec<usize>,
    pub fail_count: u32,
}

impl RestoreSelectionResult {
    pub fn ok_count(&self) -> u32 {
        self.succeeded_idxs.len() as u32
    }
}

/// Restore a set of entry indices via the real `restore_entry` path.
/// `entries` is the full list; `idxs` are indices into it. Restores high→low
/// so callers can remove succeeded indices afterward. Does not mutate `entries`.
pub fn restore_selection(
    prog: &str,
    entries: &[&Entry],
    idxs: &[usize],
    force: bool,
) -> RestoreSelectionResult {
    let mut sorted: Vec<usize> = idxs.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut succeeded_idxs = Vec::new();
    let mut fail_count = 0u32;
    for &ei in sorted.iter().rev() {
        if ei >= entries.len() {
            fail_count += 1;
            continue;
        }
        if restore::restore_entry(prog, entries[ei], force) == 0 {
            succeeded_idxs.push(ei);
        } else {
            fail_count += 1;
        }
    }
    succeeded_idxs.sort_unstable();
    RestoreSelectionResult {
        succeeded_idxs,
        fail_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    }

    #[test]
    fn filter_fuzzy_ranks_notes_first() {
        let a = entry("/var/cache/n_x_o_t_e_s_backup.bin");
        let b = entry("/home/user/Notes.md");
        let entries = vec![&a, &b];
        let ranked = filter_indices(&entries, "notes");
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0], 1);
    }

    #[test]
    fn multi_select_visible_and_remap() {
        let mut s = Selection::new();
        s.toggle(0);
        s.toggle(2);
        assert_eq!(s.marked_visible(&[2, 0, 1]), vec![2, 0]);
        s.remap_after_removals(&[0]);
        assert_eq!(s.marked_sorted(), vec![1]); // 2 -> 1
    }
}
