//! iced application: panes, catalog, tui_binds actions.

use std::collections::BTreeSet;
use std::path::PathBuf;

use iced::event::{self, Event};
use iced::keyboard::{self, Key};
use iced::window;
use iced::{Element, Pixels, Subscription, Task};

use rtrash::tui_binds::{Action, Keymap};

use crate::catalog::{Catalog, Pane};
use crate::cli::HudCli;
use crate::install_desktop;
use crate::keys::{self, Mode};
use crate::prefs;
use crate::summon::{self, SummonAction, SummonServer};
use crate::theme;
use crate::tray::{self, TrayAction};


const HUD_W: f32 = 860.0;
const HUD_H: f32 = 620.0;
const APP_ID: &str = install_desktop::APP_ID;

/// iced messages.
#[derive(Debug, Clone)]
pub enum Message {
    /// Shared TUI action after native mapping.
    Action(Action),
    /// Switch the visible pane.
    SetPane(Pane),
    /// Highlight this catalog row.
    Select(usize),
    /// Toggle the mark on this row.
    ToggleMark(usize),
    /// Filter draft changed.
    FilterChanged(String),
    /// Put path draft changed.
    PutChanged(String),
    /// Catalog scroll window.
    CatalogScroll(icedtea::collection::VisibleWindow),
    /// Native key; mapped through [`map_key`].
    RawKey {
        key: Key,
        modifiers: iced::keyboard::Modifiers,
    },
    /// Window close.
    Close,
    /// Iced assigned a window id.
    Opened(window::Id),
    /// Poll summon / tray / watch.
    Tick,
    /// Discarded click.
    Noop,
}

/// Confirm target after Enter on restore / empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKind {
    Restore,
    Empty,
}

/// iced application state.
pub struct HudApp {
    /// Live catalog.
    pub catalog: Catalog,
    /// Visible pane.
    pub pane: Pane,
    /// Cursor in the filtered list.
    pub selected: usize,
    /// Marked catalog indices (into `catalog.entries`).
    pub marked: BTreeSet<usize>,
    /// Committed filter.
    pub filter: String,
    /// Filter draft while typing.
    pub filter_draft: String,
    /// Put path draft.
    pub put_draft: String,
    /// Restore / put overwrite.
    pub force: bool,
    /// Empty dry-run.
    pub dry_run: bool,
    /// Put recursive dirs.
    pub recursive: bool,
    /// Interaction mode.
    pub mode: Mode,
    /// Pending confirm.
    pub confirm: Option<ConfirmKind>,
    /// Footer / banner.
    pub status: String,
    /// Shared keymap.
    pub keys: Keymap,
    /// Pins used to reload.
    pins: Vec<PathBuf>,
    home_only: bool,
    /// Catalog virtual window.
    pub catalog_window: icedtea::collection::VisibleWindow,
    /// Row heights for `virtual_column` (must outlive the view).
    pub list_heights: Vec<f32>,
    /// Overlay currently mapped.
    pub visible: bool,
    /// Decorated window (env RTRASH_HUD_WINDOW).
    pub window_mode: bool,
    /// Last iced window id.
    pub window_id: Option<window::Id>,
    /// Pending xdg-activation token.
    pub pending_token: Option<String>,
    /// Last refresh banner clock.
    pub last_refresh: String,
    _summon: Option<SummonServer>,
    _tray: Option<tray::HudTray>,
    watch: Option<crate::watch::Watch>,
}

impl HudApp {
    /// Load the catalog for these flags.
    pub fn boot(cli: HudCli) -> Self {
        let catalog = Catalog::load(&cli.trash_dirs, cli.home_only);
        let n = catalog.entries.len();
        let watch_roots: Vec<PathBuf> = cli
            .trash_dirs
            .iter()
            .cloned()
            .chain(catalog.dirs.iter().map(|d| d.root.clone()))
            .collect();
        let mut app = HudApp {
            catalog,
            pane: Pane::List,
            selected: 0,
            marked: BTreeSet::new(),
            filter: String::new(),
            filter_draft: String::new(),
            put_draft: String::new(),
            force: false,
            dry_run: false,
            recursive: false,
            mode: Mode::Browse,
            confirm: None,
            status: format!("{n} items"),
            keys: Keymap::load(),
            pins: cli.trash_dirs,
            home_only: cli.home_only,
            catalog_window: icedtea::collection::VisibleWindow::new(480.0),
            list_heights: Vec::new(),
            visible: cli.visible || prefs::window_mode(),
            window_mode: prefs::window_mode(),
            window_id: None,
            pending_token: summon::take_env_token(),
            last_refresh: "now".into(),
            _summon: summon::install().ok(),
            _tray: tray::install().ok(),
            watch: Some(crate::watch::install(&watch_roots)),
        };
        app.sync_heights();
        app
    }

    fn sync_heights(&mut self) {
        self.list_heights = self.visible().iter().map(|_| 36.0).collect();
    }

    /// Filtered catalog indices.
    pub fn visible(&self) -> Vec<usize> {
        let q = if self.mode == Mode::Filter {
            self.filter_draft.as_str()
        } else {
            self.filter.as_str()
        };
        let q = q.to_ascii_lowercase();
        self.catalog
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                if q.is_empty() {
                    return true;
                }
                e.path_label().to_ascii_lowercase().contains(&q)
                    || e.name.to_ascii_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn selected_index(&self) -> Option<usize> {
        let vis = self.visible();
        vis.get(self.selected).copied().or_else(|| vis.last().copied())
    }

    fn targets(&self) -> Vec<usize> {
        if !self.marked.is_empty() {
            self.marked.iter().copied().collect()
        } else if let Some(i) = self.selected_index() {
            vec![i]
        } else {
            Vec::new()
        }
    }

    fn reload(&mut self) {
        self.catalog = Catalog::load(&self.pins, self.home_only);
        let vis = self.visible();
        if self.selected >= vis.len() {
            self.selected = vis.len().saturating_sub(1);
        }
        self.marked
            .retain(|&i| i < self.catalog.entries.len());
        self.status = format!("{} items", self.catalog.entries.len());
        self.sync_heights();
    }

    fn apply(&mut self, action: Action) {
        match self.mode {
            Mode::Confirm => self.apply_confirm(action),
            Mode::Filter => self.apply_filter(action),
            Mode::Help => {
                if matches!(
                    action,
                    Action::Help | Action::Quit | Action::FilterCancel | Action::QuitHard
                ) {
                    self.mode = Mode::Browse;
                }
            }
            Mode::PutInput => {}
            Mode::Browse => self.apply_browse(action),
        }
    }

    fn apply_browse(&mut self, action: Action) {
        let vis_len = self.visible().len();
        match action {
            Action::Quit | Action::QuitHard => {
                // iced::exit is returned from update, not here.
                self.status = "quit".into();
            }
            Action::MoveDown => {
                if vis_len > 0 {
                    self.selected = (self.selected + 1).min(vis_len - 1);
                }
            }
            Action::MoveUp => {
                self.selected = self.selected.saturating_sub(1);
            }
            Action::PageDown => {
                if vis_len > 0 {
                    self.selected = (self.selected + 16).min(vis_len - 1);
                }
            }
            Action::PageUp => {
                self.selected = self.selected.saturating_sub(16);
            }
            Action::First => self.selected = 0,
            Action::Last => {
                if vis_len > 0 {
                    self.selected = vis_len - 1;
                }
            }
            Action::ToggleMark => {
                if let Some(i) = self.selected_index() {
                    if !self.marked.remove(&i) {
                        self.marked.insert(i);
                    }
                }
            }
            Action::MarkAll => {
                self.marked = self.visible().into_iter().collect();
            }
            Action::ClearMarks => self.marked.clear(),
            Action::OpenFilter => {
                self.mode = Mode::Filter;
                self.filter_draft = self.filter.clone();
            }
            Action::Help => self.mode = Mode::Help,
            Action::ToggleForce => {
                self.force = !self.force;
                self.status = format!("force={}", self.force);
            }
            Action::ToggleDryRun => {
                self.dry_run = !self.dry_run;
                self.status = format!("dry_run={}", self.dry_run);
            }
            Action::ToggleRecursive => {
                self.recursive = !self.recursive;
                self.status = format!("recursive={}", self.recursive);
            }
            Action::Action => self.begin_action(),
            Action::ConfirmYes | Action::ConfirmNo | Action::FilterCommit | Action::FilterCancel => {
            }
        }
    }

    fn apply_filter(&mut self, action: Action) {
        match action {
            Action::FilterCommit | Action::Action => {
                self.filter = self.filter_draft.clone();
                self.mode = Mode::Browse;
                self.selected = 0;
                self.sync_heights();
            }
            Action::FilterCancel | Action::Quit | Action::QuitHard => {
                self.filter_draft.clear();
                self.mode = Mode::Browse;
            }
            Action::MoveDown => self.apply_browse(Action::MoveDown),
            Action::MoveUp => self.apply_browse(Action::MoveUp),
            _ => {}
        }
    }

    fn apply_confirm(&mut self, action: Action) {
        match action {
            Action::ConfirmYes | Action::Action => {
                let kind = self.confirm.take();
                self.mode = Mode::Browse;
                match kind {
                    Some(ConfirmKind::Restore) => self.do_restore(),
                    Some(ConfirmKind::Empty) => self.do_empty(),
                    None => {}
                }
            }
            Action::ConfirmNo | Action::Quit | Action::QuitHard | Action::FilterCancel => {
                self.confirm = None;
                self.mode = Mode::Browse;
                self.status = "cancelled".into();
            }
            _ => {}
        }
    }

    fn begin_action(&mut self) {
        match self.pane {
            Pane::List => {
                self.pane = Pane::Restore;
                self.status = "restore pane".into();
            }
            Pane::Restore => {
                if self.targets().is_empty() {
                    self.status = "nothing to restore".into();
                    return;
                }
                self.confirm = Some(ConfirmKind::Restore);
                self.mode = Mode::Confirm;
                self.status = "restore? y/n".into();
            }
            Pane::Empty => {
                if self.targets().is_empty() {
                    self.status = "nothing to empty".into();
                    return;
                }
                self.confirm = Some(ConfirmKind::Empty);
                self.mode = Mode::Confirm;
                self.status = "empty? y/n".into();
            }
            Pane::Put => self.do_put(),
            Pane::Status => self.reload(),
        }
    }

    fn do_restore(&mut self) {
        let targets = self.targets();
        let n = targets.len();
        let rc = self.catalog.restore(&targets, self.force);
        self.status = if rc == 0 {
            format!("restored {n}")
        } else {
            format!("restore failed ({rc})")
        };
        self.marked.clear();
        self.reload();
    }

    fn do_empty(&mut self) {
        let targets = self.targets();
        let result = self.catalog.empty(&targets, self.dry_run);
        self.status = if self.dry_run {
            format!(
                "would remove {} ({})",
                result.ok_count(),
                rtrash::fastdelete::format_bytes(result.reclaim_bytes)
            )
        } else {
            format!("removed {}", result.ok_count())
        };
        self.marked.clear();
        if !self.dry_run {
            self.reload();
        }
    }

    fn do_put(&mut self) {
        let raw = self.put_draft.trim();
        if raw.is_empty() {
            self.status = "put: empty path".into();
            return;
        }
        let path = PathBuf::from(raw);
        match self.catalog.put(&path, self.force, self.recursive) {
            Ok(()) => {
                self.status = format!("put {}", path.display());
                self.put_draft.clear();
                self.reload();
            }
            Err(rc) => self.status = format!("put failed ({rc})"),
        }
    }

    fn wants_exit(&self, action: Action) -> bool {
        self.mode == Mode::Browse && matches!(action, Action::Quit | Action::QuitHard)
    }
}

/// Decorated desktop window. Overlay / summon is a sibling ticket.
pub fn hud_window() -> window::Settings {
    let app_id = if prefs::window_mode() {
        install_desktop::APP_ID
    } else {
        install_desktop::OVERLAY_APP_ID
    };
    let boot = icedtea::app::Boot::new("rtrash", app_id)
        .size(HUD_W, HUD_H)
        .min_size(360.0, 420.0);
    icedtea::app::bootstrap(&boot).window
}

/// Run the iced loop (iced 0.14 daemon boot, same shape as vissue-hud).
pub fn run(cli: HudCli) -> iced::Result {
    icedtea::typo::install_platform_faces();
    let cell = std::sync::Mutex::new(Some(cli));
    iced::daemon(
        move || {
            let cli = cell.lock().expect("boot").take().expect("boot once");
            let mut app = HudApp::boot(cli);
            let task = if app.visible {
                let (id, open) = window::open(hud_window());
                app.window_id = Some(id);
                open.map(Message::Opened)
            } else {
                Task::none()
            };
            (app, task)
        },
        update,
        view,
    )
    .subscription(subscription)
    .theme(|_: &HudApp, _| theme::theme())
    .title(|_: &HudApp, _| "rtrash".to_string())
    .default_font(icedtea::typo::UI)
    .settings(iced::Settings {
        default_text_size: Pixels::from(icedtea::typo::BODY),
        default_font: icedtea::typo::UI,
        ..Default::default()
    })
    .run()
}

fn update(app: &mut HudApp, message: Message) -> Task<Message> {
    match message {
        Message::Opened(id) => {
            app.window_id = Some(id);
            Task::none()
        }
        Message::Tick => tick(app),
        Message::Close => {
            if app.window_mode {
                iced::exit()
            } else {
                hide_overlay(app)
            }
        }
        Message::Noop => Task::none(),
        Message::SetPane(pane) => {
            app.pane = pane;
            if pane == Pane::Put {
                app.mode = Mode::PutInput;
            } else if app.mode == Mode::PutInput {
                app.mode = Mode::Browse;
            }
            Task::none()
        }
        Message::Select(i) => {
            if let Some(pos) = app.visible().iter().position(|&x| x == i) {
                app.selected = pos;
            }
            Task::none()
        }
        Message::ToggleMark(i) => {
            if !app.marked.remove(&i) {
                app.marked.insert(i);
            }
            Task::none()
        }
        Message::FilterChanged(q) => {
            app.filter_draft = q;
            app.mode = Mode::Filter;
            app.selected = 0;
            app.sync_heights();
            Task::none()
        }
        Message::PutChanged(q) => {
            app.put_draft = q;
            app.mode = Mode::PutInput;
            Task::none()
        }
        Message::CatalogScroll(w) => {
            app.catalog_window = w;
            Task::none()
        }
        Message::RawKey { key, modifiers } => {
            if let Some(mapped) = map_key(app, &key, modifiers) {
                return update(app, mapped);
            }
            Task::none()
        }
        Message::Action(action) => {
            if app.wants_exit(action) {
                return iced::exit();
            }
            if action == Action::Action && app.mode == Mode::PutInput {
                app.do_put();
                return Task::none();
            }
            app.apply(action);
            Task::none()
        }
    }
}

fn view(app: &HudApp, _id: window::Id) -> Element<'_, Message> {
    crate::view::view(app)
}

fn subscription(_app: &HudApp) -> Subscription<Message> {
    let keys = event::listen_with(|event, _status, _id| match event {
        Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
            Some(Message::RawKey { key, modifiers })
        }
        Event::Window(window::Event::CloseRequested) => Some(Message::Close),
        _ => None,
    });
    let tick = iced::time::every(std::time::Duration::from_millis(200)).map(|_| Message::Tick);
    Subscription::batch([keys, tick])
}

fn tick(app: &mut HudApp) -> Task<Message> {
    if let Some(req) = summon::try_recv() {
        app.pending_token = req.token;
        return apply_summon(app, req.action);
    }
    if let Some(tray) = tray::try_recv() {
        return match tray {
            TrayAction::Show => apply_summon(app, SummonAction::Show),
            TrayAction::Toggle => apply_summon(app, SummonAction::Toggle),
            TrayAction::Quit => iced::exit(),
        };
    }
    if app.watch.as_ref().is_some_and(|w| w.take()) {
        app.reload();
        app.last_refresh = "live".into();
    }
    Task::none()
}

fn apply_summon(app: &mut HudApp, action: SummonAction) -> Task<Message> {
    let show = match action {
        SummonAction::Show => true,
        SummonAction::Hide => false,
        SummonAction::Toggle => !app.visible,
    };
    if show {
        show_overlay(app)
    } else {
        hide_overlay(app)
    }
}

fn show_overlay(app: &mut HudApp) -> Task<Message> {
    app.visible = true;
    if app.window_id.is_some() {
        return Task::none();
    }
    let (id, open) = window::open(hud_window());
    app.window_id = Some(id);
    open.map(Message::Opened)
}

fn hide_overlay(app: &mut HudApp) -> Task<Message> {
    app.visible = false;
    if let Some(id) = app.window_id.take() {
        window::close(id)
    } else {
        Task::none()
    }
}

/// Key subscription uses the live keymap so view stays dumb.
pub fn map_key(app: &HudApp, key: &Key, modifiers: iced::keyboard::Modifiers) -> Option<Message> {
    if app.mode != Mode::Filter && app.mode != Mode::PutInput {
        if let Some(pane) = keys::pane_digit(key, modifiers) {
            return Some(Message::SetPane(pane));
        }
    }
    keys::resolve(&app.keys, key, modifiers, app.mode).map(Message::Action)
}
