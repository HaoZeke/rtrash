//! Reap profiles: named sets of cache and temp globs with an age and an
//! action, built in for common tools and overridable in the config file
//! under `[reap.profiles.NAME]`.
//!
//! The built-in paths come from each tool's documented cache location.
//! They name only caches, logs and scratch files the tool rebuilds, never
//! its config, credentials, history or sessions.

use std::path::{Path, PathBuf};

/// What reap does with a stale entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Trash,
    Permanent,
}

impl Action {
    pub fn parse(s: &str) -> Option<Action> {
        match s {
            "trash" => Some(Action::Trash),
            "permanent" => Some(Action::Permanent),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Action::Trash => "trash",
            Action::Permanent => "permanent",
        }
    }
}

/// One profile, built in or from the config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub about: String,
    /// Globs, each optionally prefixed `linux:`, `macos:`, `unix:` or `windows:`.
    pub paths: Vec<String>,
    /// Seconds.
    pub age: i64,
    pub action: Action,
    pub include: Vec<String>,
    /// A file an entry must hold, such as cargo's `.rustc_info.json`.
    pub marker: Option<String>,
    /// Commands that, holding anything in an entry's parent, keep it.
    pub busy: Vec<String>,
    /// Keep entries that hold credential-like files.
    pub guard: bool,
    /// The tool's own cleanup, run by `--tool` when it is on PATH.
    /// `{age}` becomes the age in days or hours, such as `7d` or `2h`.
    pub tool: Vec<String>,
    /// Extra directories a `{roots}` glob searches.
    pub roots: Vec<String>,
    /// How deep `**` reaches; four when unset.
    pub depth: Option<usize>,
    pub from_config: bool,
}

const DAY: i64 = 86_400;

struct B {
    name: &'static str,
    about: &'static str,
    paths: &'static [&'static str],
    days: i64,
    action: Action,
    include: &'static [&'static str],
    marker: Option<&'static str>,
    busy: &'static [&'static str],
    guard: bool,
    tool: &'static [&'static str],
}

const fn b(
    name: &'static str,
    about: &'static str,
    paths: &'static [&'static str],
    days: i64,
    action: Action,
) -> B {
    B {
        name,
        about,
        paths,
        days,
        action,
        include: &[],
        marker: None,
        busy: &[],
        guard: false,
        tool: &[],
    }
}

const fn group(name: &'static str, about: &'static str, include: &'static [&'static str]) -> B {
    B {
        include,
        ..b(name, about, &[], 0, Action::Trash)
    }
}

use Action::{Permanent, Trash};

/// `C` is the per-user cache root: `$XDG_CACHE_HOME` on Linux,
/// `~/Library/Caches` on macOS.
const BUILTIN: &[B] = &[
    group(
        "ai",
        "AI coding tools: claude, cursor, codex, aider, ollama, huggingface",
        &["claude", "cursor", "codex", "aider", "ollama", "huggingface"],
    ),
    B {
        guard: true,
        ..b(
            "claude",
            "Claude Code shell snapshots, todo lists and temp dirs (never settings, credentials or projects)",
            &[
                "~/.claude/shell-snapshots/*",
                "~/.claude/todos/*",
                "unix:${TMPDIR:-/tmp}/claude-*",
                "windows:${TEMP}/claude-*",
            ],
            7,
            Trash,
        )
    },
    B {
        guard: true,
        ..b(
            "cursor",
            "Cursor editor caches and logs, and agent terminal logs (never User settings or state)",
            &[
                "linux:${XDG_CONFIG_HOME:-~/.config}/Cursor/Cache/*",
                "linux:${XDG_CONFIG_HOME:-~/.config}/Cursor/CachedData/*",
                "linux:${XDG_CONFIG_HOME:-~/.config}/Cursor/Code Cache/*",
                "linux:${XDG_CONFIG_HOME:-~/.config}/Cursor/GPUCache/*",
                "linux:${XDG_CONFIG_HOME:-~/.config}/Cursor/logs/*",
                "windows:${APPDATA}/Cursor/Cache/*",
                "windows:${APPDATA}/Cursor/CachedData/*",
                "windows:${APPDATA}/Cursor/Code Cache/*",
                "windows:${APPDATA}/Cursor/GPUCache/*",
                "windows:${APPDATA}/Cursor/logs/*",
                "macos:~/Library/Application Support/Cursor/Cache/*",
                "macos:~/Library/Application Support/Cursor/CachedData/*",
                "macos:~/Library/Application Support/Cursor/Code Cache/*",
                "macos:~/Library/Application Support/Cursor/GPUCache/*",
                "macos:~/Library/Application Support/Cursor/logs/*",
                "~/.cursor/projects/*/terminals/*",
            ],
            7,
            Trash,
        )
    },
    B {
        guard: true,
        ..b("codex", "Codex CLI logs (never sessions, auth or config)", &["${CODEX_HOME:-~/.codex}/log/*"], 14, Trash)
    },
    B {
        guard: true,
        ..b("aider", "aider caches (never history files)", &["~/.aider/caches/*"], 14, Trash)
    },
    b(
        "ollama",
        "ollama runner scratch dirs in the temp dir (never models)",
        &["unix:${TMPDIR:-/tmp}/ollama*", "windows:${TEMP}/ollama*"],
        2,
        Permanent,
    ),
    b(
        "huggingface",
        "Hugging Face partial downloads and the xet chunk cache (never finished model or dataset files)",
        &[
            "unix:${HF_HOME:-~/.cache/huggingface}/hub/*/blobs/*.incomplete",
            "unix:${HF_HOME:-~/.cache/huggingface}/xet/*/chunk-cache/*",
            "windows:${HF_HOME:-~/.cache/huggingface}/hub/*/blobs/*.incomplete",
            "windows:${HF_HOME:-~/.cache/huggingface}/xet/*/chunk-cache/*",
        ],
        2,
        Permanent,
    ),
    group(
        "compilers",
        "build caches: cargo targets and registry, sccache, ccache, go, zig, bazel, gradle",
        &["cargo-target", "cargo-registry", "sccache", "ccache", "go", "zig", "bazel", "gradle"],
    ),
    B {
        marker: Some(".rustc_info.json"),
        busy: &["cargo", "rustc"],
        ..b(
            "cargo-target",
            "idle cargo target dirs up to four levels below home and each root, and $CARGO_TARGET_DIR",
            &["~/**/target", "{roots}/**/target", "${CARGO_TARGET_DIR}"],
            14,
            Permanent,
        )
    },
    b(
        "cargo-registry",
        "downloaded crates and their sources, and git checkouts",
        &[
            "${CARGO_HOME:-~/.cargo}/registry/cache/*/*",
            "${CARGO_HOME:-~/.cargo}/registry/src/*/*",
            "${CARGO_HOME:-~/.cargo}/git/checkouts/*",
        ],
        30,
        Permanent,
    ),
    b("sccache", "sccache local disk cache", &["unix:${SCCACHE_DIR:-C/sccache}/*/*", "macos:~/Library/Caches/Mozilla.sccache/*/*", "windows:${SCCACHE_DIR:-C/Mozilla/sccache/cache}/*/*"], 30, Permanent),
    B {
        tool: &["ccache", "--evict-older-than", "{age}"],
        ..b("ccache", "ccache objects (never ccache.conf)", &["${CCACHE_DIR:-C/ccache}/[0-9a-f]/*"], 30, Permanent)
    },
    B {
        tool: &["go", "clean", "-cache"],
        ..b("go", "Go build cache (not the module cache)", &["${GOCACHE:-C/go-build}/[0-9a-f][0-9a-f]"], 10, Permanent)
    },
    b("zig", "Zig global cache", &["linux:${XDG_CACHE_HOME:-~/.cache}/zig/o/*", "linux:${XDG_CACHE_HOME:-~/.cache}/zig/tmp/*", "macos:~/Library/Caches/zig/o/*", "macos:~/Library/Caches/zig/tmp/*", "windows:C/zig/o/*", "windows:C/zig/tmp/*"], 30, Permanent),
    b("bazel", "Bazel output bases no server holds", &["linux:${XDG_CACHE_HOME:-~/.cache}/bazel/_bazel_*/[0-9a-f]*", "macos:/private/var/tmp/_bazel_*/[0-9a-f]*"], 30, Permanent),
    b("gradle", "Gradle build cache and daemon logs", &["${GRADLE_USER_HOME:-~/.gradle}/caches/build-cache-*/*", "${GRADLE_USER_HOME:-~/.gradle}/daemon/*/daemon-*.out.log"], 30, Permanent),
    group(
        "pkg",
        "package caches: pip, uv, npm, pnpm, yarn, bun, conda, mamba, pre-commit",
        &["pip", "uv", "npm", "pnpm", "yarn", "bun", "conda", "mamba", "pre-commit"],
    ),
    B {
        tool: &["pip", "cache", "purge"],
        ..b("pip", "pip HTTP and wheel cache", &["unix:${PIP_CACHE_DIR:-C/pip}/http*/*", "unix:${PIP_CACHE_DIR:-C/pip}/wheels/*", "windows:${PIP_CACHE_DIR:-C/pip/Cache}/http*/*", "windows:${PIP_CACHE_DIR:-C/pip/Cache}/wheels/*"], 30, Permanent)
    },
    B {
        tool: &["uv", "cache", "prune"],
        ..b("uv", "uv cache buckets", &["unix:${UV_CACHE_DIR:-~/.cache/uv}/*-v[0-9]*/*", "windows:${UV_CACHE_DIR:-C/uv/cache}/*-v[0-9]*/*"], 30, Permanent)
    },
    B {
        tool: &["npm", "cache", "clean", "--force"],
        ..b("npm", "npm content cache, logs and npx installs", &["unix:~/.npm/_cacache/content-v2/*/*", "unix:~/.npm/_logs/*", "unix:~/.npm/_npx/*", "windows:C/npm-cache/_cacache/content-v2/*/*", "windows:C/npm-cache/_logs/*", "windows:C/npm-cache/_npx/*"], 30, Permanent)
    },
    B {
        tool: &["pnpm", "store", "prune"],
        ..b("pnpm", "pnpm metadata cache (the store is left to pnpm store prune)", &["unix:C/pnpm/*", "windows:C/pnpm-cache/*"], 30, Permanent)
    },
    B {
        tool: &["yarn", "cache", "clean"],
        ..b("yarn", "yarn v1 and berry caches", &["linux:${XDG_CACHE_HOME:-~/.cache}/yarn/v*/*", "macos:~/Library/Caches/Yarn/v*/*", "windows:C/Yarn/Cache/v*/*", "~/.yarn/berry/cache/*"], 30, Permanent)
    },
    B {
        tool: &["bun", "pm", "cache", "rm"],
        ..b("bun", "bun install cache", &["${BUN_INSTALL_CACHE_DIR:-~/.bun/install/cache}/*"], 30, Permanent)
    },
    B {
        tool: &["conda", "clean", "--tarballs", "--yes"],
        ..b(
            "conda",
            "conda package tarballs (not the extracted packages envs link to)",
            &[
                "~/miniconda3/pkgs/*.conda",
                "~/miniconda3/pkgs/*.tar.bz2",
                "~/miniforge3/pkgs/*.conda",
                "~/miniforge3/pkgs/*.tar.bz2",
                "~/anaconda3/pkgs/*.conda",
                "~/anaconda3/pkgs/*.tar.bz2",
                "~/.conda/pkgs/*.conda",
                "~/.conda/pkgs/*.tar.bz2",
            ],
            30,
            Permanent,
        )
    },
    B {
        tool: &["micromamba", "clean", "--tarballs", "--yes"],
        ..b(
            "mamba",
            "micromamba and mamba package tarballs (not the extracted packages envs link to)",
            &[
                "${MAMBA_ROOT_PREFIX:-~/micromamba}/pkgs/*.conda",
                "${MAMBA_ROOT_PREFIX:-~/micromamba}/pkgs/*.tar.bz2",
                "~/.local/share/mamba/pkgs/*.conda",
                "~/.local/share/mamba/pkgs/*.tar.bz2",
                "~/.mamba/pkgs/*.conda",
                "~/.mamba/pkgs/*.tar.bz2",
            ],
            30,
            Permanent,
        )
    },
    B {
        tool: &["pre-commit", "gc"],
        busy: &["pre-commit", "prek"],
        ..b(
            "pre-commit",
            "pre-commit and prek hook environments, which they rebuild on the next run",
            &[
                "unix:${PRE_COMMIT_HOME:-C/pre-commit}/repo*",
                "unix:${PREK_HOME:-C/prek}/repos/*",
                "windows:${PRE_COMMIT_HOME:-~/.cache/pre-commit}/repo*",
                "windows:${PREK_HOME:-~/.cache/prek}/repos/*",
            ],
            30,
            Permanent,
        )
    },
    b(
        "thumbnails",
        "freedesktop thumbnails",
        &["linux:${XDG_CACHE_HOME:-~/.cache}/thumbnails/*/*"],
        90,
        Permanent,
    ),
];

fn from_builtin(x: &B) -> Profile {
    Profile {
        name: x.name.into(),
        about: x.about.into(),
        paths: x.paths.iter().map(|s| s.to_string()).collect(),
        age: x.days * DAY,
        action: x.action,
        include: x.include.iter().map(|s| s.to_string()).collect(),
        marker: x.marker.map(str::to_string),
        busy: x.busy.iter().map(|s| s.to_string()).collect(),
        guard: x.guard,
        tool: x.tool.iter().map(|s| s.to_string()).collect(),
        roots: Vec::new(),
        depth: None,
        from_config: false,
    }
}

/// Built-ins with the config file's profiles over them; warnings for
/// values that make no sense.
pub fn all(cfg: &crate::config::Config, warn: &mut Vec<String>) -> Vec<Profile> {
    let mut out: Vec<Profile> = BUILTIN.iter().map(from_builtin).collect();
    for name in cfg.profile_names() {
        let sec = format!("reap.profiles.{name}");
        let age = match cfg.str(&sec, "age") {
            None => 30 * DAY,
            Some(a) => crate::reap::parse_age(a).unwrap_or_else(|| {
                warn.push(cfg.bad_value(&sec, "age", "an age such as 7d"));
                30 * DAY
            }),
        };
        let action = match cfg.str(&sec, "action") {
            None => Action::Trash,
            Some(a) => Action::parse(a).unwrap_or_else(|| {
                warn.push(cfg.bad_value(&sec, "action", "trash or permanent"));
                Action::Trash
            }),
        };
        let p = Profile {
            name: name.clone(),
            about: cfg
                .str(&sec, "about")
                .unwrap_or("from the config file")
                .to_string(),
            paths: cfg.list(&sec, "paths").to_vec(),
            age,
            action,
            include: cfg.list(&sec, "include").to_vec(),
            marker: cfg.str(&sec, "marker").map(str::to_string),
            busy: cfg.list(&sec, "busy").to_vec(),
            guard: cfg.bool(&sec, "guard").unwrap_or(false),
            tool: cfg.list(&sec, "tool").to_vec(),
            roots: cfg.list(&sec, "roots").to_vec(),
            depth: cfg.int(&sec, "depth").and_then(|d| usize::try_from(d).ok()),
            from_config: true,
        };
        match out.iter_mut().find(|q| q.name == name) {
            Some(q) => *q = p,
            None => out.push(p),
        }
    }
    out
}

/// The named profiles with groups opened up, each once, in order.
pub fn select(all: &[Profile], names: &[String]) -> Result<Vec<Profile>, String> {
    fn add(
        all: &[Profile],
        name: &str,
        out: &mut Vec<Profile>,
        depth: usize,
    ) -> Result<(), String> {
        if depth > 8 {
            return Err(format!("profile '{name}' includes itself"));
        }
        let p = all
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| format!("no profile named '{name}'"))?;
        if !p.paths.is_empty() && !out.iter().any(|q| q.name == p.name) {
            out.push(p.clone());
        }
        for inc in &p.include {
            add(all, inc, out, depth + 1)?;
        }
        Ok(())
    }
    let mut out = Vec::new();
    for n in names {
        add(all, n, &mut out, 0)?;
    }
    Ok(out)
}

/// The cache root `C` stands for.
fn cache_root() -> Option<String> {
    if cfg!(windows) {
        return std::env::var("LOCALAPPDATA")
            .ok()
            .filter(|v| is_absolute(v));
    }
    if cfg!(target_os = "macos") {
        return crate::util::home_dir().map(|h| format!("{}/Library/Caches", h.display()));
    }
    match std::env::var("XDG_CACHE_HOME") {
        Ok(v) if v.starts_with('/') => Some(v),
        _ => crate::util::home_dir().map(|h| format!("{}/.cache", h.display())),
    }
}

/// Every expansion of `pat`: one per root for a `{roots}` glob (none
/// without roots), else at most one.
pub fn expand_all(pat: &str, roots: &[PathBuf]) -> Vec<String> {
    let (os, body) = match pat.split_once(':') {
        Some((os @ ("linux" | "macos" | "unix" | "windows"), rest)) => (Some(os), rest),
        _ => (None, pat),
    };
    let Some(rest) = body.strip_prefix("{roots}") else {
        return expand(pat).into_iter().collect();
    };
    roots
        .iter()
        .filter_map(|r| {
            let joined = format!("{}{rest}", r.display());
            match os {
                Some(os) => expand(&format!("{os}:{joined}")),
                None => expand(&joined),
            }
        })
        .collect()
}

/// The `{roots}` directories: `--roots`, then `RTRASH_REAP_ROOTS` (a
/// path list like PATH), then the profile's `roots` key, each once.
pub fn roots(cli: &[PathBuf], profile: &Profile) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = cli.to_vec();
    if let Some(v) = std::env::var_os("RTRASH_REAP_ROOTS") {
        out.extend(std::env::split_paths(&v).filter(|p| p.is_absolute()));
    }
    out.extend(
        profile
            .roots
            .iter()
            .filter_map(|r| expand(r))
            .map(PathBuf::from),
    );
    let mut seen = Vec::new();
    out.retain(|p| {
        let keep = !seen.contains(p);
        seen.push(p.clone());
        keep
    });
    out
}

/// Expand `~`, `C`, `$VAR` and `${VAR:-default}`; None when the pattern
/// is for another OS or a variable without default is unset or empty.
pub fn expand(pat: &str) -> Option<String> {
    let mut pat = pat;
    for (prefix, here) in [
        ("linux:", cfg!(target_os = "linux")),
        ("macos:", cfg!(target_os = "macos")),
        ("unix:", cfg!(unix)),
        ("windows:", cfg!(windows)),
    ] {
        if let Some(r) = pat.strip_prefix(prefix) {
            if !here {
                return None;
            }
            pat = r;
            break;
        }
    }
    let s = expand_vars(pat)?;
    let s = if s == "~" || s.starts_with("~/") {
        format!("{}{}", crate::util::home_dir()?.display(), &s[1..])
    } else if s == "C" || s.starts_with("C/") {
        format!("{}{}", cache_root()?, &s[1..])
    } else {
        s
    };
    is_absolute(&s).then_some(s)
}

/// `/x`, or on Windows also `C:\x`, `C:/x` and `\\server\share`.
fn is_absolute(s: &str) -> bool {
    if s.starts_with('/') {
        return true;
    }
    if !cfg!(windows) {
        return false;
    }
    let b = s.as_bytes();
    s.starts_with("\\\\")
        || (b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && (b[2] == b'/' || b[2] == b'\\'))
}

fn expand_vars(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        rest = &rest[i + 1..];
        if let Some(body) = rest.strip_prefix('{') {
            let end = body.find('}')?;
            let inner = &body[..end];
            rest = &body[end + 1..];
            let (name, default) = match inner.split_once(":-") {
                Some((n, d)) => (n, Some(d)),
                None => (inner, None),
            };
            match std::env::var(name) {
                Ok(v) if !v.is_empty() => out.push_str(&v),
                _ => out.push_str(&expand_vars(default?)?),
            }
        } else {
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            let name = &rest[..end];
            rest = &rest[end..];
            match std::env::var(name) {
                Ok(v) if !v.is_empty() => out.push_str(&v),
                _ => return None,
            }
        }
    }
    out.push_str(rest);
    Some(out)
}

/// How deep `**` reaches by default.
pub const STAR_DEPTH: usize = 4;

/// The existing paths an expanded pattern matches.
/// A `*` never matches a leading dot, `**` spans up to four directories
/// and never enters hidden directories, symlinks or `node_modules`, and
/// a match is never searched for more matches.
pub fn glob(pattern: &str, depth: usize) -> Vec<PathBuf> {
    let seps: &[char] = if cfg!(windows) { &['/', '\\'] } else { &['/'] };
    let mut comps: Vec<&str> = pattern.split(seps).filter(|c| !c.is_empty()).collect();
    let mut base = PathBuf::from("/");
    // A drive letter starts from that drive's root.
    if cfg!(windows)
        && comps
            .first()
            .is_some_and(|c| c.len() == 2 && c.ends_with(':'))
    {
        base = PathBuf::from(format!("{}\\", comps.remove(0)));
    }
    let mut out = Vec::new();
    walk(&base, &comps, depth, &mut out);
    out.sort();
    out.dedup();
    out
}

fn has_magic(c: &str) -> bool {
    c.contains(['*', '?', '['])
}

fn walk(base: &Path, comps: &[&str], max: usize, out: &mut Vec<PathBuf>) {
    let Some((first, rest)) = comps.split_first() else {
        if std::fs::symlink_metadata(base).is_ok() {
            out.push(base.to_path_buf());
        }
        return;
    };
    if *first == "**" {
        star(base, rest, max, out, 0);
        return;
    }
    if !has_magic(first) {
        let next = base.join(first);
        if rest.is_empty() || next.is_dir() {
            walk(&next, rest, max, out);
        }
        return;
    }
    let Ok(rd) = std::fs::read_dir(base) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') && !first.starts_with('.') {
            continue;
        }
        if crate::util::fnmatch(first, &name) {
            let p = e.path();
            if rest.is_empty() {
                out.push(p);
            } else if e.file_type().is_ok_and(|t| t.is_dir()) {
                walk(&p, rest, max, out);
            }
        }
    }
}

fn star(base: &Path, rest: &[&str], max: usize, out: &mut Vec<PathBuf>, depth: usize) {
    let before = out.len();
    walk(base, rest, max, out);
    if depth >= max {
        return;
    }
    let found: Vec<PathBuf> = out[before..].to_vec();
    let Ok(rd) = std::fs::read_dir(base) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        let p = e.path();
        // Never look for matches inside a match.
        if found.contains(&p) {
            continue;
        }
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            star(&p, rest, max, out, depth + 1);
        }
    }
}

/// Names that look like credentials, history or settings, which a
/// guarded profile never reaps an entry holding.
const SECRET: &[&str] = &[
    ".credentials*",
    "credentials*",
    "auth.json",
    "*.pem",
    "*.key",
    "id_rsa*",
    "id_ed25519*",
    "*history*",
    "settings*.json",
    "config.toml",
    "*.sqlite",
    "*.db",
];

pub fn secret_name(name: &str) -> bool {
    SECRET.iter().any(|p| crate::util::fnmatch(p, name))
}

/// The tool command with `{age}` filled in.
pub fn tool_command(p: &Profile, age: i64) -> Vec<String> {
    let a = if age % DAY == 0 {
        format!("{}d", age / DAY)
    } else {
        format!("{}h", (age / 3600).max(1))
    };
    p.tool.iter().map(|s| s.replace("{age}", &a)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables_expand_with_defaults_and_unset_ones_drop_the_path() {
        std::env::set_var("RTRASH_T_SET", "/x");
        std::env::remove_var("RTRASH_T_UNSET");
        assert_eq!(expand("${RTRASH_T_SET}/a").as_deref(), Some("/x/a"));
        assert_eq!(expand("${RTRASH_T_UNSET:-/d}/a").as_deref(), Some("/d/a"));
        assert_eq!(expand("$RTRASH_T_UNSET/a"), None);
        assert_eq!(expand("${RTRASH_T_UNSET}"), None);
        let other = if cfg!(target_os = "macos") {
            "linux:/a"
        } else {
            "macos:/a"
        };
        assert_eq!(expand(other), None);
        assert_eq!(expand("relative/*"), None);
    }

    #[test]
    fn roots_globs_expand_once_per_root_and_vanish_without_roots() {
        let roots = [PathBuf::from("/w"), PathBuf::from("/srv/code")];
        assert_eq!(
            expand_all("{roots}/**/target", &roots),
            ["/w/**/target", "/srv/code/**/target"]
        );
        assert!(expand_all("{roots}/**/target", &[]).is_empty());
        assert_eq!(expand_all("/a/*", &roots), ["/a/*"]);
    }

    #[test]
    fn groups_open_up_once_and_unknown_names_fail() {
        let all: Vec<Profile> = BUILTIN.iter().map(from_builtin).collect();
        let got = select(&all, &["ai".into(), "claude".into()]).unwrap();
        let names: Vec<&str> = got.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "claude",
                "cursor",
                "codex",
                "aider",
                "ollama",
                "huggingface"
            ]
        );
        assert!(select(&all, &["nope".into()]).is_err());
    }

    #[test]
    fn no_builtin_path_names_config_credentials_or_history() {
        for p in BUILTIN {
            for path in p.paths {
                for bad in [
                    "settings",
                    "credential",
                    "history",
                    "projects/*/",
                    "sessions",
                    "auth",
                    "/User/",
                    "models",
                ] {
                    if bad == "projects/*/" && path.contains("/terminals/") {
                        continue;
                    }
                    assert!(!path.contains(bad), "{} reaches {path}", p.name);
                }
            }
        }
    }

    #[test]
    fn globs_skip_dot_names_and_stop_at_a_match() {
        let t = std::env::temp_dir().join(format!("rtrash-glob-{}", std::process::id()));
        for d in [
            "a/target/debug",
            "a/b/c/target",
            ".hidden/target",
            "a/target/x/target",
            "deep/1/2/3/4/5/target",
        ] {
            std::fs::create_dir_all(t.join(d)).unwrap();
        }
        std::fs::write(t.join("a/.lock"), b"").unwrap();
        let got = glob(&format!("{}/**/target", t.display()), STAR_DEPTH);
        let rel: Vec<PathBuf> = got
            .iter()
            .map(|p| p.strip_prefix(&t).unwrap().to_path_buf())
            .collect();
        let want: Vec<PathBuf> = [&["a", "b", "c", "target"][..], &["a", "target"]]
            .iter()
            .map(|c| c.iter().collect())
            .collect();
        assert_eq!(rel, want);
        let got = glob(&format!("{}/a/*", t.display()), STAR_DEPTH);
        assert_eq!(glob(&format!("{}/**/target", t.display()), 6).len(), 3);
        assert!(got.iter().all(|p| !p.ends_with(".lock")));
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn secrets_are_recognised() {
        assert!(secret_name(".credentials.json"));
        assert!(secret_name("history.jsonl"));
        assert!(!secret_name("snapshot-zsh-1.sh"));
    }
}
