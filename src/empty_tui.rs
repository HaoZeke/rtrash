//! Interactive empty browser: multi-select permanent delete with confirm.

use std::io;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::list::Entry;
use crate::trashdir;
use crate::tui_binds::{Action, Keymap};
use crate::tui_fuzzy;
use crate::tui_keys;
use crate::tui_list;
use crate::tui_select::Selection;
use crate::tui_term;

/// Fuzzy-rank cached original-path haystacks. An empty query keeps
/// original order and allocates no path `String`.
pub fn filter_indices(hays: &[&str], query: &str) -> Vec<usize> {
    tui_fuzzy::rank_indices(hays, query)
}

fn hay_of(entry: &Entry) -> String {
    entry.original.to_string_lossy().into_owned()
}

/// Rank `hays` without cloning a path per row. An empty query skips the
/// pointer list as well.
fn rank_hays(hays: &[String], query: &str) -> Vec<usize> {
    if query.trim().is_empty() {
        return (0..hays.len()).collect();
    }
    let refs: Vec<&str> = hays.iter().map(String::as_str).collect();
    filter_indices(&refs, query)
}

/// Result of a permanent-remove batch: per-input success flags + dry-run bytes.
#[derive(Debug, Clone)]
pub struct RemoveBatchResult {
    /// `true` iff the entry at the same index in the input slice was removed
    /// (or counted in dry-run).
    pub succeeded: Vec<bool>,
    pub reclaim_bytes: u64,
    /// Why each entry that stayed did, one `prog: ...` line apiece in input
    /// order. Nothing is printed while the browser owns the terminal: it
    /// shows the last line and prints them all once it has closed.
    pub reasons: Vec<String>,
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
/// Does not reimplement wipe logic: uses `permanent_guard::remove` + info unlink.
pub fn permanently_remove_entries(
    prog: &str,
    entries: &[&Entry],
    dry_run: bool,
) -> RemoveBatchResult {
    let mut succeeded = Vec::with_capacity(entries.len());
    let mut bytes = 0u64;
    let mut reasons = Vec::new();
    let keep = match crate::keep::Keep::load() {
        Ok(keep) => keep,
        Err(e) => {
            let reason = format!("{prog}: cannot read keep file: {e}; refusing permanent deletion");
            return RemoveBatchResult {
                succeeded: vec![false; entries.len()],
                reclaim_bytes: 0,
                reasons: vec![reason],
            };
        }
    };
    for entry in entries {
        if keep.protects(&entry.original) {
            reasons.push(format!(
                "{prog}: kept '{}' (listed in the keep file)",
                crate::util::err_path(&entry.original)
            ));
            succeeded.push(false);
            continue;
        }
        if dry_run {
            bytes = bytes.saturating_add(trashdir::entry_reclaim_bytes(&entry.dir, &entry.name));
            succeeded.push(true);
            continue;
        }
        // The browser read the trash when it opened. Under the trash lock,
        // an item that left it since, and whose name a put gave to another
        // item, stays; one already gone counts as removed.
        let held = match entry.relock() {
            Ok(held) => held,
            Err(crate::list::Relock::Gone) => {
                succeeded.push(true);
                continue;
            }
            Err(why) => {
                reasons.push(format!("{prog}: {}", why.reason(entry)));
                succeeded.push(false);
                continue;
            }
        };
        let (payload, info_path) = (held.payload(), held.record());
        // A total current before this removal stays current after it: the
        // item's size (from the directorysizes cache where valid) and its
        // one count come off.
        let carry = crate::trash_total::cached(&entry.dir).map(|(b, n)| {
            let sz = trashdir::entry_reclaim_bytes(&entry.dir, &entry.name);
            (b.saturating_sub(sz), n.saturating_sub(1))
        });
        crate::trash_total::forget(&entry.dir);
        crate::trash_total::touched(&entry.dir);
        if let Err(e) = crate::permanent_guard::remove(&payload) {
            reasons.push(format!(
                "{prog}: cannot remove '{}': {e}",
                payload.display()
            ));
            succeeded.push(false);
            continue;
        }
        // The payload is gone: its record goes too before a Ctrl-C acts.
        let _section = crate::interrupt::critical();
        if let Err(e) = std::fs::remove_file(&info_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                reasons.push(format!(
                    "{prog}: cannot remove '{}': {e}",
                    info_path.display()
                ));
                succeeded.push(false);
                continue;
            }
        }
        trashdir::directorysizes_remove(&entry.dir, &entry.name);
        if let Some((b, n)) = carry {
            crate::trash_total::store(&entry.dir, b, n);
        }
        succeeded.push(true);
    }
    RemoveBatchResult {
        succeeded,
        reclaim_bytes: bytes,
        reasons,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Browse,
    Filter,
    Help,
    Confirm,
}

struct App<'a> {
    prog: &'a str,
    entries: Vec<&'a Entry>,
    /// Cached original-path display strings; one per `entries` row.
    hays: Vec<String>,
    /// Per entry, whether its payload is gone (one stat each, at start).
    gone: Vec<bool>,
    filtered: Vec<usize>,
    list_state: ListState,
    sel: Selection,
    mode: Mode,
    filter: String,
    filter_draft: String,
    status: String,
    quit: bool,
    dry_run: bool,
    viewport_rows: usize,
    keys: Keymap,
    /// Why entries stayed, as [`RemoveBatchResult::reasons`] gave them;
    /// printed on stderr once the browser has closed.
    said: Vec<String>,
}

impl<'a> App<'a> {
    fn new(prog: &'a str, entries: Vec<&'a Entry>, dry_run: bool) -> Self {
        let mut list_state = ListState::default();
        if !entries.is_empty() {
            list_state.select(Some(0));
        }
        let n = entries.len();
        let keys = Keymap::load();
        let dry_hint = format!("{} dry-run", keys.display_chords(Action::ToggleDryRun));
        let hays = entries.iter().copied().map(hay_of).collect();
        let gone = entries.iter().map(|e| e.missing()).collect();
        let mut app = Self {
            prog,
            entries,
            hays,
            gone,
            filtered: (0..n).collect(),
            list_state,
            sel: Selection::new(),
            mode: Mode::Browse,
            filter: String::new(),
            filter_draft: String::new(),
            status: format!("{n} item(s) · {} ", keys.browse_footer("purge", &dry_hint)),
            quit: false,
            dry_run,
            viewport_rows: 10,
            keys,
            said: Vec::new(),
        };
        app.refilter_query(&app.filter.clone());
        app
    }

    fn refilter(&mut self) {
        let q = self.filter.clone();
        self.refilter_query(&q);
    }

    /// Drop one row and the cached path and gone mark that belong to it.
    fn remove_at(&mut self, ei: usize) {
        debug_assert_eq!(self.hays.len(), self.entries.len());
        debug_assert_eq!(self.gone.len(), self.entries.len());
        self.entries.remove(ei);
        self.hays.remove(ei);
        self.gone.remove(ei);
    }

    fn refilter_query(&mut self, query: &str) {
        debug_assert_eq!(self.hays.len(), self.entries.len());
        debug_assert_eq!(self.gone.len(), self.entries.len());
        let prev = self.selected_entry_idx();
        self.filtered = rank_hays(&self.hays, query);
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
            self.status = "nothing marked · Space to mark or a for all visible".into();
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
                    self.remove_at(ei);
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
        if let Some(last) = result.reasons.last() {
            self.status = format!("{} · {}", self.status, crate::util::term_text(last));
        }
        self.said.extend(result.reasons);
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
            Mode::Filter => match self.keys.resolve_filter(code, mods) {
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
                    self.status =
                        tui_keys::status_filter_committed(&self.filter, self.filtered.len());
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
            },
            Mode::Confirm => match self.keys.resolve_confirm(code, mods) {
                Some(Action::ConfirmYes) => self.do_purge(),
                Some(Action::ConfirmNo) => {
                    self.mode = Mode::Browse;
                    self.status = "purge cancelled".into();
                }
                Some(Action::QuitHard) => self.quit = true,
                _ => {}
            },
            Mode::Browse => match self.keys.resolve_browse(code, mods) {
                Some(Action::Quit | Action::QuitHard) => self.quit = true,
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
                    self.status =
                        format!("help · {} close", self.keys.display_chords(Action::Help));
                }
                Some(Action::ToggleDryRun) => {
                    self.dry_run = !self.dry_run;
                    self.status = format!("dry_run={}", self.dry_run);
                }
                Some(Action::Action) => self.try_purge(),
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
            Constraint::Length(tui_term::FOOTER_ROWS),
        ])
        .split(f.area());
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
    let (start, end) = tui_list::viewport_range(
        app.list_state.offset(),
        app.filtered.len(),
        app.viewport_rows,
    );
    let items: Vec<ListItem> = app.filtered[start..end]
        .iter()
        .map(|&ei| {
            let e = app.entries[ei];
            let mark = if app.sel.is_marked(ei) { "[x]" } else { "[ ]" };
            let gone = if app.gone[ei] {
                crate::restore_tui::GONE_MARK
            } else {
                ""
            };
            ListItem::new(format!(
                "{mark} {}  {}{gone}",
                e.date.as_deref().unwrap_or("?"),
                app.hays[ei]
            ))
        })
        .collect();
    let mut view_state = ListState::default();
    view_state.select(tui_list::viewport_selected(
        app.list_state.selected(),
        start,
        end,
    ));
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
        &mut view_state,
    );
    let dry_hint = format!("{} dry-run", app.keys.display_chords(Action::ToggleDryRun));
    let keys = match app.mode {
        Mode::Browse => app.keys.browse_footer("purge", &dry_hint),
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
        Mode::Confirm => format!(
            "{} confirm  {} cancel",
            app.keys.display_chords(Action::ConfirmYes),
            app.keys.display_chords(Action::ConfirmNo)
        ),
    };
    let help_title = format!("keys · {} help", app.keys.display_chords(Action::Help));
    f.render_widget(
        Paragraph::new(vec![Line::from(keys), Line::from(app.status.as_str())])
            .block(Block::default().borders(Borders::ALL).title(help_title)),
        chunks[2],
    );
    if app.mode == Mode::Help {
        draw_help_overlay(
            f,
            &app.keys,
            &["toggle_dry_run — report only, no permanent delete"],
        );
    }
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

fn draw_help_overlay(f: &mut ratatui::Frame, keys: &Keymap, extras: &[&str]) {
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
            if let Some((code, mods)) =
                tui_term::next_key(&mut terminal, Duration::from_millis(200))?
            {
                app.on_key(code, mods);
            }
        }
        Ok(())
    })();
    tui_term::leave(&mut terminal);
    // Printing while the browser owned the screen would have drawn over it.
    for line in &app.said {
        eprintln!("{}", crate::util::err_text(line));
    }
    if let Err(e) = result {
        eprintln!("{prog}: tui error: {e}");
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trashdir::TrashDir;
    use std::path::PathBuf;

    #[test]
    fn removing_entries_carries_the_running_total() {
        let root = std::env::temp_dir().join(format!("rtrash-tui-total-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("info")).unwrap();
        std::fs::create_dir_all(root.join("files")).unwrap();
        std::fs::write(root.join("files/x"), "x").unwrap();
        std::fs::write(root.join("info/x.trashinfo"), "[Trash Info]\nPath=/x\n").unwrap();
        let dir = TrashDir::from_root(&root).unwrap();
        crate::trash_total::store(&dir, 999_999, 1);
        assert!(root.join("rtrash-total").exists());
        let sz = trashdir::entry_reclaim_bytes(&dir, "x");
        let e = Entry {
            date: None,
            epoch: 0,
            original: PathBuf::from("/x"),
            name: "x".into(),
            dir: dir.clone(),
            batch: None,
            recorded: true,
            payload: None,
        };
        let r = permanently_remove_entries("rtrash", &[&e], false);
        assert_eq!(r.succeeded, vec![true]);
        assert!(!root.join("files/x").exists());
        assert_eq!(
            crate::trash_total::cached(&dir),
            Some((999_999 - sz, 0)),
            "the total did not follow the removal"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn footer_shows_the_status_line() {
        let e = Entry {
            date: None,
            epoch: 0,
            original: PathBuf::from("/x/a"),
            name: "a".into(),
            dir: TrashDir {
                root: PathBuf::from("/tmp/trash"),
                topdir: None,
            },
            batch: None,
            recorded: true,
            payload: None,
        };
        let mut app = App::new("rtrash", vec![&e], false);
        app.status = "purge 1 item?".into();
        let text = crate::tui_term::render_text(80, 12, |f| ui(f, &mut app));
        assert!(text.contains("purge 1 item?"), "{text}");
    }

    /// The browser read the trash when it opened: an item that took the
    /// name of one marked since then stays (round 244).
    #[test]
    fn removing_entries_spares_a_reused_name() {
        let root = std::env::temp_dir().join(format!("rtrash-tui-retaken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("info")).unwrap();
        std::fs::create_dir_all(root.join("files")).unwrap();
        let record = root.join("info/x.trashinfo");
        let body = |batch: &str| format!("[Trash Info]\nPath=/x\nX-Rtrash-Batch={batch}\n");
        std::fs::write(root.join("files/x"), "old").unwrap();
        std::fs::write(&record, body("1-1")).unwrap();
        let dir = TrashDir::from_root(&root).unwrap();
        let seen = crate::list::collect(std::slice::from_ref(&dir));
        assert_eq!(seen.len(), 1);
        std::fs::write(root.join("files/x"), "new").unwrap();
        std::fs::write(&record, body("2-2")).unwrap();
        let r = permanently_remove_entries("rtrash", &[&seen[0]], false);
        assert_eq!(r.succeeded, vec![false]);
        assert_eq!(
            std::fs::read_to_string(root.join("files/x")).unwrap(),
            "new"
        );
        assert!(record.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The browser holds stderr while it owns the terminal, so a purge
    /// that leaves an entry says why in the status line, and the reason
    /// waits to be printed once the browser closes.
    #[test]
    fn a_purge_that_leaves_an_entry_says_why_in_the_status_line() {
        let root = std::env::temp_dir().join(format!("rtrash-tui-why-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("info")).unwrap();
        std::fs::create_dir_all(root.join("files")).unwrap();
        let record = root.join("info/x.trashinfo");
        let body = |batch: &str| format!("[Trash Info]\nPath=/x\nX-Rtrash-Batch={batch}\n");
        std::fs::write(root.join("files/x"), "old").unwrap();
        std::fs::write(&record, body("1-1")).unwrap();
        let dir = TrashDir::from_root(&root).unwrap();
        let seen = crate::list::collect(std::slice::from_ref(&dir));
        // Another item takes the name while the browser is open.
        std::fs::write(root.join("files/x"), "new").unwrap();
        std::fs::write(&record, body("2-2")).unwrap();
        let mut app = App::new("rtrash", vec![&seen[0]], false);
        app.do_purge();
        assert!(
            app.status
                .starts_with("removed 0 · fail 1 · 1 left · rtrash: 'x' changed in the trash"),
            "{}",
            app.status
        );
        assert_eq!(app.said.len(), 1);
        assert!(record.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_query_ranks_cached_haystacks_in_order() {
        let owned: Vec<String> = (0..5000).map(|i| format!("/tmp/file-{i}.txt")).collect();
        let all = rank_hays(&owned, "  ");
        assert_eq!(all.len(), 5000);
        assert_eq!((all[0], all[4999]), (0, 4999));
        let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
        let ranked = filter_indices(&refs, "file-1");
        assert!(!ranked.is_empty());
        assert!(
            ranked
                .iter()
                .all(|&i| tui_fuzzy::fuzzy_score(refs[i], "file-1").is_some()),
            "a keystroke ranks the cached haystacks"
        );
    }

    #[test]
    fn a_scrolled_list_draws_only_its_viewport() {
        let entries: Vec<Entry> = (0..30)
            .map(|i| Entry {
                date: None,
                epoch: 0,
                original: PathBuf::from(format!("/tmp/row-{i:02}")),
                name: "n".into(),
                dir: TrashDir {
                    root: PathBuf::from("/tmp/trash"),
                    topdir: None,
                },
                batch: None,
                recorded: true,
                payload: None,
            })
            .collect();
        let refs: Vec<&Entry> = entries.iter().collect();
        let mut app = App::new("rtrash", refs, false);
        app.list_state.select(Some(app.filtered.len() - 1));
        let text = crate::tui_term::render_text(80, 12, |f| ui(f, &mut app));
        assert!(text.contains("row-29"), "{text}");
        assert!(text.contains("row-27"), "{text}");
        assert!(!text.contains("row-00"), "{text}");
        assert_eq!(app.list_state.selected(), Some(29));
        let selected = text.lines().find(|l| l.contains("row-29")).unwrap();
        assert!(selected.contains('▸'), "{selected}");
    }

    /// A purge that removes one row drops that row's cached path too.
    #[test]
    fn a_purge_drops_the_cached_path_with_its_row() {
        let root = std::env::temp_dir().join(format!("rtrash-tui-hay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("info")).unwrap();
        std::fs::create_dir_all(root.join("files")).unwrap();
        std::fs::write(root.join("files/a"), "a").unwrap();
        std::fs::write(root.join("files/b"), "b").unwrap();
        std::fs::write(root.join("info/a.trashinfo"), "[Trash Info]\nPath=/left\n").unwrap();
        std::fs::write(root.join("info/b.trashinfo"), "[Trash Info]\nPath=/right\n").unwrap();
        let dir = TrashDir::from_root(&root).unwrap();
        let left = Entry {
            date: None,
            epoch: 0,
            original: PathBuf::from("/left"),
            name: "a".into(),
            dir: dir.clone(),
            batch: None,
            recorded: true,
            payload: None,
        };
        let right = Entry {
            date: None,
            epoch: 0,
            original: PathBuf::from("/right"),
            name: "b".into(),
            dir: dir.clone(),
            batch: None,
            recorded: true,
            payload: None,
        };
        let mut app = App::new("rtrash", vec![&left, &right], false);
        assert_eq!(app.hays, ["/left", "/right"]);
        app.do_purge();
        assert_eq!(app.entries.len(), 1);
        assert_eq!(app.hays, ["/right"]);
        assert_eq!(app.gone.len(), 1);
        assert!(root.join("files/b").exists());
        assert!(!root.join("files/a").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
