//! Interactive put browser: multi-select paths in a directory and trash them.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::put;
use crate::tui_fuzzy;
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
}

impl App {
    fn new(prog: &str, cwd: PathBuf, rows: Vec<PathRow>) -> Self {
        let mut list_state = ListState::default();
        if !rows.is_empty() {
            list_state.select(Some(0));
        }
        let n = rows.len();
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
            status: format!("{n} entries · Space mark · Enter put · / fuzzy · q quit"),
            quit: false,
            recursive: true,
            force: false,
        };
        app.refilter();
        app
    }

    fn refilter(&mut self) {
        let prev = self.selected_idx();
        self.filtered = filter_indices(&self.rows, &self.filter);
        let new_sel = prev
            .and_then(|ei| self.filtered.iter().position(|&i| i == ei))
            .or(if self.filtered.is_empty() {
                None
            } else {
                Some(0)
            });
        self.list_state.select(new_sel);
    }

    fn selected_idx(&self) -> Option<usize> {
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
            Mode::Filter => match code {
                KeyCode::Esc => self.mode = Mode::Browse,
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
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.do_put(),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.mode = Mode::Browse;
                    self.status = "put cancelled".into();
                }
                _ => {}
            },
            Mode::Browse => match code {
                KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
                KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => self.quit = true,
                KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
                KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
                KeyCode::Char(' ') => {
                    if let Some(ei) = self.selected_idx() {
                        self.sel.toggle(ei);
                        self.status = format!("marked {}", self.sel.len());
                    }
                }
                KeyCode::Char('a') => {
                    self.sel.mark_all(self.filtered.iter().copied());
                    self.status = format!("marked all ({})", self.sel.len());
                }
                KeyCode::Char('A') => {
                    self.sel.clear();
                    self.status = "cleared".into();
                }
                KeyCode::Char('/') => {
                    self.mode = Mode::Filter;
                    self.filter_draft = self.filter.clone();
                }
                KeyCode::Char('r') => {
                    self.recursive = !self.recursive;
                    self.status = format!("recursive={}", self.recursive);
                }
                KeyCode::Char('f') => {
                    self.force = !self.force;
                    self.status = format!("force={}", self.force);
                }
                KeyCode::Enter => self.try_put(),
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
    let filter_show = if app.mode == Mode::Filter {
        format!("/{}", app.filter_draft)
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
    let keys = match app.mode {
        Mode::Browse => "Space mark  a/A  / fuzzy  r recursive  f force  Enter put  q quit",
        Mode::Filter => "fuzzy… Enter apply Esc cancel",
        Mode::Confirm => "y trash selection  n cancel",
    };
    f.render_widget(
        Paragraph::new(vec![Line::from(keys), Line::from(app.status.as_str())])
            .block(Block::default().borders(Borders::ALL).title("keys")),
        chunks[2],
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
