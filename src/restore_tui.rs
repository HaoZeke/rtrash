//! Interactive restore browser (ratatui): multi-select, fuzzy filter, multi-restore.

use std::io::{self};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::tui_binds::{Action, Keymap};
use crate::tui_fuzzy;
use crate::tui_keys;
use crate::tui_list;
use crate::tui_select::Selection;
use crate::tui_term;

/// A trashed item the browser can show and restore: a FreeDesktop entry on
/// Unix, a Recycle Bin item on Windows.
pub trait TuiItem {
    /// Where the item came from and goes back to.
    fn original(&self) -> &Path;
    /// Deletion time as `YYYY-MM-DDTHH:MM:SS`, when known.
    fn date(&self) -> Option<&str>;
    /// Put the item back without printing: where it went, plus warnings
    /// that did not stop it. `force` replaces what sits at the original
    /// path.
    fn restore(&self, force: bool) -> Result<(PathBuf, Vec<String>), String>;
    /// True when the payload is gone, so a restore cannot work.
    fn missing(&self) -> bool {
        false
    }
}

/// What a row adds for an item whose payload is gone.
pub const GONE_MARK: &str = "  [payload gone]";

/// One picker row: mark, date, original path, and [`GONE_MARK`] when the
/// payload is gone.
pub fn row_text(mark: &str, date: &str, original: &Path, gone: bool) -> String {
    format!(
        "{mark} {date:<20}  {}{}",
        original.display(),
        if gone { GONE_MARK } else { "" }
    )
}

#[cfg(unix)]
impl TuiItem for crate::list::Entry {
    fn original(&self) -> &Path {
        &self.original
    }
    fn date(&self) -> Option<&str> {
        self.date.as_deref()
    }
    fn restore(&self, force: bool) -> Result<(PathBuf, Vec<String>), String> {
        crate::restore::restore_one(self, force).map(|d| (d.dest, d.warnings))
    }
    fn missing(&self) -> bool {
        crate::list::Entry::missing(self)
    }
}

/// Fuzzy-rank entry indices by original path. Empty query keeps original order.
pub fn filter_indices<E: TuiItem>(entries: &[&E], query: &str) -> Vec<usize> {
    let hays: Vec<String> = entries
        .iter()
        .map(|e| e.original().to_string_lossy().into_owned())
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

struct App<'a, E: TuiItem> {
    prog: &'a str,
    entries: Vec<&'a E>,
    /// Per entry, whether its payload is gone (one stat each, at start).
    gone: Vec<bool>,
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
    keys: Keymap,
    newest_first: bool,
    /// Outcome lines, printed once the terminal is back to normal.
    log: Vec<LogLine>,
}

impl<'a, E: TuiItem> App<'a, E> {
    fn new(prog: &'a str, entries: Vec<&'a E>, force: bool, newest_first: bool) -> Self {
        let mut list_state = ListState::default();
        if !entries.is_empty() {
            list_state.select(Some(0));
        }
        let n = entries.len();
        let keys = Keymap::load();
        let force_hint = footer_extras(&keys);
        let gone = entries.iter().map(|e| e.missing()).collect();
        let mut app = Self {
            prog,
            entries,
            gone,
            filtered: (0..n).collect(),
            list_state,
            sel: Selection::new(),
            force,
            mode: Mode::Browse,
            filter: String::new(),
            filter_draft: String::new(),
            status: format!(
                "{n} item(s) · {} ",
                keys.browse_footer("restore", &force_hint)
            ),
            quit: false,
            pending_idxs: Vec::new(),
            viewport_rows: 10,
            keys,
            newest_first,
            log: Vec::new(),
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

    /// Reverse the list between newest and oldest first, keeping marks,
    /// the filter and the cursor on the same entries.
    fn toggle_order(&mut self) {
        let n = self.entries.len();
        let prev = self.selected_entry_idx();
        self.entries.reverse();
        self.sel.remap(|i| n - 1 - i);
        self.newest_first = !self.newest_first;
        self.filtered = filter_indices(&self.entries, &self.filter);
        let sel = tui_list::reselect_after_filter(prev.map(|i| n - 1 - i), &self.filtered);
        self.list_state.select(sel);
        self.ensure_scroll();
        self.status = order_label(self.newest_first).into();
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

    fn selected_entry(&self) -> Option<&E> {
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
            let dest_exists = entry.original().symlink_metadata().is_ok();
            if dest_exists && !self.force {
                self.mode = Mode::ConfirmOverwrite;
                self.pending_idxs = vec![ei];
                self.status = format!(
                    "overwrite existing '{}' ?  y confirm · n cancel",
                    entry.original().display()
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
                .any(|&ei| self.entries[ei].original().symlink_metadata().is_ok());
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
        let last_error = result
            .log
            .iter()
            .rev()
            .find(|l| l.is_error)
            .map(|l| l.text.clone());
        self.log.extend(result.log.iter().cloned());
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
        if let Some(e) = last_error {
            self.status = format!("{} · {e}", self.status);
        }
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        match self.mode {
            Mode::Filter => self.on_key_filter(code, mods),
            Mode::Help => self.on_key_help(code, mods),
            Mode::ConfirmOverwrite | Mode::ConfirmBulk => self.on_key_confirm(code, mods),
            Mode::Browse => self.on_key_browse(code, mods),
        }
    }

    fn on_key_help(&mut self, code: KeyCode, mods: KeyModifiers) {
        match self.keys.resolve_help(code, mods) {
            Some(Action::Help | Action::Quit | Action::FilterCancel) => {
                self.mode = Mode::Browse;
                self.status = "help closed".into();
            }
            Some(Action::QuitHard) => self.quit = true,
            _ => {}
        }
    }

    fn on_key_filter(&mut self, code: KeyCode, mods: KeyModifiers) {
        match self.keys.resolve_filter(code, mods) {
            Some(Action::FilterCancel) => {
                let applied = self.filter.clone();
                self.filter_draft = applied.clone();
                self.refilter_query(&applied);
                self.mode = Mode::Browse;
                self.status = tui_keys::status_filter_cancelled().into();
            }
            Some(Action::FilterCommit) => {
                self.filter = self.filter_draft.clone();
                self.mode = Mode::Browse;
                self.status = tui_keys::status_filter_committed(&self.filter, self.filtered.len());
            }
            Some(Action::MoveDown) => self.move_sel(1),
            Some(Action::MoveUp) => self.move_sel(-1),
            Some(Action::QuitHard) => self.quit = true,
            _ => {
                if code == KeyCode::Backspace {
                    self.filter_draft.pop();
                    let d = self.filter_draft.clone();
                    self.refilter_query(&d);
                    self.status = tui_keys::status_filter_live(&d, self.filtered.len());
                } else if let KeyCode::Char(c) = code {
                    if !c.is_control() && !mods.contains(KeyModifiers::CONTROL) {
                        self.filter_draft.push(c);
                        let d = self.filter_draft.clone();
                        self.refilter_query(&d);
                        self.status = tui_keys::status_filter_live(&d, self.filtered.len());
                    }
                }
            }
        }
    }

    fn on_key_confirm(&mut self, code: KeyCode, mods: KeyModifiers) {
        match self.keys.resolve_confirm(code, mods) {
            Some(Action::ConfirmYes) => {
                let idxs = std::mem::take(&mut self.pending_idxs);
                self.perform_restores_tracked(&idxs, true);
            }
            Some(Action::ConfirmNo) => {
                self.pending_idxs.clear();
                self.mode = Mode::Browse;
                self.status = "cancelled".into();
            }
            Some(Action::QuitHard) => self.quit = true,
            _ => {}
        }
    }

    fn on_key_browse(&mut self, code: KeyCode, mods: KeyModifiers) {
        match self.keys.resolve_browse(code, mods) {
            Some(Action::Quit) => self.quit = true,
            Some(Action::QuitHard) => self.quit = true,
            Some(Action::MoveDown) => self.move_sel(1),
            Some(Action::MoveUp) => self.move_sel(-1),
            Some(Action::PageDown) => self.page(true),
            Some(Action::PageUp) => self.page(false),
            Some(Action::First) => {
                if !self.filtered.is_empty() {
                    self.list_state.select(Some(0));
                    self.ensure_scroll();
                }
            }
            Some(Action::Last) => {
                if !self.filtered.is_empty() {
                    self.list_state.select(Some(self.filtered.len() - 1));
                    self.ensure_scroll();
                }
            }
            Some(Action::ToggleMark) => {
                if let Some(ei) = self.selected_entry_idx() {
                    self.sel.toggle(ei);
                    self.status = tui_keys::status_marked(self.sel.len());
                }
            }
            Some(Action::MarkAll) => {
                self.sel.mark_all(self.filtered.iter().copied());
                self.status = tui_keys::status_marked_all(self.sel.len());
            }
            Some(Action::ClearMarks) => {
                self.sel.clear();
                self.status = tui_keys::status_cleared().into();
            }
            Some(Action::OpenFilter) => {
                self.mode = Mode::Filter;
                self.filter_draft = self.filter.clone();
                let d = self.filter_draft.clone();
                self.refilter_query(&d);
                self.status = tui_keys::status_filter_live(&d, self.filtered.len());
            }
            Some(Action::Help) => {
                self.mode = Mode::Help;
                self.status = format!("help · {} close", self.keys.display_chords(Action::Help));
            }
            Some(Action::ToggleForce) => {
                self.force = !self.force;
                self.status = format!("force={}", self.force);
            }
            Some(Action::ToggleOrder) => self.toggle_order(),
            Some(Action::Action) => self.try_restore(),
            _ => {}
        }
    }
}

fn order_label(newest_first: bool) -> &'static str {
    if newest_first {
        "newest first"
    } else {
        "oldest first"
    }
}

fn footer_extras(keys: &Keymap) -> String {
    format!(
        "{} force  {} order",
        keys.display_chords(Action::ToggleForce),
        keys.display_chords(Action::ToggleOrder)
    )
}

fn date_label<E: TuiItem>(e: &E) -> String {
    e.date()
        .map(|d| d.replacen('T', " ", 1))
        .unwrap_or_else(|| "????-??-?? ??:??:??".into())
}

fn ui<E: TuiItem>(f: &mut ratatui::Frame, app: &mut App<'_, E>) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(tui_term::FOOTER_ROWS),
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
        " rtrash restore · {} item(s) · {} marked · {}{} ",
        app.entries.len(),
        app.sel.len(),
        order_label(app.newest_first),
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
            ListItem::new(row_text(mark, &date_label(e), e.original(), app.gone[ei]))
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

    let force_hint = footer_extras(&app.keys);
    let keys = match app.mode {
        Mode::Browse => app.keys.browse_footer("restore", &force_hint),
        Mode::Filter => format!(
            "live fuzzy…  {} commit  {} cancel",
            app.keys.display_chords(Action::FilterCommit),
            app.keys.display_chords(Action::FilterCancel)
        ),
        Mode::Help => format!(
            "{} or {} close help",
            app.keys.display_chords(Action::Help),
            app.keys.display_chords(Action::Quit)
        ),
        Mode::ConfirmOverwrite | Mode::ConfirmBulk => format!(
            "{} confirm  {} cancel",
            app.keys.display_chords(Action::ConfirmYes),
            app.keys.display_chords(Action::ConfirmNo)
        ),
    };
    let help_title = format!("keys · {} help", app.keys.display_chords(Action::Help));
    let footer = Paragraph::new(vec![Line::from(keys), Line::from(app.status.as_str())])
        .block(Block::default().borders(Borders::ALL).title(help_title));
    f.render_widget(footer, chunks[2]);

    if app.mode == Mode::Help {
        draw_help(
            f,
            &app.keys,
            &[
                "toggle_force — force overwrite on restore",
                "toggle_order — newest or oldest first",
            ],
        );
    }

    if matches!(app.mode, Mode::ConfirmOverwrite | Mode::ConfirmBulk) {
        if let Some(e) = app.selected_entry() {
            draw_confirm(f, e.original(), app.pending_idxs.len());
        } else if !app.pending_idxs.is_empty() {
            draw_confirm_bulk(f, app.pending_idxs.len());
        }
    }
}

fn draw_help(f: &mut ratatui::Frame, keys: &Keymap, extras: &[&str]) {
    let area = {
        let r = f.area();
        let v = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(5),
                Constraint::Percentage(90),
                Constraint::Percentage(5),
            ])
            .split(r);
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(8),
                Constraint::Percentage(84),
                Constraint::Percentage(8),
            ])
            .split(v[1])[1]
    };
    f.render_widget(Clear, area);
    let mut lines: Vec<Line> = keys.help_lines().into_iter().map(Line::from).collect();
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
pub fn run<E: TuiItem>(prog: &str, entries: Vec<&E>, force: bool, newest_first: bool) -> i32 {
    if entries.is_empty() {
        eprintln!("{prog}: no trashed files match");
        return 1;
    }
    if entries.len() == 1 {
        return restore_single(prog, entries[0], force);
    }

    let mut terminal = match tui_term::enter() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{prog}: cannot start TUI: {e}");
            return 1;
        }
    };

    let mut app = App::new(prog, entries, force, newest_first);
    let result = (|| -> io::Result<()> {
        loop {
            terminal.draw(|f| ui(f, &mut app))?;
            if app.quit {
                break;
            }
            if let Some((code, mods)) =
                tui_term::next_key(&mut terminal, Duration::from_millis(200))?
            {
                app.on_key(code, mods);
            }
        }
        Ok(())
    })();

    tui_term::leave(&mut terminal);
    // Printing while the TUI owned the screen would have drawn over it.
    // The lines quote paths from the records verbatim.
    for line in &app.log {
        if line.is_error {
            eprintln!("{}", crate::util::err_text(&line.text));
        } else {
            println!("{}", crate::util::term_text(&line.text));
        }
    }
    if let Err(e) = result {
        eprintln!("{prog}: tui error: {e}");
        return 1;
    }
    0
}

/// One line of restore output, held back while the TUI owns the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub is_error: bool,
    pub text: String,
}

/// Result of multi-select restore: which input indices were restored.
#[derive(Debug, Clone)]
pub struct RestoreSelectionResult {
    /// Indices into the input `entries` slice that restored successfully.
    pub succeeded_idxs: Vec<usize>,
    pub fail_count: u32,
    /// `restored '...'` lines and errors, in the order they happened.
    pub log: Vec<LogLine>,
}

impl RestoreSelectionResult {
    pub fn ok_count(&self) -> u32 {
        self.succeeded_idxs.len() as u32
    }
}

/// Restore a set of entry indices via [`TuiItem::restore`], which prints
/// nothing; the outcome lines come back in the result's `log`.
/// `entries` is the full list; `idxs` are indices into it. Restores high→low
/// so callers can remove succeeded indices afterward. Does not mutate `entries`.
pub fn restore_selection<E: TuiItem>(
    prog: &str,
    entries: &[&E],
    idxs: &[usize],
    force: bool,
) -> RestoreSelectionResult {
    let mut sorted: Vec<usize> = idxs.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut succeeded_idxs = Vec::new();
    let mut fail_count = 0u32;
    let mut log = Vec::new();
    for &ei in sorted.iter().rev() {
        if ei >= entries.len() {
            fail_count += 1;
            continue;
        }
        match entries[ei].restore(force) {
            Ok((dest, warnings)) => {
                succeeded_idxs.push(ei);
                for w in warnings {
                    log.push(LogLine {
                        is_error: true,
                        text: format!("{prog}: warning: {w}"),
                    });
                }
                log.push(LogLine {
                    is_error: false,
                    text: format!("restored '{}'", dest.display()),
                });
            }
            Err(e) => {
                fail_count += 1;
                log.push(LogLine {
                    is_error: true,
                    text: format!("{prog}: {e}"),
                });
            }
        }
    }
    succeeded_idxs.sort_unstable();
    RestoreSelectionResult {
        succeeded_idxs,
        fail_count,
        log,
    }
}

/// Restore one item and print the outcome, as a plain restore does.
fn restore_single<E: TuiItem>(prog: &str, e: &E, force: bool) -> i32 {
    match e.restore(force) {
        Ok((dest, warnings)) => {
            for w in &warnings {
                eprintln!("{prog}: warning: {}", crate::util::err_text(w));
            }
            println!("restored '{}'", crate::util::term_path(&dest));
            0
        }
        Err(e) => {
            eprintln!("{prog}: {}", crate::util::err_text(&e));
            1
        }
    }
}

#[cfg(test)]
mod generic_tests {
    //! Platform-free checks: the same browser logic drives Recycle Bin
    //! items on Windows, so these run under Wine too.
    use super::*;

    struct Fake {
        path: PathBuf,
        date: Option<String>,
        fail: bool,
    }

    impl TuiItem for Fake {
        fn original(&self) -> &Path {
            &self.path
        }
        fn date(&self) -> Option<&str> {
            self.date.as_deref()
        }
        fn restore(&self, _force: bool) -> Result<(PathBuf, Vec<String>), String> {
            if self.fail {
                Err(format!("cannot restore '{}'", self.path.display()))
            } else {
                Ok((self.path.clone(), vec![]))
            }
        }
    }

    fn fake(p: &str, fail: bool) -> Fake {
        Fake {
            path: PathBuf::from(p),
            date: Some("2026-10-10T12:00:00".into()),
            fail,
        }
    }

    #[test]
    fn generic_items_filter_and_restore() {
        let items = [fake("alpha.txt", false), fake("beta.txt", true)];
        let refs: Vec<&Fake> = items.iter().collect();
        assert_eq!(filter_indices(&refs, "alp"), vec![0]);
        assert_eq!(date_label(refs[0]), "2026-10-10 12:00:00");
        let r = restore_selection("trash-restore", &refs, &[0, 1], false);
        assert_eq!(r.succeeded_idxs, vec![0]);
        assert_eq!(r.fail_count, 1);
        assert!(r.log.iter().any(|l| l.text == "restored 'alpha.txt'"));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
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
            batch: None,
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

    #[test]
    fn toggle_order_reverses_and_keeps_marks_and_cursor() {
        let (a, b, c) = (entry("/new"), entry("/mid"), entry("/old"));
        let mut app = App::new("restore", vec![&a, &b, &c], false, true);
        app.keys = Keymap::builtin();
        app.sel.toggle(0);
        app.move_sel(1); // cursor on /mid
        app.toggle_order();
        let order: Vec<_> = app.entries.iter().map(|e| e.original.clone()).collect();
        assert_eq!(order, [PathBuf::from("/old"), "/mid".into(), "/new".into()]);
        assert!(!app.newest_first);
        assert_eq!(app.sel.marked_sorted(), vec![2], "the mark follows /new");
        assert_eq!(
            app.selected_entry().unwrap().original,
            PathBuf::from("/mid")
        );
        assert_eq!(app.status, "oldest first");
        app.toggle_order();
        assert!(app.newest_first);
        assert_eq!(app.sel.marked_sorted(), vec![0]);
    }

    #[test]
    fn filter_takes_every_letter() {
        let (a, b) = (entry("/x/jk"), entry("/x/b"));
        let mut app = App::new("rtrash", vec![&a, &b], false, true);
        app.keys = Keymap::builtin();
        app.on_key(KeyCode::Char('/'), KeyModifiers::NONE);
        for c in ['j', 'k', 'J'] {
            let mods = if c.is_uppercase() {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            };
            app.on_key(KeyCode::Char(c), mods);
        }
        assert_eq!(app.filter_draft, "jkJ");
        // Arrows still move, and Enter commits.
        app.on_key(KeyCode::Backspace, KeyModifiers::NONE);
        app.on_key(KeyCode::Down, KeyModifiers::NONE);
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.filter, "jk");
        assert_eq!(app.mode, Mode::Browse);
    }

    #[test]
    fn footer_shows_the_status_line() {
        let (a, b) = (entry("/x/a"), entry("/x/b"));
        let mut app = App::new("rtrash", vec![&a, &b], false, true);
        app.status = "overwrite existing '/x/a' ?".into();
        let text = crate::tui_term::render_text(80, 12, |f| ui(f, &mut app));
        assert!(text.contains("overwrite existing '/x/a' ?"), "{text}");
    }
}
