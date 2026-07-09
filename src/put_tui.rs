//! Interactive put browser: multi-select paths in a directory and trash them.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::put;
use crate::tui_binds::{Action, Keymap};
use crate::tui_fuzzy;
use crate::tui_keys;
use crate::tui_list;
use crate::tui_select::Selection;
use crate::tui_term;

#[derive(Clone)]
pub struct PathRow {
    pub path: PathBuf,
    pub is_dir: bool,
}

/// List directory entries (non-hidden first), for TUI and tests.
pub fn list_dir_rows(dir: &Path) -> io::Result<Vec<PathRow>> {
    let mut rows = Vec::new();
    for ent in fs::read_dir(dir)? {
        let ent = ent?;
        let name = ent.file_name();
        let name_s = name.to_string_lossy();
        if name_s == "." || name_s == ".." {
            continue;
        }
        let meta = ent.metadata().ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        rows.push(PathRow {
            path: ent.path(),
            is_dir,
        });
    }
    rows.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.path.file_name().cmp(&b.path.file_name()))
    });
    Ok(rows)
}

pub fn filter_indices(rows: &[PathRow], query: &str) -> Vec<usize> {
    let hays: Vec<String> = rows
        .iter()
        .map(|r| {
            r.path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let refs: Vec<&str> = hays.iter().map(String::as_str).collect();
    tui_fuzzy::rank_indices(&refs, query)
}

/// Put selected paths using default recursive-for-dirs opts. Returns exit code.
pub fn put_selection(prog: &str, paths: &[PathBuf], recursive: bool, force: bool) -> i32 {
    let opts = put::Opts {
        recursive,
        force,
        ..put::Opts::default()
    };
    let mut status = 0;
    for p in paths {
        let mut o = opts.clone();
        if p.is_dir() && !o.recursive {
            o.recursive = true; // TUI put dirs as trees by default when marked
        }
        if let Err(code) = put::put_one(prog, p, &o) {
            status = status.max(code);
        }
    }
    status
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Browse,
    Filter,
    Help,
    Confirm,
}

struct App {
    prog: String,
    cwd: PathBuf,
    rows: Vec<PathRow>,
    filtered: Vec<usize>,
    list_state: ListState,
    sel: Selection,
    mode: Mode,
    filter: String,
    filter_draft: String,
    status: String,
    quit: bool,
    recursive: bool,
    force: bool,
    viewport_rows: usize,
    keys: Keymap,
}

impl App {
    fn new(prog: &str, cwd: PathBuf, rows: Vec<PathRow>) -> Self {
        let mut list_state = ListState::default();
        if !rows.is_empty() {
            list_state.select(Some(0));
        }
        let n = rows.len();
        let keys = Keymap::load();
        let extra = format!(
            "{} recursive · {} force",
            keys.display_chords(Action::ToggleRecursive),
            keys.display_chords(Action::ToggleForce)
        );
        let mut app = Self {
            prog: prog.to_string(),
            cwd,
            rows,
            filtered: (0..n).collect(),
            list_state,
            sel: Selection::new(),
            mode: Mode::Browse,
            filter: String::new(),
            filter_draft: String::new(),
            status: format!("{n} entries · {} ", keys.browse_footer("put", &extra)),
            quit: false,
            recursive: true,
            force: false,
            viewport_rows: 10,
            keys,
        };
        app.refilter_query(&app.filter.clone());
        app
    }

    fn refilter(&mut self) {
        let q = self.filter.clone();
        self.refilter_query(&q);
    }

    fn refilter_query(&mut self, query: &str) {
        let prev = self.selected_idx();
        self.filtered = filter_indices(&self.rows, query);
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

    fn selected_idx(&self) -> Option<usize> {
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

    fn targets(&self) -> Vec<PathBuf> {
        let marked = self.sel.marked_visible(&self.filtered);
        let idxs = if !marked.is_empty() {
            marked
        } else if let Some(ei) = self.selected_idx() {
            vec![ei]
        } else {
            return Vec::new();
        };
        idxs.into_iter()
            .map(|i| self.rows[i].path.clone())
            .collect()
    }

    fn try_put(&mut self) {
        let t = self.targets();
        if t.is_empty() {
            self.status = "nothing selected".into();
            return;
        }
        self.mode = Mode::Confirm;
        self.status = format!("trash {} path(s)?  y confirm · n cancel", t.len());
    }

    fn do_put(&mut self) {
        let paths = self.targets();
        let code = put_selection(&self.prog, &paths, self.recursive, self.force);
        // Refresh directory listing
        if let Ok(rows) = list_dir_rows(&self.cwd) {
            self.rows = rows;
            self.sel.clear();
            self.refilter();
        }
        self.mode = Mode::Browse;
        self.status = if code == 0 {
            format!("put ok · {} paths left in view", self.rows.len())
        } else {
            format!("put exit {code} · {} paths left", self.rows.len())
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
                Some(Action::ConfirmYes) => self.do_put(),
                Some(Action::ConfirmNo) => {
                    self.mode = Mode::Browse;
                    self.status = "put cancelled".into();
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
                    if let Some(ei) = self.selected_idx() {
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
                Some(Action::ToggleRecursive) => {
                    self.recursive = !self.recursive;
                    self.status = format!("recursive={}", self.recursive);
                }
                Some(Action::ToggleForce) => {
                    self.force = !self.force;
                    self.status = format!("force={}", self.force);
                }
                Some(Action::Action) => self.try_put(),
                _ => {}
            },
        }
    }
}

fn ui(f: &mut ratatui::Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
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
        " rtrash put · {} · {} marked · rec={} force={}{} ",
        app.cwd.display(),
        app.sel.len(),
        app.recursive,
        app.force,
        filter_show
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        ))
        .block(Block::default().borders(Borders::ALL).title("put")),
        chunks[0],
    );
    let items: Vec<ListItem> = app
        .filtered
        .iter()
        .map(|&ei| {
            let r = &app.rows[ei];
            let mark = if app.sel.is_marked(ei) { "[x]" } else { "[ ]" };
            let kind = if r.is_dir { "/" } else { " " };
            let name = r.path.file_name().unwrap_or_default().to_string_lossy();
            ListItem::new(format!("{mark} {name}{kind}"))
        })
        .collect();
    f.render_stateful_widget(
        List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" paths "))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD))
            .highlight_symbol("▸ "),
        chunks[1],
        &mut app.list_state,
    );
    let extra = format!(
        "{} recursive · {} force",
        app.keys.display_chords(Action::ToggleRecursive),
        app.keys.display_chords(Action::ToggleForce)
    );
    let keys = match app.mode {
        Mode::Browse => app.keys.browse_footer("put", &extra),
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
            &["toggle_recursive · toggle_force — put options"],
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

pub fn run(prog: &str, start_dir: &Path) -> i32 {
    let cwd = start_dir
        .canonicalize()
        .unwrap_or_else(|_| start_dir.to_path_buf());
    let rows = match list_dir_rows(&cwd) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{prog}: cannot read '{}': {e}", cwd.display());
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
    let mut app = App::new(prog, cwd, rows);
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
