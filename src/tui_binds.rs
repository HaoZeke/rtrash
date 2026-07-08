//! Configurable TUI keybinds for restore / empty / put browsers.
//!
//! Defaults match the shipped keymap. Override any action in
//! `$XDG_CONFIG_HOME/rtrash/keys.toml` (or `$HOME/.config/rtrash/keys.toml`),
//! or point `RTRASH_KEYS` at a file. TOML format:
//!
//! ```toml
//! [keys]
//! move_down = ["down", "j"]
//! quit = ["q", "esc"]
//! toggle_mark = "space"          # string or array
//! help = []                      # clear (unbind)
//! ```
//!
//! Unlisted actions keep defaults. Empty array clears that action's defaults
//! (leave at least quit bound).

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyModifiers};
use serde::Deserialize;

/// Bindable roles shared across browsers (mode filters which apply).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Quit,
    /// Ctrl-c style hard quit (still overridable).
    QuitHard,
    MoveUp,
    MoveDown,
    PageUp,
    PageDown,
    First,
    Last,
    ToggleMark,
    MarkAll,
    ClearMarks,
    OpenFilter,
    Help,
    /// Primary Enter action (restore / purge / put) in browse mode.
    Action,
    ConfirmYes,
    ConfirmNo,
    FilterCommit,
    FilterCancel,
    /// Restore + put: toggle force.
    ToggleForce,
    /// Empty: toggle dry-run.
    ToggleDryRun,
    /// Put: toggle recursive dirs.
    ToggleRecursive,
}

impl Action {
    pub const ALL: &'static [Action] = &[
        Action::Quit,
        Action::QuitHard,
        Action::MoveUp,
        Action::MoveDown,
        Action::PageUp,
        Action::PageDown,
        Action::First,
        Action::Last,
        Action::ToggleMark,
        Action::MarkAll,
        Action::ClearMarks,
        Action::OpenFilter,
        Action::Help,
        Action::Action,
        Action::ConfirmYes,
        Action::ConfirmNo,
        Action::FilterCommit,
        Action::FilterCancel,
        Action::ToggleForce,
        Action::ToggleDryRun,
        Action::ToggleRecursive,
    ];

    pub const BROWSE: &'static [Action] = &[
        Action::Quit,
        Action::QuitHard,
        Action::MoveUp,
        Action::MoveDown,
        Action::PageUp,
        Action::PageDown,
        Action::First,
        Action::Last,
        Action::ToggleMark,
        Action::MarkAll,
        Action::ClearMarks,
        Action::OpenFilter,
        Action::Help,
        Action::Action,
        Action::ToggleForce,
        Action::ToggleDryRun,
        Action::ToggleRecursive,
    ];

    pub const FILTER: &'static [Action] = &[
        Action::FilterCommit,
        Action::FilterCancel,
        Action::MoveUp,
        Action::MoveDown,
        Action::QuitHard,
    ];

    pub const CONFIRM: &'static [Action] = &[
        Action::ConfirmYes,
        Action::ConfirmNo,
        Action::QuitHard,
    ];

    pub const HELP: &'static [Action] =
        &[Action::Help, Action::Quit, Action::FilterCancel, Action::QuitHard];

    pub fn name(self) -> &'static str {
        match self {
            Action::Quit => "quit",
            Action::QuitHard => "quit_hard",
            Action::MoveUp => "move_up",
            Action::MoveDown => "move_down",
            Action::PageUp => "page_up",
            Action::PageDown => "page_down",
            Action::First => "first",
            Action::Last => "last",
            Action::ToggleMark => "toggle_mark",
            Action::MarkAll => "mark_all",
            Action::ClearMarks => "clear_marks",
            Action::OpenFilter => "open_filter",
            Action::Help => "help",
            Action::Action => "action",
            Action::ConfirmYes => "confirm_yes",
            Action::ConfirmNo => "confirm_no",
            Action::FilterCommit => "filter_commit",
            Action::FilterCancel => "filter_cancel",
            Action::ToggleForce => "toggle_force",
            Action::ToggleDryRun => "toggle_dry_run",
            Action::ToggleRecursive => "toggle_recursive",
        }
    }

    pub fn from_name(s: &str) -> Option<Action> {
        Action::ALL.iter().copied().find(|a| a.name() == s)
    }

    pub fn doc(self) -> &'static str {
        match self {
            Action::Quit => "quit browser (browse mode)",
            Action::QuitHard => "hard quit (e.g. ctrl-c)",
            Action::MoveUp => "move selection up",
            Action::MoveDown => "move selection down",
            Action::PageUp => "page up by viewport",
            Action::PageDown => "page down by viewport",
            Action::First => "jump to first item",
            Action::Last => "jump to last item",
            Action::ToggleMark => "toggle mark on cursor",
            Action::MarkAll => "mark all visible",
            Action::ClearMarks => "clear all marks",
            Action::OpenFilter => "open live fuzzy filter",
            Action::Help => "toggle help overlay",
            Action::Action => "primary action (restore/purge/put)",
            Action::ConfirmYes => "confirm bulk / overwrite",
            Action::ConfirmNo => "cancel confirm",
            Action::FilterCommit => "commit filter draft",
            Action::FilterCancel => "cancel filter (restore prior)",
            Action::ToggleForce => "toggle force (restore/put)",
            Action::ToggleDryRun => "toggle dry-run (empty)",
            Action::ToggleRecursive => "toggle recursive (put)",
        }
    }
}

/// One key chord (optional modifiers + key).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Chord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub key: ChordKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ChordKey {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Enter,
    Esc,
    Tab,
    Backspace,
    Delete,
    F(u8),
}

impl Chord {
    pub fn char(c: char) -> Self {
        Self {
            ctrl: false,
            alt: false,
            shift: false,
            key: ChordKey::Char(c),
        }
    }

    pub fn display(&self) -> String {
        let mut s = String::new();
        if self.ctrl {
            s.push_str("ctrl-");
        }
        if self.alt {
            s.push_str("alt-");
        }
        if self.shift {
            s.push_str("shift-");
        }
        s.push_str(&self.key.display());
        s
    }

    /// Parse a single chord token (`j`, `space`, `ctrl-c`, `pgup`, `G`, …).
    pub fn parse(token: &str) -> Result<Self, String> {
        let t = token.trim();
        if t.is_empty() {
            return Err("empty key".into());
        }
        // Preserve original single-letter case before lowercasing prefixes.
        if t.chars().count() == 1 {
            let c = t.chars().next().unwrap();
            return Ok(Self::char(c));
        }
        let lower = t.to_ascii_lowercase();
        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        let mut rest = lower.as_str();
        loop {
            if let Some(r) = rest.strip_prefix("ctrl-") {
                ctrl = true;
                rest = r;
                continue;
            }
            if let Some(r) = rest.strip_prefix("control-") {
                ctrl = true;
                rest = r;
                continue;
            }
            if let Some(r) = rest.strip_prefix("c-") {
                ctrl = true;
                rest = r;
                continue;
            }
            if let Some(r) = rest.strip_prefix("^") {
                ctrl = true;
                rest = r;
                continue;
            }
            if let Some(r) = rest.strip_prefix("alt-") {
                alt = true;
                rest = r;
                continue;
            }
            if let Some(r) = rest.strip_prefix("a-") {
                alt = true;
                rest = r;
                continue;
            }
            if let Some(r) = rest.strip_prefix("shift-") {
                shift = true;
                rest = r;
                continue;
            }
            if let Some(r) = rest.strip_prefix("s-") {
                shift = true;
                rest = r;
                continue;
            }
            break;
        }
        if rest.is_empty() {
            return Err(format!("incomplete key: {token}"));
        }
        let key = parse_key_name(rest, t)?;
        Ok(Self {
            ctrl,
            alt,
            shift,
            key,
        })
    }

    pub fn matches(&self, code: KeyCode, mods: KeyModifiers) -> bool {
        let has_ctrl = mods.contains(KeyModifiers::CONTROL);
        let has_alt = mods.contains(KeyModifiers::ALT);
        if self.ctrl != has_ctrl || self.alt != has_alt {
            return false;
        }
        match (&self.key, code) {
            (ChordKey::Char(c), KeyCode::Char(k)) => *c == k,
            (ChordKey::Up, KeyCode::Up) => true,
            (ChordKey::Down, KeyCode::Down) => true,
            (ChordKey::Left, KeyCode::Left) => true,
            (ChordKey::Right, KeyCode::Right) => true,
            (ChordKey::PageUp, KeyCode::PageUp) => true,
            (ChordKey::PageDown, KeyCode::PageDown) => true,
            (ChordKey::Home, KeyCode::Home) => true,
            (ChordKey::End, KeyCode::End) => true,
            (ChordKey::Enter, KeyCode::Enter) => true,
            (ChordKey::Esc, KeyCode::Esc) => true,
            (ChordKey::Tab, KeyCode::Tab) => true,
            (ChordKey::Backspace, KeyCode::Backspace) => true,
            (ChordKey::Delete, KeyCode::Delete) => true,
            (ChordKey::F(n), KeyCode::F(m)) => *n == m,
            _ => false,
        }
    }
}

impl ChordKey {
    fn display(&self) -> String {
        match self {
            ChordKey::Char(' ') => "space".into(),
            ChordKey::Char(c) => c.to_string(),
            ChordKey::Up => "up".into(),
            ChordKey::Down => "down".into(),
            ChordKey::Left => "left".into(),
            ChordKey::Right => "right".into(),
            ChordKey::PageUp => "pgup".into(),
            ChordKey::PageDown => "pgdn".into(),
            ChordKey::Home => "home".into(),
            ChordKey::End => "end".into(),
            ChordKey::Enter => "enter".into(),
            ChordKey::Esc => "esc".into(),
            ChordKey::Tab => "tab".into(),
            ChordKey::Backspace => "backspace".into(),
            ChordKey::Delete => "delete".into(),
            ChordKey::F(n) => format!("f{n}"),
        }
    }
}

fn parse_key_name(rest: &str, original: &str) -> Result<ChordKey, String> {
    match rest {
        "space" | "spc" => Ok(ChordKey::Char(' ')),
        "up" => Ok(ChordKey::Up),
        "down" => Ok(ChordKey::Down),
        "left" => Ok(ChordKey::Left),
        "right" => Ok(ChordKey::Right),
        "pgup" | "pageup" | "page-up" => Ok(ChordKey::PageUp),
        "pgdn" | "pagedown" | "page-down" | "pgdown" => Ok(ChordKey::PageDown),
        "home" => Ok(ChordKey::Home),
        "end" => Ok(ChordKey::End),
        "enter" | "return" | "ret" => Ok(ChordKey::Enter),
        "esc" | "escape" => Ok(ChordKey::Esc),
        "tab" => Ok(ChordKey::Tab),
        "backspace" | "bs" => Ok(ChordKey::Backspace),
        "delete" | "del" => Ok(ChordKey::Delete),
        s if s.starts_with('f') && s.len() <= 3 => {
            let n: u8 = s[1..]
                .parse()
                .map_err(|_| format!("bad function key: {original}"))?;
            if (1..=12).contains(&n) {
                Ok(ChordKey::F(n))
            } else {
                Err(format!("f-key out of range: {original}"))
            }
        }
        s if s.chars().count() == 1 => Ok(ChordKey::Char(s.chars().next().unwrap())),
        _ => Err(format!("unknown key: {original}")),
    }
}

/// Full keymap: action → list of chords (any match triggers).
#[derive(Debug, Clone)]
pub struct Keymap {
    binds: HashMap<Action, Vec<Chord>>,
    /// Source path if loaded from disk (for diagnostics).
    pub source: Option<PathBuf>,
}

impl Default for Keymap {
    fn default() -> Self {
        Self::builtin()
    }
}

impl Keymap {
    pub fn builtin() -> Self {
        let mut binds = HashMap::new();
        let set = |m: &mut HashMap<Action, Vec<Chord>>, a: Action, keys: &[&str]| {
            let chords: Vec<Chord> = keys
                .iter()
                .map(|k| Chord::parse(k).expect("builtin key"))
                .collect();
            m.insert(a, chords);
        };
        set(&mut binds, Action::Quit, &["q", "esc"]);
        set(&mut binds, Action::QuitHard, &["ctrl-c"]);
        set(&mut binds, Action::MoveDown, &["down", "j"]);
        set(&mut binds, Action::MoveUp, &["up", "k"]);
        set(&mut binds, Action::PageDown, &["pagedown"]);
        set(&mut binds, Action::PageUp, &["pageup"]);
        set(&mut binds, Action::First, &["home", "g"]);
        set(&mut binds, Action::Last, &["end", "G"]);
        set(&mut binds, Action::ToggleMark, &["space"]);
        set(&mut binds, Action::MarkAll, &["a"]);
        set(&mut binds, Action::ClearMarks, &["A"]);
        set(&mut binds, Action::OpenFilter, &["/"]);
        set(&mut binds, Action::Help, &["?"]);
        set(&mut binds, Action::Action, &["enter"]);
        set(&mut binds, Action::ConfirmYes, &["y", "Y", "enter"]);
        set(&mut binds, Action::ConfirmNo, &["n", "N", "esc"]);
        set(&mut binds, Action::FilterCommit, &["enter"]);
        set(&mut binds, Action::FilterCancel, &["esc"]);
        set(&mut binds, Action::ToggleForce, &["f"]);
        set(&mut binds, Action::ToggleDryRun, &["n"]);
        set(&mut binds, Action::ToggleRecursive, &["r"]);
        Self {
            binds,
            source: None,
        }
    }

    /// Resolve within an allowed action set (mode-scoped — avoids Enter/Esc clashes).
    pub fn resolve_among(
        &self,
        code: KeyCode,
        mods: KeyModifiers,
        allowed: &[Action],
    ) -> Option<Action> {
        for &a in allowed {
            if let Some(chords) = self.binds.get(&a) {
                if chords.iter().any(|c| c.matches(code, mods)) {
                    return Some(a);
                }
            }
        }
        None
    }

    pub fn resolve_browse(&self, code: KeyCode, mods: KeyModifiers) -> Option<Action> {
        self.resolve_among(code, mods, Action::BROWSE)
    }

    pub fn resolve_filter(&self, code: KeyCode, mods: KeyModifiers) -> Option<Action> {
        self.resolve_among(code, mods, Action::FILTER)
    }

    pub fn resolve_confirm(&self, code: KeyCode, mods: KeyModifiers) -> Option<Action> {
        self.resolve_among(code, mods, Action::CONFIRM)
    }

    pub fn resolve_help(&self, code: KeyCode, mods: KeyModifiers) -> Option<Action> {
        self.resolve_among(code, mods, Action::HELP)
    }

    pub fn chords(&self, action: Action) -> &[Chord] {
        self.binds.get(&action).map(|v| v.as_slice()).unwrap_or(&[])
    }

    pub fn display_chords(&self, action: Action) -> String {
        let c = self.chords(action);
        if c.is_empty() {
            return "∅".into();
        }
        c.iter()
            .map(|ch| ch.display())
            .collect::<Vec<_>>()
            .join("/")
    }

    /// Apply overrides from a TOML config body (partial merge onto self).
    pub fn apply_config_text(&mut self, text: &str) -> Result<Vec<String>, String> {
        let file: KeysFile = toml::from_str(text).map_err(|e| format!("toml: {e}"))?;
        let mut notes = Vec::new();
        for (name, spec) in file.keys {
            let Some(action) = Action::from_name(&name) else {
                return Err(format!(
                    "unknown action `{name}` under [keys] (see rtrash keys --list)"
                ));
            };
            let tokens = spec.into_tokens();
            if tokens.is_empty() {
                self.binds.insert(action, Vec::new());
                notes.push(format!("cleared binds for {}", action.name()));
                continue;
            }
            let mut chords = Vec::with_capacity(tokens.len());
            for tok in &tokens {
                chords.push(
                    Chord::parse(tok).map_err(|e| format!("key `{tok}` for `{name}`: {e}"))?,
                );
            }
            self.binds.insert(action, chords);
        }
        Ok(notes)
    }

    pub fn load_file(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut map = Self::builtin();
        map.apply_config_text(&text)?;
        map.source = Some(path.to_path_buf());
        Ok(map)
    }

    /// Load from `RTRASH_KEYS`, else default config path if it exists, else builtin.
    pub fn load() -> Self {
        if let Ok(p) = std::env::var("RTRASH_KEYS") {
            let path = PathBuf::from(p);
            match Self::load_file(&path) {
                Ok(m) => return m,
                Err(e) => eprintln!("rtrash: keys config: {e} (using defaults)"),
            }
        }
        if let Some(path) = default_keys_path() {
            if path.is_file() {
                match Self::load_file(&path) {
                    Ok(m) => return m,
                    Err(e) => eprintln!("rtrash: keys config: {e} (using defaults)"),
                }
            }
        }
        Self::builtin()
    }

    /// Default sample TOML body (comments + all actions).
    pub fn sample_config() -> String {
        let mut out = String::from(
            "# rtrash TUI keybinds — copy to $XDG_CONFIG_HOME/rtrash/keys.toml\n\
             # Override any action under [keys]; unlisted actions keep defaults.\n\
             # Each value is a string or array of key names:\n\
             #   j, space, enter, esc, up, down, pgup, pgdn, home, end,\n\
             #   ctrl-c, alt-x, f1..f12\n\
             # Clear an action:  help = []\n\n\
             [keys]\n",
        );
        let builtin = Self::builtin();
        for &a in Action::ALL {
            let keys: Vec<String> = builtin.chords(a).iter().map(|c| c.display()).collect();
            let arr = keys
                .iter()
                .map(|k| format!("\"{k}\""))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!("# {}\n{} = [{arr}]\n", a.doc(), a.name()));
        }
        out
    }

    pub fn format_table(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!(
            "source: {}",
            self.source
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(built-in defaults)".into())
        ));
        for &a in Action::ALL {
            lines.push(format!(
                "  {:<18} {:<24}  # {}",
                a.name(),
                self.display_chords(a),
                a.doc()
            ));
        }
        lines.join("\n")
    }

    /// Compact browse footer from current binds.
    pub fn browse_footer(&self, action_label: &str, extras: &str) -> String {
        let base = format!(
            "{}/{}/{}/{}  {} mark  {}/{} all/none  {} fuzzy  {} help  {} quit  {} {action_label}",
            self.display_chords(Action::MoveUp),
            self.display_chords(Action::MoveDown),
            self.display_chords(Action::PageUp),
            self.display_chords(Action::PageDown),
            self.display_chords(Action::ToggleMark),
            self.display_chords(Action::MarkAll),
            self.display_chords(Action::ClearMarks),
            self.display_chords(Action::OpenFilter),
            self.display_chords(Action::Help),
            self.display_chords(Action::Quit),
            self.display_chords(Action::Action),
        );
        if extras.is_empty() {
            base
        } else {
            format!("{base}  {extras}")
        }
    }

    pub fn help_lines(&self) -> Vec<String> {
        let mut lines = vec![
            "Keybinds (fully customizable TOML)".into(),
            format!(
                "  config: {}",
                default_keys_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "~/.config/rtrash/keys.toml".into())
            ),
            "  dump:   rtrash keys --sample > ~/.config/rtrash/keys.toml".into(),
            "  list:   rtrash keys --list".into(),
        ];
        if let Some(ref s) = self.source {
            lines.push(format!("  loaded: {}", s.display()));
        }
        lines.push(String::new());
        for &a in Action::ALL {
            lines.push(format!(
                "  {:<16} {:<20} {}",
                a.name(),
                self.display_chords(a),
                a.doc()
            ));
        }
        lines
    }
}

impl fmt::Display for Keymap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.format_table())
    }
}

/// Deserialized `keys.toml` root.
#[derive(Debug, Default, Deserialize)]
struct KeysFile {
    #[serde(default)]
    keys: BTreeMap<String, KeySpec>,
}

/// One action value: `"j"`, `["down", "j"]`, or `[]` to clear.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum KeySpec {
    One(String),
    Many(Vec<String>),
}

impl KeySpec {
    fn into_tokens(self) -> Vec<String> {
        match self {
            KeySpec::One(s) => {
                // Allow a single string with spaces as multiple keys for convenience.
                s.split_whitespace().map(|t| t.to_string()).collect()
            }
            KeySpec::Many(v) => v,
        }
    }
}

/// `$XDG_CONFIG_HOME/rtrash/keys.toml` or `~/.config/rtrash/keys.toml`.
pub fn default_keys_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("rtrash/keys.toml"));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/rtrash/keys.toml"))
}

/// CLI: `rtrash keys [--list|--sample|--path]`.
pub fn run_cli(prog: &str, args: &[String]) -> i32 {
    let mut sample = false;
    let mut path_only = false;
    for a in args {
        match a.as_str() {
            "--list" | "-l" => {}
            "--sample" => sample = true,
            "--path" => path_only = true,
            "--help" | "-h" => {
                print!(
                    "\
Usage: {prog} keys [OPTION]
Show or generate TUI keybind configuration (TOML).

  --list, -l   print resolved keymap (default)
  --sample     print a full sample keys.toml to stdout
  --path       print the default config path
  --help       this help

Config file: $XDG_CONFIG_HOME/rtrash/keys.toml
             (fallback: ~/.config/rtrash/keys.toml)
Override:    RTRASH_KEYS=/path/to/keys.toml

Example:

  [keys]
  move_down = [\"down\", \"j\"]
  toggle_mark = \"space\"
  help = []                 # unbind

Every bindable action can be remapped; unlisted actions keep defaults.
Modes use separate action sets so the same key can mean confirm_yes in
confirm mode and action in browse mode (e.g. enter).
"
                );
                return 0;
            }
            other => {
                eprintln!("{prog}: unrecognized option '{other}'");
                return 2;
            }
        }
    }
    if path_only {
        match default_keys_path() {
            Some(p) => println!("{}", p.display()),
            None => {
                eprintln!("{prog}: cannot resolve config path (set HOME or XDG_CONFIG_HOME)");
                return 1;
            }
        }
        return 0;
    }
    if sample {
        print!("{}", Keymap::sample_config());
        return 0;
    }
    let map = Keymap::load();
    println!("{map}");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_chords() {
        assert_eq!(Chord::parse("j").unwrap(), Chord::char('j'));
        assert_eq!(Chord::parse("G").unwrap().key, ChordKey::Char('G'));
        assert_eq!(Chord::parse("space").unwrap().key, ChordKey::Char(' '));
        assert_eq!(Chord::parse("pgup").unwrap().key, ChordKey::PageUp);
        let c = Chord::parse("ctrl-c").unwrap();
        assert!(c.ctrl && c.key == ChordKey::Char('c'));
        assert!(Chord::parse("f5").unwrap().key == ChordKey::F(5));
    }

    #[test]
    fn mode_scoped_enter() {
        let m = Keymap::builtin();
        assert_eq!(
            m.resolve_browse(KeyCode::Enter, KeyModifiers::NONE),
            Some(Action::Action)
        );
        assert_eq!(
            m.resolve_confirm(KeyCode::Enter, KeyModifiers::NONE),
            Some(Action::ConfirmYes)
        );
        assert_eq!(
            m.resolve_filter(KeyCode::Enter, KeyModifiers::NONE),
            Some(Action::FilterCommit)
        );
        assert_eq!(
            m.resolve_filter(KeyCode::Esc, KeyModifiers::NONE),
            Some(Action::FilterCancel)
        );
    }

    #[test]
    fn builtin_resolve_defaults() {
        let m = Keymap::builtin();
        assert_eq!(
            m.resolve_browse(KeyCode::Char('j'), KeyModifiers::NONE),
            Some(Action::MoveDown)
        );
        assert_eq!(
            m.resolve_browse(KeyCode::Char(' '), KeyModifiers::NONE),
            Some(Action::ToggleMark)
        );
        assert_eq!(
            m.resolve_browse(KeyCode::Char('?'), KeyModifiers::NONE),
            Some(Action::Help)
        );
        assert_eq!(
            m.resolve_browse(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(Action::QuitHard)
        );
    }

    #[test]
    fn partial_override_merges() {
        let mut m = Keymap::builtin();
        m.apply_config_text(
            r#"
[keys]
move_down = ["down"]
toggle_mark = "x"
"#,
        )
        .unwrap();
        assert_eq!(
            m.resolve_browse(KeyCode::Char('j'), KeyModifiers::NONE),
            None
        );
        assert_eq!(
            m.resolve_browse(KeyCode::Down, KeyModifiers::NONE),
            Some(Action::MoveDown)
        );
        assert_eq!(
            m.resolve_browse(KeyCode::Char('x'), KeyModifiers::NONE),
            Some(Action::ToggleMark)
        );
        assert_eq!(
            m.resolve_browse(KeyCode::Char('q'), KeyModifiers::NONE),
            Some(Action::Quit)
        );
    }

    #[test]
    fn clear_action() {
        let mut m = Keymap::builtin();
        m.apply_config_text("[keys]\nhelp = []\n").unwrap();
        assert!(m.chords(Action::Help).is_empty());
        assert_eq!(
            m.resolve_browse(KeyCode::Char('?'), KeyModifiers::NONE),
            None
        );
    }

    #[test]
    fn unknown_action_errors() {
        let mut m = Keymap::builtin();
        assert!(m
            .apply_config_text("[keys]\nnope = \"x\"\n")
            .is_err());
    }

    #[test]
    fn sample_roundtrip_parses() {
        let sample = Keymap::sample_config();
        let mut m = Keymap::builtin();
        m.apply_config_text(&sample).unwrap();
        assert_eq!(
            m.resolve_browse(KeyCode::Char('j'), KeyModifiers::NONE),
            Some(Action::MoveDown)
        );
    }

    #[test]
    fn string_or_array_values() {
        let mut m = Keymap::builtin();
        m.apply_config_text(
            r#"
[keys]
move_up = "k"
move_down = ["down", "j", "J"]
"#,
        )
        .unwrap();
        assert_eq!(
            m.resolve_browse(KeyCode::Char('J'), KeyModifiers::NONE),
            Some(Action::MoveDown)
        );
        assert_eq!(
            m.resolve_browse(KeyCode::Char('k'), KeyModifiers::NONE),
            Some(Action::MoveUp)
        );
    }

    #[test]
    fn load_file_from_disk() {
        let dir = std::env::temp_dir().join(format!(
            "rtrash-keys-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("keys.toml");
        fs::write(
            &path,
            r#"
[keys]
move_down = ["J"]
quit = ["Q"]
"#,
        )
        .unwrap();
        let m = Keymap::load_file(&path).unwrap();
        assert_eq!(
            m.resolve_browse(KeyCode::Char('J'), KeyModifiers::NONE),
            Some(Action::MoveDown)
        );
        assert_eq!(
            m.resolve_browse(KeyCode::Char('j'), KeyModifiers::NONE),
            None
        );
        assert_eq!(
            m.resolve_browse(KeyCode::Char('Q'), KeyModifiers::NONE),
            Some(Action::Quit)
        );
        // defaults still for mark
        assert_eq!(
            m.resolve_browse(KeyCode::Char(' '), KeyModifiers::NONE),
            Some(Action::ToggleMark)
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
