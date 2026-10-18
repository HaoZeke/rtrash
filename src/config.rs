//! The config file: `$XDG_CONFIG_HOME/rtrash/config.toml` (or
//! `~/.config/rtrash/config.toml`; `%APPDATA%\rtrash\config.toml` on
//! Windows when `XDG_CONFIG_HOME` is unset). `RTRASH_CONFIG` names
//! another file, and an empty `RTRASH_CONFIG` turns the file off.
//!
//! Settings rank below environment variables and command-line options.
//! The file is a small TOML subset, enough for flat tables: `[section]`
//! headers and `key = value` lines, where a value is a string (basic or
//! literal), an integer, a boolean, or an array of strings that may span
//! lines. Anything else, unknown keys included, earns a warning and is
//! skipped; the rest of the file still applies.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// One parsed value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Str(String),
    Int(i64),
    Bool(bool),
    List(Vec<String>),
}

/// What a known key holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Str,
    Int,
    Bool,
    List,
}

/// Every key the file may set: (section, key, kind, what it does).
const KEYS: &[(&str, &str, Kind, &str)] = &[
    (
        "macos",
        "trash",
        Kind::Str,
        "finder (macOS default) or freedesktop: the trash plain put, list, restore and empty use",
    ),
    (
        "wsl",
        "recycle",
        Kind::Bool,
        "on WSL, put files under /mnt/<drive>/ into the Windows Recycle Bin",
    ),
    ("du", "top", Kind::Int, "rows per list (--top)"),
    (
        "du",
        "sort",
        Kind::Str,
        "disk, apparent, items or mtime (--sort)",
    ),
    ("du", "count", Kind::Bool, "add the ITEMS column (--count)"),
    (
        "du",
        "one_file_system",
        Kind::Bool,
        "stay on one file system (-x)",
    ),
    ("du", "threads", Kind::Int, "walker threads (--threads)"),
    (
        "du",
        "git",
        Kind::Bool,
        "ask git about rows (false = --no-git)",
    ),
    (
        "du",
        "exclude",
        Kind::List,
        "patterns to leave out (--exclude)",
    ),
    (
        "du",
        "exclude_regex",
        Kind::List,
        "path regexes to leave out (--exclude-regex)",
    ),
    (
        "du",
        "include_regex",
        Kind::List,
        "path regexes the files must match (--include-regex)",
    ),
    (
        "du",
        "exclude_caches",
        Kind::Bool,
        "leave out tagged caches (--exclude-caches)",
    ),
    (
        "du",
        "gitignore",
        Kind::Bool,
        "leave out what git ignores (--gitignore)",
    ),
    (
        "du",
        "no_delete",
        Kind::Bool,
        "open -i without delete (--no-delete)",
    ),
    ("du", "si", Kind::Bool, "sizes in powers of 1000 (--si)"),
    (
        "list",
        "sort",
        Kind::Str,
        "date, size, name or path (--sort)",
    ),
    ("list", "size", Kind::Bool, "show sizes (--size)"),
    (
        "list",
        "reverse",
        Kind::Bool,
        "reverse the order (--reverse)",
    ),
    (
        "policy",
        "max_size",
        Kind::Str,
        "trash budget after put (RTRASH_MAX_SIZE)",
    ),
    (
        "policy",
        "warn_size",
        Kind::Str,
        "trash size, or percent of its disk, a put warns past (RTRASH_WARN_SIZE)",
    ),
    (
        "policy",
        "min_free",
        Kind::Str,
        "bytes a copy across devices must leave free (RTRASH_MIN_FREE)",
    ),
    (
        "policy",
        "in_use",
        Kind::Str,
        "warn, refuse or ignore when put meets an item in use (RTRASH_IN_USE)",
    ),
    (
        "policy",
        "prune",
        Kind::Str,
        "oldest or largest (RTRASH_PRUNE)",
    ),
    (
        "protect",
        "paths",
        Kind::List,
        "paths and patterns put refuses",
    ),
];

/// The settings from the config file, with warnings for lines skipped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub path: Option<PathBuf>,
    values: BTreeMap<(String, String), Value>,
    pub warnings: Vec<String>,
}

impl Config {
    /// Parse `text` (from `path`, for messages).
    pub fn parse(text: &str, path: Option<&Path>) -> Config {
        let mut cfg = Config {
            path: path.map(Path::to_path_buf),
            ..Config::default()
        };
        let name = path.map_or("config".into(), |p| p.display().to_string());
        let mut section = String::new();
        let mut lines = text.lines().enumerate();
        while let Some((n, raw)) = lines.next() {
            let line = strip_comment(raw).trim().to_string();
            if line.is_empty() {
                continue;
            }
            let mut warn = |msg: String| cfg.warnings.push(format!("{name}:{}: {msg}", n + 1));
            if let Some(rest) = line.strip_prefix('[') {
                match rest.strip_suffix(']') {
                    Some(s) if is_section(s.trim()) => section = s.trim().to_string(),
                    _ => warn(format!("bad section header '{line}'")),
                }
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                warn(format!("expected 'key = value', got '{line}'"));
                continue;
            };
            let key = key.trim();
            let mut value = value.trim().to_string();
            // An array may continue over the following lines.
            if value.starts_with('[') {
                while !array_closed(&value) {
                    match lines.next() {
                        Some((_, more)) => {
                            value.push(' ');
                            value.push_str(strip_comment(more).trim());
                        }
                        None => break,
                    }
                }
            }
            let Some(kind) = key_kind(&section, key) else {
                let at = if section.is_empty() {
                    key.to_string()
                } else {
                    format!("{section}.{key}")
                };
                warn(format!("unknown key '{at}'"));
                continue;
            };
            match parse_value(&value) {
                Some(v) if kind_of(&v) == kind => {
                    cfg.values.insert((section.clone(), key.to_string()), v);
                }
                Some(_) => warn(format!(
                    "'{section}.{key}' wants {}, got '{value}'",
                    kind_name(kind)
                )),
                None => warn(format!("cannot read the value '{value}'")),
            }
        }
        cfg
    }

    /// The names of the `[reap.profiles.NAME]` tables that set a key.
    pub fn profile_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .values
            .keys()
            .filter_map(|(s, _)| s.strip_prefix("reap.profiles.").map(str::to_string))
            .collect();
        v.dedup();
        v
    }

    pub fn get(&self, section: &str, key: &str) -> Option<&Value> {
        self.values.get(&(section.to_string(), key.to_string()))
    }

    pub fn str(&self, section: &str, key: &str) -> Option<&str> {
        match self.get(section, key) {
            Some(Value::Str(s)) => Some(s),
            _ => None,
        }
    }

    pub fn int(&self, section: &str, key: &str) -> Option<i64> {
        match self.get(section, key) {
            Some(Value::Int(n)) => Some(*n),
            _ => None,
        }
    }

    pub fn bool(&self, section: &str, key: &str) -> Option<bool> {
        match self.get(section, key) {
            Some(Value::Bool(b)) => Some(*b),
            _ => None,
        }
    }

    pub fn list(&self, section: &str, key: &str) -> &[String] {
        match self.get(section, key) {
            Some(Value::List(v)) => v,
            _ => &[],
        }
    }

    /// A warning about a value that parsed but means nothing.
    pub fn bad_value(&self, section: &str, key: &str, want: &str) -> String {
        let at = self
            .path
            .as_ref()
            .map_or("config".into(), |p| p.display().to_string());
        let v = match self.get(section, key) {
            Some(Value::Str(s)) => s.clone(),
            Some(Value::Int(n)) => n.to_string(),
            Some(other) => format!("{other:?}"),
            None => String::new(),
        };
        format!("{at}: ignoring {section}.{key} = '{v}' (want {want})")
    }
}

fn kind_of(v: &Value) -> Kind {
    match v {
        Value::Str(_) => Kind::Str,
        Value::Int(_) => Kind::Int,
        Value::Bool(_) => Kind::Bool,
        Value::List(_) => Kind::List,
    }
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Str => "a string",
        Kind::Int => "an integer",
        Kind::Bool => "true or false",
        Kind::List => "an array of strings",
    }
}

/// The keys of a `[reap.profiles.NAME]` table.
const PROFILE_KEYS: &[(&str, Kind, &str)] = &[
    ("about", Kind::Str, "one line on what the profile reaps"),
    ("paths", Kind::List, "globs of entries to reap"),
    ("age", Kind::Str, "entries older than this go, such as 7d"),
    ("action", Kind::Str, "trash or permanent"),
    ("include", Kind::List, "other profiles this one runs"),
    (
        "marker",
        Kind::Str,
        "a file an entry must hold to be reaped",
    ),
    (
        "busy",
        Kind::List,
        "commands that keep the entry's parent busy",
    ),
    (
        "guard",
        Kind::Bool,
        "keep entries holding credential-like files",
    ),
    (
        "keep_newest",
        Kind::Bool,
        "spare the newest version of each NAME-VERSION entry",
    ),
    (
        "tool",
        Kind::List,
        "the tool's own cleanup command, for --tool",
    ),
];

/// What `key` holds in `section`, when it is a known key.
fn key_kind(section: &str, key: &str) -> Option<Kind> {
    if let Some(name) = section.strip_prefix("reap.profiles.") {
        if !is_bare_key(name) {
            return None;
        }
        return PROFILE_KEYS
            .iter()
            .find(|(k, _, _)| *k == key)
            .map(|&(_, kind, _)| kind);
    }
    KEYS.iter()
        .find(|(s, k, _, _)| *s == section && *k == key)
        .map(|&(_, _, kind, _)| kind)
}

/// A section header: bare keys joined by dots.
fn is_section(s: &str) -> bool {
    s.split('.').all(is_bare_key)
}

fn is_bare_key(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The line without a `#` comment (one outside any string).
fn strip_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if c == '\\' => escaped = true,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '#' => return &line[..i],
            None => {}
        }
    }
    line
}

/// Whether an array value has its closing bracket (outside strings).
fn array_closed(v: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut depth = 0i32;
    for c in v.chars() {
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if c == '\\' => escaped = true,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '"' | '\'' => quote = Some(c),
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        return true;
                    }
                }
                _ => {}
            },
        }
    }
    false
}

/// Parse one value: a string, an integer, a boolean or a string array.
pub fn parse_value(v: &str) -> Option<Value> {
    let v = v.trim();
    match v {
        "true" => return Some(Value::Bool(true)),
        "false" => return Some(Value::Bool(false)),
        _ => {}
    }
    if v.starts_with('"') || v.starts_with('\'') {
        let (s, rest) = parse_string(v)?;
        return rest.trim().is_empty().then_some(Value::Str(s));
    }
    if let Some(inner) = v.strip_prefix('[') {
        let mut items = Vec::new();
        let mut rest = inner.trim_start();
        loop {
            if let Some(after) = rest.strip_prefix(']') {
                return after.trim().is_empty().then_some(Value::List(items));
            }
            let (s, after) = parse_string(rest)?;
            items.push(s);
            rest = after.trim_start();
            if let Some(after) = rest.strip_prefix(',') {
                rest = after.trim_start();
            } else if !rest.starts_with(']') {
                return None;
            }
        }
    }
    let digits = v.replace('_', "");
    digits.parse::<i64>().ok().map(Value::Int)
}

/// A basic (`"..."`, with escapes) or literal (`'...'`) string at the
/// start of `s`; returns it and what follows.
fn parse_string(s: &str) -> Option<(String, &str)> {
    let mut chars = s.char_indices();
    let (_, q) = chars.next()?;
    if q == '\'' {
        let end = s[1..].find('\'')? + 1;
        return Some((s[1..end].to_string(), &s[end + 1..]));
    }
    if q != '"' {
        return None;
    }
    let mut out = String::new();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Some((out, &s[i + 1..])),
            '\\' => {
                let (_, e) = chars.next()?;
                match e {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    'u' | 'U' => {
                        let len = if e == 'u' { 4 } else { 8 };
                        let mut hex = String::new();
                        for _ in 0..len {
                            hex.push(chars.next()?.1);
                        }
                        out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                    }
                    _ => return None,
                }
            }
            c => out.push(c),
        }
    }
    None
}

/// Where the config file lives, or `None` when `RTRASH_CONFIG` is empty.
pub fn path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("RTRASH_CONFIG") {
        return (!p.is_empty()).then(|| PathBuf::from(p));
    }
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute());
    #[cfg(windows)]
    let xdg = xdg.or_else(|| {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    });
    let base = xdg.or_else(|| crate::util::home_dir().map(|h| h.join(".config")))?;
    Some(base.join("rtrash").join("config.toml"))
}

/// Read the config file. A missing file is an empty config; one that
/// cannot be read earns a warning.
pub fn load() -> Config {
    let Some(p) = path() else {
        return Config::default();
    };
    match std::fs::read_to_string(&p) {
        Ok(text) => Config::parse(&text, Some(&p)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config {
            path: Some(p),
            ..Config::default()
        },
        Err(e) => Config {
            warnings: vec![format!("{}: cannot read: {e}", p.display())],
            path: Some(p),
            ..Config::default()
        },
    }
}

/// The config, read once per process; its warnings go to stderr then.
pub fn get() -> &'static Config {
    static CFG: OnceLock<Config> = OnceLock::new();
    CFG.get_or_init(|| {
        let c = load();
        for w in &c.warnings {
            eprintln!("rtrash: warning: {w}");
        }
        c
    })
}

/// A config file with every key commented out, for `rtrash config --default`.
pub fn template() -> String {
    let mut out = String::from(
        "# rtrash config. Environment variables and command-line options win\n\
         # over these settings. Uncomment a line to use it.\n",
    );
    let mut section = "";
    for (s, k, kind, what) in KEYS {
        if *s != section {
            section = s;
            let _ = write!(out, "\n[{s}]\n");
        }
        let example = match (*s, *k, kind) {
            ("du", "sort", _) => "\"disk\"".to_string(),
            ("du", "top", _) => "20".to_string(),
            ("du", "threads", _) => "8".to_string(),
            ("du", "exclude", _) => "[\"*.iso\", \"/scratch/\"]".to_string(),
            ("du", "exclude_regex", _) => "[\"\\\\.tmp$\"]".to_string(),
            ("du", "git", _) => "true".to_string(),
            ("list", "sort", _) => "\"date\"".to_string(),
            ("policy", "max_size", _) => "\"50G\"".to_string(),
            ("policy", "min_free", _) => "\"10G\"".to_string(),
            ("policy", "warn_size", _) => "\"20%\"".to_string(),
            ("policy", "in_use", _) => "\"warn\"".to_string(),
            ("policy", "prune", _) => "\"oldest\"".to_string(),
            ("protect", "paths", _) => "[\"/srv/data\"]".to_string(),
            (_, _, Kind::Bool) => "false".to_string(),
            (_, _, Kind::List) => "[]".to_string(),
            (_, _, Kind::Int) => "0".to_string(),
            (_, _, Kind::Str) => "\"\"".to_string(),
        };
        let _ = writeln!(out, "# {k} = {example}    # {what}");
    }
    out.push_str(
        "\n# A reap profile; the same name as a built-in replaces it.\n# [reap.profiles.NAME]\n",
    );
    for (k, _, what) in PROFILE_KEYS {
        let _ = writeln!(out, "#   {k}: {what}");
    }
    out
}

const HELP: &str = "\
Usage: {prog} config [--path | --default]
Show the config file and the settings it makes. Settings rank below
environment variables and command-line options.

      --path       print where the config file is looked for
      --default    print a config file with every setting commented out
      --help       display this help and exit
      --version    output version information and exit
";

pub fn run(prog: &str, args: &[String]) -> i32 {
    let mut mode = "show";
    for a in args {
        match a.as_str() {
            "--path" => mode = "path",
            "--default" => mode = "default",
            "--help" => {
                print!("{}", HELP.replace("{prog}", prog));
                return 0;
            }
            "--version" => {
                println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                return 0;
            }
            other => {
                eprintln!("{prog}: config: unrecognized argument '{other}'");
                eprintln!("Try '{prog} config --help' for more information.");
                return 2;
            }
        }
    }
    match mode {
        "path" => match path() {
            Some(p) => println!("{}", p.display()),
            None => println!("(off: RTRASH_CONFIG is empty)"),
        },
        "default" => print!("{}", template()),
        _ => {
            let c = load();
            match &c.path {
                Some(p) if p.is_file() => println!("# {}", p.display()),
                Some(p) => println!("# {} (not present)", p.display()),
                None => println!("# off: RTRASH_CONFIG is empty"),
            }
            let mut section = "";
            for ((s, k), v) in &c.values {
                if s != section {
                    section = s;
                    println!("[{s}]");
                }
                println!("{k} = {}", show(v));
            }
            for w in &c.warnings {
                eprintln!("{prog}: warning: {w}");
            }
            if !c.warnings.is_empty() {
                return 1;
            }
        }
    }
    0
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn show(v: &Value) -> String {
    match v {
        Value::Str(s) => quote(s),
        Value::Int(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::List(l) => {
            let items: Vec<String> = l.iter().map(|s| quote(s)).collect();
            format!("[{}]", items.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_subset() {
        let c = Config::parse(
            r#"
# top comment
[du]
top = 5          # trailing comment
sort = "items"
gitignore = true
exclude = [
  "*.iso",   # images
  '/scratch/',
  "a#b",
]
[policy]
max_size = '50G'
[protect]
paths = ["/srv/da\"ta", "\u00e9"]
"#,
            None,
        );
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        assert_eq!(c.int("du", "top"), Some(5));
        assert_eq!(c.str("du", "sort"), Some("items"));
        assert_eq!(c.bool("du", "gitignore"), Some(true));
        assert_eq!(c.list("du", "exclude"), ["*.iso", "/scratch/", "a#b"]);
        assert_eq!(c.str("policy", "max_size"), Some("50G"));
        assert_eq!(c.list("protect", "paths"), ["/srv/da\"ta", "é"]);
        assert_eq!(c.list("du", "nothing"), [] as [String; 0]);
    }

    #[test]
    fn bad_lines_warn_and_the_rest_applies() {
        let c = Config::parse(
            "[du]\ntop = \"five\"\ncolour = true\nnot a line\n[bad header\nsort = 'mtime'\n[list]\nsize = yes\n",
            Some(Path::new("/c.toml")),
        );
        assert_eq!(c.str("du", "sort"), Some("mtime"));
        assert_eq!(c.int("du", "top"), None);
        assert_eq!(c.warnings.len(), 5, "{:?}", c.warnings);
        assert!(c.warnings[0].starts_with("/c.toml:2: 'du.top' wants an integer"));
        assert!(c.warnings[1].contains("unknown key 'du.colour'"));
        assert!(c.warnings[4].contains("cannot read the value 'yes'"));
    }

    #[test]
    fn values_parse_or_refuse() {
        assert_eq!(parse_value("1_000"), Some(Value::Int(1000)));
        assert_eq!(parse_value("\"a\\tb\""), Some(Value::Str("a\tb".into())));
        assert_eq!(parse_value("\"open"), None);
        assert_eq!(parse_value("\"x\" y"), None);
        assert_eq!(parse_value("[\"a\" \"b\"]"), None);
        assert_eq!(parse_value("[]"), Some(Value::List(Vec::new())));
        assert_eq!(parse_value("[1]"), None);
        assert_eq!(parse_value("\"\\q\""), None);
    }

    #[test]
    fn reap_profile_tables_take_their_keys_and_nothing_else() {
        let c = Config::parse(
            "[reap.profiles.mine]\npaths = [\"~/x/*\"]\nage = \"2d\"\nguard = true\nbogus = 1\n[reap.profiles.a.b]\nage = \"1d\"\n[a..b]\n",
            None,
        );
        assert_eq!(c.profile_names(), ["mine"]);
        assert_eq!(c.list("reap.profiles.mine", "paths"), ["~/x/*"]);
        assert_eq!(c.bool("reap.profiles.mine", "guard"), Some(true));
        assert_eq!(c.warnings.len(), 3, "{:?}", c.warnings);
    }

    #[test]
    fn template_parses_back_clean_when_uncommented() {
        let t = template();
        let uncommented: String = t
            .lines()
            .map(|l| {
                l.strip_prefix("# ")
                    .filter(|r| r.contains(" = "))
                    .unwrap_or(l)
            })
            .map(|l| format!("{l}\n"))
            .collect();
        let c = Config::parse(&uncommented, None);
        assert!(c.warnings.is_empty(), "{:?}\n{uncommented}", c.warnings);
        assert_eq!(c.int("du", "top"), Some(20));
        assert_eq!(Config::parse(&t, None), Config::default());
    }
}
