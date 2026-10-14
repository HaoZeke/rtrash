//! `rtrash reap`: trash stale entries in temporary directories, as
//! systemd-tmpfiles and tmpreaper age them, but never one a live process
//! holds.
//!
//! An entry is each name directly inside a reaped directory, whole.
//! Its age is that of the newest time anywhere in it.
//! Like tmpfiles it reads atime, mtime and ctime, but not a directory's
//! atime, which listing it updates.
//! Like tmpreaper it leaves sockets, pipes and devices, and like tmpwatch
//! `--fuser` it leaves anything open; it also leaves mount points, other
//! users' entries unless run as root, lock files that name a live process,
//! and paths that tmpfiles.d `x` and `X` lines exclude.

use std::fs;
use std::path::{Path, PathBuf};

use crate::holders::{alive, Snapshot};
use crate::reap_profiles::{self, secret_name, Action};
use crate::util::{json_escape, str_from_os_ref};

const HELP: &str = "\
Usage: {prog} [OPTION]... [DIR]...
Trash the entries in DIR that nobody has touched for --older-than and that
no live process holds. Without DIR or --profile: $TMPDIR, /tmp and /var/tmp.

  -p, --profile=NAME    also reap a profile's caches and temp files, each
                          with its own age and action (repeatable); groups:
                          ai, compilers, pkg
      --list-profiles   list the profiles (with -v, their paths)
      --roots=DIR[:DIR]  directories a profile's {roots} globs search, such
                          as /workspace for cargo-target (repeatable; also
                          RTRASH_REAP_ROOTS and the profile's roots key)
      --depth=N         how deep ** reaches below home or a root (default 4)
      --tool            hand a profile to its tool's own cleanup (such as
                          go clean -cache) when the tool is on PATH
      --older-than=AGE  only entries whose newest time is older than AGE:
                          a number with s, m, h, d or w (days without one);
                          for DIR the age its tmpfiles.d line gives, else
                          10d; for a profile, the profile's age
      --age-by=LETTERS  which times count, from a (access), c (change) and
                          m (modification); default acm; a directory's
                          access time never counts
  -n, --dry-run         show what would go and each profile's total
      --permanent       delete instead of moving to the trash
      --trash           move to the trash, even for a permanent profile
      --trash-dir=PATH  put into this trash root instead of the usual one
      --home-only       always use the home trash
  -v, --verbose         also show each skipped entry and why
      --shared          also show what a delete would free: the bytes in
                          extents no other file shares, and the extents
                          shared by reflinks or snapshots, counted once
      --install-timer[=EVERY]  write a user systemd service and timer
                          (Linux), a launchd agent (macOS) or a Task
                          Scheduler task (Windows) that runs this reap,
                          with the other options, every EVERY (default
                          1h); with -n print them instead
      --watch[=EVERY]   stay running and reap again every EVERY (default
                          1m, at least 1s); needs --when-below, so a pass
                          does nothing while space is fine; between full
                          passes a tick checks the last pass's file
                          systems (one statfs each) and skips while they
                          are above the target, with a full pass at least
                          every 10 ticks; stops on Ctrl-C
                          with --watch, a service that stays running
                          instead (Linux and macOS)
      --remove-timer    stop and remove the job --install-timer made; with
                          -n print what that would run and remove
      --timer-status    say whether that job is installed, and its state
                          and last and next run (with --json, an object);
                          exits 1 when it is not
      --when-below=SPEC run only when a reaped file system has less free
                          than SPEC (a size such as 20G, a percent such
                          as 10%, and ,inodes=PCT), then reap oldest first
                          by profile, measuring again before each entry,
                          until it is back above it; exits at
                          once while space is fine, for a timer job
      --purge-trash     under --when-below, first empty the oldest items
                          of your trash (the Finder Trash first on macOS,
                          the drive's Recycle Bin on Windows) on a short
                          file system, since a put there frees nothing
      --until=SPEC      the target to reap back to under --when-below
                          (default: the --when-below SPEC)
      --json            print a JSON object ({dry_run, older_than,
                          profiles, reaped, skipped, failed})
      --help            display this help and exit
      --version         output version information and exit

An entry stays when a live process has it or anything in it open or mapped
or as its working directory, when it is or holds a socket, pipe, device or
mount point, when another user owns any of it (unless run as root), when
it is a lock or pid file naming a live process, when it holds
$XDG_RUNTIME_DIR, or when a tmpfiles.d x, X, r or R line covers it. A cargo
target directory also stays while cargo or rustc works in its project.
";

fn usage_err(prog: &str, msg: &str) -> i32 {
    eprintln!("{prog}: {msg}");
    eprintln!("Try '{prog} --help' for more information.");
    2
}

/// `90`, `90s`, `15m`, `2h`, `10d`, `1w` to seconds; a bare number is days.
pub fn parse_age(s: &str) -> Option<i64> {
    let s = s.trim();
    let (num, unit) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, "d"),
    };
    let n: i64 = num.parse().ok()?;
    let mul = match unit {
        "s" => 1,
        "m" | "min" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => return None,
    };
    n.checked_mul(mul)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AgeBy {
    atime: bool,
    ctime: bool,
    mtime: bool,
}

impl AgeBy {
    fn parse(s: &str) -> Option<AgeBy> {
        let mut a = AgeBy {
            atime: false,
            ctime: false,
            mtime: false,
        };
        for c in s.chars() {
            match c {
                'a' => a.atime = true,
                'c' => a.ctime = true,
                'm' => a.mtime = true,
                _ => return None,
            }
        }
        (a.atime || a.ctime || a.mtime).then_some(a)
    }

    /// The newest counted time of one inode, in seconds since the epoch.
    fn newest(&self, m: &fs::Metadata) -> i64 {
        let mut t = i64::MIN;
        if self.atime && !m.is_dir() {
            t = t.max(plat::atime(m));
        }
        if self.ctime {
            t = t.max(plat::ctime(m));
        }
        if self.mtime {
            t = t.max(plat::mtime(m));
        }
        if t == i64::MIN {
            // A directory aged by atime alone: fall back to its mtime.
            t = plat::mtime(m);
        }
        t
    }
}

/// The tmpfiles.d files reap reads, each with whether it came from a
/// user directory, in the order systemd-tmpfiles reads them. Within each
/// set (the user's, then the system's) a file masks any same-named file
/// in a later directory, and the files are taken in name order.
fn tmpfiles_confs() -> Vec<(PathBuf, bool)> {
    let env = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let home = crate::util::home_dir();
    let under_home = |rel: &str| home.as_ref().map(|h| h.join(rel));
    let user_dirs: Vec<PathBuf> = [
        env("XDG_CONFIG_HOME").or_else(|| under_home(".config")),
        env("XDG_RUNTIME_DIR"),
        env("XDG_DATA_HOME").or_else(|| under_home(".local/share")),
        Some(PathBuf::from("/etc/xdg")),
        Some(PathBuf::from("/usr/local/share")),
        Some(PathBuf::from("/usr/share")),
    ]
    .into_iter()
    .flatten()
    .map(|d| d.join("user-tmpfiles.d"))
    .collect();
    let system_dirs: Vec<PathBuf> = [
        "/etc/tmpfiles.d",
        "/run/tmpfiles.d",
        "/usr/local/lib/tmpfiles.d",
        "/usr/lib/tmpfiles.d",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    let mut out = Vec::new();
    for (dirs, user) in [(user_dirs, true), (system_dirs, false)] {
        let mut files: Vec<(std::ffi::OsString, PathBuf)> = Vec::new();
        for dir in dirs {
            let Ok(rd) = fs::read_dir(&dir) else {
                continue;
            };
            for p in rd.flatten().map(|e| e.path()) {
                let Some(name) = p.file_name().map(|n| n.to_os_string()) else {
                    continue;
                };
                if p.extension().and_then(|x| x.to_str()) == Some("conf")
                    && !files.iter().any(|(n, _)| *n == name)
                {
                    files.push((name, p));
                }
            }
        }
        files.sort();
        // A conf file is small; a huge one is not a conf file.
        out.extend(
            files
                .into_iter()
                .map(|(_, p)| p)
                .filter(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() <= 1 << 20))
                .map(|p| (p, user)),
        );
    }
    out
}

/// The value of tmpfiles.d specifier `c`, as the user's manager (`user`)
/// or the system's would give it. `None` for one reap cannot know.
fn specifier(c: char, user: bool) -> Option<String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let home = || crate::util::home_dir().map(|h| h.to_string_lossy().into_owned());
    let state = || env("XDG_STATE_HOME").or_else(|| home().map(|h| format!("{h}/.local/state")));
    Some(match (c, user) {
        ('%', _) => "%".into(),
        ('h', true) => home()?,
        ('h', false) => "/root".into(),
        ('u', true) => env("USER").or_else(|| env("LOGNAME"))?,
        ('u', false) => "root".into(),
        #[cfg(unix)]
        ('U', true) => unsafe { libc::getuid() }.to_string(),
        ('U', false) => "0".into(),
        ('t', true) => env("XDG_RUNTIME_DIR")?,
        ('t', false) => "/run".into(),
        ('T', true) => env("TMPDIR").unwrap_or_else(|| "/tmp".into()),
        ('T', false) => "/tmp".into(),
        ('V', true) => env("TMPDIR").unwrap_or_else(|| "/var/tmp".into()),
        ('V', false) => "/var/tmp".into(),
        ('C', true) => env("XDG_CACHE_HOME").or_else(|| home().map(|h| format!("{h}/.cache")))?,
        ('C', false) => "/var/cache".into(),
        ('S', true) => state()?,
        ('S', false) => "/var/lib".into(),
        ('L', true) => format!("{}/log", state()?),
        ('L', false) => "/var/log".into(),
        _ => return None,
    })
}

/// `path` with its specifiers filled in; one reap cannot know becomes
/// `unknown` (or the whole path `None` when `unknown` is `None`).
fn expand_specifiers(path: &str, user: bool, unknown: Option<&str>) -> Option<String> {
    let mut out = String::new();
    let mut chars = path.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            None => out.push('%'),
            Some(k) => match specifier(k, user) {
                Some(v) => out.push_str(&v),
                None => out.push_str(unknown?),
            },
        }
    }
    Some(out)
}

/// tmpfiles.d paths to leave alone: the path field of every `x` and `X`
/// line and of every `r` and `R` removal line, user and system, with specifiers filled in where reap knows them
/// and widened to `*` where it does not.
pub fn tmpfiles_exclusions() -> Vec<String> {
    let mut out = Vec::new();
    for (p, user) in tmpfiles_confs() {
        if let Ok(body) = fs::read_to_string(&p) {
            out.extend(parse_tmpfiles(&body, user));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// A free space target: `--when-below` and `--until`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Floor {
    pub bytes: Option<u64>,
    pub pct: Option<u64>,
    pub inodes_pct: Option<u64>,
}

impl Floor {
    /// `10G`, `15%`, either with `,inodes=PCT`, or `inodes=PCT` alone.
    pub fn parse(spec: &str) -> Option<Floor> {
        let mut f = Floor::default();
        for part in spec.split(',') {
            let part = part.trim();
            if let Some(v) = part.strip_prefix("inodes=") {
                f.inodes_pct = Some(parse_pct(v)?);
            } else if part.ends_with('%') {
                f.pct = Some(parse_pct(part)?);
            } else {
                f.bytes = Some(crate::filters::parse_size(part)?);
            }
        }
        (f != Floor::default()).then_some(f)
    }

    /// Whether `s` is under any part of the target.
    pub fn below(&self, s: &crate::space::Space) -> bool {
        let (b, i) = self.need(s);
        b > 0 || i > 0
    }

    /// Bytes and inodes to free for `s` to meet every part of the target.
    pub fn need(&self, s: &crate::space::Space) -> (u64, u64) {
        let pct_of = |p: u64, total: u64| (u128::from(total) * u128::from(p) / 100) as u64;
        let mut want = self.bytes.unwrap_or(0);
        if let Some(p) = self.pct {
            want = want.max(pct_of(p, s.total));
        }
        let inodes = match (self.inodes_pct, s.inodes, s.inodes_avail) {
            (Some(p), Some(t), Some(a)) => pct_of(p, t).saturating_sub(a),
            _ => 0,
        };
        (want.saturating_sub(s.avail), inodes)
    }
}

fn parse_pct(v: &str) -> Option<u64> {
    let n: u64 = v.strip_suffix('%').unwrap_or(v).trim().parse().ok()?;
    (n <= 100).then_some(n)
}

/// How many of `sizes` (bytes, inodes), taken in order, free `need`; all
/// of them when they can't.
fn enough(sizes: &[(u64, u64)], need: (u64, u64)) -> usize {
    let (mut b, mut i) = (0u64, 0u64);
    for (n, (sb, si)) in sizes.iter().enumerate() {
        if b >= need.0 && i >= need.1 {
            return n;
        }
        b = b.saturating_add(*sb);
        i = i.saturating_add(*si);
    }
    sizes.len()
}

/// The tmpfiles.d age of each path a `d`, `D`, `e`, `q`, `Q` or `v` line
/// cleans, in seconds, from the user's files and then the system's (see
/// [`tmpfiles_confs`]); the first line for a path wins, as
/// systemd-tmpfiles reads them.
pub fn tmpfiles_ages() -> Vec<(PathBuf, TmpAge)> {
    let mut out: Vec<(PathBuf, TmpAge)> = Vec::new();
    for (p, user) in tmpfiles_confs() {
        if let Ok(body) = fs::read_to_string(&p) {
            for (path, age) in parse_tmpfiles_ages(&body, user) {
                if !out.iter().any(|(x, _)| *x == path) {
                    out.push((path, age));
                }
            }
        }
    }
    out
}

/// The `(path, age)` pairs of a tmpfiles.d body's cleaning lines, with
/// specifiers filled in for a user file (`user`) or a system one. Paths
/// with a specifier reap cannot know, and ages that are `-`, zero or
/// unreadable, are left out.
fn parse_tmpfiles_ages(body: &str, user: bool) -> Vec<(PathBuf, TmpAge)> {
    let mut out = Vec::new();
    for line in body.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 6 || f[0].starts_with('#') {
            continue;
        }
        let kind = f[0].trim_end_matches(['!', '-', '=', '~', '^', '+', '?']);
        if !matches!(kind, "d" | "D" | "e" | "q" | "Q" | "v") {
            continue;
        }
        let Some(path) = expand_specifiers(f[1], user, None) else {
            continue;
        };
        if let Some(age) = parse_tmp_age(f[5]) {
            out.push((PathBuf::from(path), age));
        }
    }
    out
}

/// One tmpfiles.d age field: the span, which times count, and whether a
/// leading `~` moves cleaning one level down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TmpAge {
    pub secs: i64,
    /// From a `LETTERS:` prefix; `None` counts every time.
    by: Option<AgeBy>,
    /// `~`: only entries inside the directory's subdirectories go, never
    /// the entries directly inside it.
    pub below: bool,
}

/// Read an age field such as `10d`, `~1w`, `cmA:2h` or `~mM:1d`. The
/// letters are tmpfiles.d's: a, c and m for files, A, C and M for
/// directories, b and B for birth time; reap counts a time when either
/// case names it and has no birth time, so a prefix of only b and B counts
/// every time. `None` for `-`, zero or an unreadable span.
fn parse_tmp_age(field: &str) -> Option<TmpAge> {
    let mut rest = field;
    let mut below = false;
    if let Some(r) = rest.strip_prefix('~') {
        below = true;
        rest = r;
    }
    let mut by = None;
    if let Some((letters, span)) = rest.split_once(':') {
        if !letters.chars().all(|c| "aAbBcCmM".contains(c)) {
            return None;
        }
        let has = |l: char| letters.contains(l) || letters.contains(l.to_ascii_uppercase());
        let a = AgeBy {
            atime: has('a'),
            ctime: has('c'),
            mtime: has('m'),
        };
        by = (a.atime || a.ctime || a.mtime).then_some(a);
        rest = span;
    }
    if let Some(r) = rest.strip_prefix('~') {
        below = true;
        rest = r;
    }
    let secs = parse_span(rest).filter(|&s| s > 0)?;
    Some(TmpAge { secs, by, below })
}

/// A systemd time span such as `10d`, `1w 2d`, `12h30min` or a bare
/// number of seconds.
fn parse_span(s: &str) -> Option<i64> {
    let mut total = 0i64;
    let mut rest = s.trim();
    if rest.is_empty() || rest == "-" {
        return None;
    }
    while !rest.is_empty() {
        let n_end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        let n: i64 = rest[..n_end].parse().ok()?;
        rest = rest[n_end..].trim_start();
        let u_end = rest
            .find(|c: char| c.is_ascii_digit() || c == ' ')
            .unwrap_or(rest.len());
        let mul = match &rest[..u_end] {
            "" | "s" | "sec" | "second" | "seconds" => 1,
            "m" | "min" | "minute" | "minutes" => 60,
            "h" | "hr" | "hour" | "hours" => 3600,
            "d" | "day" | "days" => 86_400,
            "w" | "week" | "weeks" => 7 * 86_400,
            "M" | "month" | "months" => 2_629_800,
            "y" | "year" | "years" => 31_557_600,
            "ms" | "us" | "usec" | "msec" => 0,
            _ => return None,
        };
        total = total.checked_add(n.checked_mul(mul)?)?;
        rest = rest[u_end..].trim_start();
    }
    Some(total)
}

/// The tmpfiles.d age for `root`: a line for that path (also after
/// resolving links), else the first glob line that matches it with as
/// many components, as systemd-tmpfiles applies a glob to each match.
fn tmpfiles_age_for(root: &Path, ages: &[(PathBuf, TmpAge)]) -> Option<TmpAge> {
    let exact = ages
        .iter()
        .find(|(p, _)| p == root || fs::canonicalize(p).is_ok_and(|c| c == root));
    if let Some((_, a)) = exact {
        return Some(*a);
    }
    let text = str_from_os_ref(root.as_os_str());
    let depth = root.components().count();
    ages.iter()
        .find(|(p, _)| {
            let pat = p.to_string_lossy();
            pat.contains(['*', '?', '['])
                && p.components().count() == depth
                && crate::util::fnmatch(&pat, &text)
        })
        .map(|(_, a)| *a)
}

fn parse_tmpfiles(body: &str, user: bool) -> Vec<String> {
    let mut out = Vec::new();
    for line in body.lines() {
        let mut f = line.split_whitespace();
        let (Some(kind), Some(path)) = (f.next(), f.next()) else {
            continue;
        };
        if kind.starts_with('#') {
            continue;
        }
        let kind = kind.trim_end_matches(['!', '-', '=', '~', '^', '+', '?']);
        // r and R lines leave the removal to systemd-tmpfiles at boot.
        if !matches!(kind, "x" | "X" | "r" | "R") {
            continue;
        }
        if let Some(pat) = expand_specifiers(path, user, Some("*")) {
            out.push(pat);
        }
    }
    out
}

/// `NAME-VERSION` split at the last `-`, or `NAMEVERSION` where the name
/// ends in a letter (`IntelliJIdea2024.3`), with VERSION digits in
/// dot-separated parts (`1148`, `131.0.6778.85`), compared part by part.
fn name_version(p: &Path) -> Option<(String, Vec<u64>)> {
    let name = p.file_name()?.to_str()?;
    let parse = |v: &str| -> Option<Vec<u64>> { v.split('.').map(|x| x.parse().ok()).collect() };
    if let Some((n, v)) = name.rsplit_once('-') {
        if let Some(parts) = parse(v) {
            return Some((n.to_string(), parts));
        }
    }
    let cut = name.rfind(|c: char| !(c.is_ascii_digit() || c == '.'))? + 1;
    let (n, v) = name.split_at(cut);
    if !n.ends_with(|c: char| c.is_ascii_alphabetic()) || v.is_empty() {
        return None;
    }
    Some((n.to_string(), parse(v)?))
}

/// Drop the entries not named `NAME-VERSION` and the highest VERSION of
/// each NAME, leaving the older versions to reap.
fn spare_newest_versions(entries: &mut Vec<(PathBuf, u64)>) {
    let mut top: Vec<(PathBuf, String, Vec<u64>)> = Vec::new();
    for (e, _) in entries.iter() {
        let Some((n, v)) = name_version(e) else {
            continue;
        };
        let dir = e.parent().map(Path::to_path_buf).unwrap_or_default();
        match top.iter_mut().find(|(d, m, _)| *d == dir && *m == n) {
            Some(t) => {
                if v > t.2 {
                    t.2 = v;
                }
            }
            None => top.push((dir, n, v)),
        }
    }
    entries.retain(|(e, _)| match name_version(e) {
        None => false,
        Some((n, v)) => {
            let dir = e.parent().map(Path::to_path_buf).unwrap_or_default();
            top.iter().any(|(d, m, t)| *d == dir && *m == n && v < *t)
        }
    });
}

/// Does an exclusion cover `path` or something inside it?
fn excluded(path: &Path, pats: &[String]) -> Option<String> {
    let text = str_from_os_ref(path.as_os_str());
    let depth = text.matches('/').count();
    for pat in pats {
        if crate::util::fnmatch(pat, &text) {
            return Some(pat.clone());
        }
        // A deeper pattern whose leading components match reaches inside.
        let comps: Vec<&str> = pat.split('/').collect();
        if comps.len() > depth + 1 && crate::util::fnmatch(&comps[..=depth].join("/"), &text) {
            return Some(pat.clone());
        }
    }
    None
}

/// What is known about one entry after walking it.
struct Walk {
    newest: i64,
    bytes: u64,
    /// Files and directories in the entry, itself included.
    inodes: u64,
    /// Why it must stay, found during the walk.
    keep: Option<String>,
}

fn kind_reason(ft: &fs::FileType) -> Option<&'static str> {
    plat::kind_reason(ft)
}

/// Walk an entry for its newest time and size, stopping at the first
/// reason to keep it; a time newer than `cutoff` is such a reason, so a
/// busy tree is never walked in full.
fn walk(entry: &Path, top: &fs::Metadata, dev: u64, ctx: &Ctx, g: &Group) -> Walk {
    let mut w = Walk {
        newest: g.age_by.unwrap_or(ctx.age_by).newest(top),
        bytes: plat::bytes(top),
        inodes: 1,
        keep: None,
    };
    if !top.is_dir() || w.newest > g.cutoff {
        return w;
    }
    let mut stack = vec![entry.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                w.keep = Some(format!("cannot read '{}': {e}", dir.display()));
                return w;
            }
        };
        for e in rd {
            let e = match e {
                Ok(e) => e,
                Err(e) => {
                    w.keep = Some(format!("cannot read '{}': {e}", dir.display()));
                    return w;
                }
            };
            let p = e.path();
            let m = match fs::symlink_metadata(&p) {
                Ok(m) => m,
                Err(e) => {
                    w.keep = Some(format!("cannot read '{}': {e}", p.display()));
                    return w;
                }
            };
            if plat::dev(&m) != dev {
                w.keep = Some(format!("holds the mount point '{}'", p.display()));
                return w;
            }
            if let Some(k) = kind_reason(&m.file_type()) {
                w.keep = Some(format!("holds {k} '{}'", p.display()));
                return w;
            }
            if ctx.euid != 0 && plat::uid(&m) != ctx.euid {
                w.keep = Some(format!("holds '{}' of user {}", p.display(), plat::uid(&m)));
                return w;
            }
            if g.guard && secret_name(&e.file_name().to_string_lossy()) {
                w.keep = Some(format!(
                    "holds '{}', which looks like settings, history or a credential",
                    p.display()
                ));
                return w;
            }
            w.newest = w.newest.max(g.age_by.unwrap_or(ctx.age_by).newest(&m));
            if w.newest > g.cutoff {
                return w;
            }
            w.bytes += plat::bytes(&m);
            w.inodes += 1;
            if m.is_dir() {
                stack.push(p);
            }
        }
    }
    w
}

/// A small lock or pid file whose first word is a live process id.
fn live_lock(path: &Path, m: &fs::Metadata) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    let lockish = name.ends_with(".lock")
        || name.ends_with(".pid")
        || name.ends_with("-lock")
        || name.ends_with(".lck");
    if !lockish || !m.is_file() || m.len() > 64 {
        return None;
    }
    let body = fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&body);
    let pid: u32 = text.split_whitespace().next()?.parse().ok()?;
    alive(pid).then_some(pid)
}

/// One entry and what reap made of it.
struct Outcome {
    path: PathBuf,
    group: usize,
    age: i64,
    bytes: u64,
    inodes: u64,
    /// Device of the directory the entry sits in.
    dev: u64,
    skip: Option<String>,
    /// Bytes only this entry uses (`--shared`).
    unique: Option<u64>,
}

/// The entries of one profile, or of the plain directories (`tmp`).
struct Group {
    name: String,
    action: Action,
    older: i64,
    /// Which times count here, when a tmpfiles.d line says; else
    /// `Ctx::age_by`.
    age_by: Option<AgeBy>,
    cutoff: i64,
    marker: Option<String>,
    busy: Vec<String>,
    guard: bool,
    /// The tool command `--tool` runs instead of the pass, when on PATH.
    tool: Option<Vec<String>>,
    /// Entry and the device of the directory it sits in.
    entries: Vec<(PathBuf, u64)>,
}

/// What every judgement needs.
struct Ctx {
    now: i64,
    euid: u32,
    age_by: AgeBy,
    snap: Snapshot,
    excl: Vec<String>,
    runtime: Option<PathBuf>,
    home: Option<PathBuf>,
}

fn judge(path: &Path, parent_dev: u64, gi: usize, g: &Group, ctx: &Ctx) -> Option<Outcome> {
    let m = fs::symlink_metadata(path).ok()?;
    if let Some(marker) = &g.marker {
        // Not the kind of entry this profile is about.
        if !m.is_dir() || fs::symlink_metadata(path.join(marker)).is_err() {
            return None;
        }
    }
    let mut o = Outcome {
        path: path.to_path_buf(),
        group: gi,
        age: ctx.now - g.age_by.unwrap_or(ctx.age_by).newest(&m),
        bytes: 0,
        inodes: 1,
        dev: parent_dev,
        skip: None,
        unique: None,
    };
    let name = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    o.skip = if path.parent().is_none() || ctx.home.as_ref().is_some_and(|h| h.starts_with(path)) {
        Some("holds your home directory".to_string())
    } else if name.starts_with(".Trash") {
        Some("a trash directory".to_string())
    } else if ctx.runtime.as_ref().is_some_and(|r| r.starts_with(path)) {
        Some("the XDG runtime directory".to_string())
    } else if let Some(pat) = excluded(path, &ctx.excl) {
        Some(format!("excluded by tmpfiles.d '{pat}'"))
    } else if plat::fs_key(path, &m) != parent_dev {
        Some("a mount point".to_string())
    } else if let Some(k) = kind_reason(&m.file_type()) {
        Some(k.to_string())
    } else if ctx.euid != 0 && plat::uid(&m) != ctx.euid {
        Some(format!("owned by user {}", plat::uid(&m)))
    } else if g.guard && secret_name(&name) {
        Some("looks like settings, history or a credential".to_string())
    } else {
        None
    };
    if o.skip.is_some() {
        return Some(o);
    }
    // Below the entry, a mount point is any change of device.
    let w = walk(path, &m, plat::dev(&m), ctx, g);
    o.age = ctx.now - w.newest;
    o.bytes = w.bytes;
    o.inodes = w.inodes;
    o.skip = if let Some(k) = w.keep {
        Some(k)
    } else if w.newest > g.cutoff {
        Some(format!("touched {} ago", fmt_age(ctx.now - w.newest)))
    } else if let Some(pid) = live_lock(path, &m) {
        Some(format!("lock file of live pid {pid}"))
    } else if let Some(h) = ctx.snap.under(path).first() {
        let more = ctx.snap.under(path).iter().any(|x| x.pid != h.pid);
        let more = if more { " and others" } else { "" };
        Some(format!(
            "in use by pid {} ({}, {}){more}",
            h.pid, h.command, h.how
        ))
    } else {
        busy_holder(path, g, ctx)
    };
    Some(o)
}

/// A `busy` command (cargo, rustc) at work in the entry's parent, such
/// as the project a target directory belongs to.
fn busy_holder(path: &Path, g: &Group, ctx: &Ctx) -> Option<String> {
    if g.busy.is_empty() {
        return None;
    }
    let parent = path.parent()?;
    ctx.snap
        .under(parent)
        .into_iter()
        .find(|h| g.busy.iter().any(|b| h.is(b)))
        .map(|h| {
            format!(
                "'{}' is busy with pid {} ({}, {})",
                parent.display(),
                h.pid,
                h.command,
                h.how
            )
        })
}

/// Is `c` (canonical) the home directory?
fn is_home(c: &Path) -> bool {
    crate::util::home_dir()
        .and_then(|h| fs::canonicalize(h).ok())
        .is_some_and(|h| h == c)
}

fn default_dirs() -> Vec<PathBuf> {
    if cfg!(windows) {
        return vec![std::env::temp_dir()];
    }
    let mut v = Vec::new();
    if let Some(t) = std::env::var_os("TMPDIR") {
        let t = PathBuf::from(t);
        if t.is_absolute() {
            v.push(t);
        }
    }
    v.push(PathBuf::from("/tmp"));
    v.push(PathBuf::from("/var/tmp"));
    v
}

/// What differs between Unix and Windows: metadata, deleting and trashing.
#[cfg(unix)]
mod plat {
    use std::fs;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    pub fn dev(m: &fs::Metadata) -> u64 {
        m.dev()
    }
    /// Which file system `--when-below` measures for `_path`.
    pub fn fs_key(_path: &Path, m: &fs::Metadata) -> u64 {
        m.dev()
    }
    pub fn uid(m: &fs::Metadata) -> u32 {
        m.uid()
    }
    pub fn bytes(m: &fs::Metadata) -> u64 {
        m.blocks() * 512
    }
    /// False for a further name of a file already counted.
    pub fn first_link(m: &fs::Metadata, seen: &mut std::collections::HashSet<(u64, u64)>) -> bool {
        m.is_dir() || m.nlink() < 2 || seen.insert((m.dev(), m.ino()))
    }
    /// A file's bytes no other file shares, from its extent map when the
    /// probe finds that worth asking for.
    pub fn file_unique(
        p: &Path,
        m: &fs::Metadata,
        disk: u64,
        probe: &crate::du_extent::Probe,
        set: &mut crate::du_extent::SharedSet,
    ) -> u64 {
        if !m.is_file() {
            return disk;
        }
        match probe.wants(m.dev(), disk, || fs::File::open(p).ok()) {
            Some(f) => {
                let s = crate::du_extent::file_share(&f, disk);
                set.add(m.dev(), &s.shared);
                s.unique
            }
            None => disk,
        }
    }
    pub fn atime(m: &fs::Metadata) -> i64 {
        m.atime()
    }
    pub fn mtime(m: &fs::Metadata) -> i64 {
        m.mtime()
    }
    pub fn ctime(m: &fs::Metadata) -> i64 {
        m.ctime()
    }
    pub fn euid() -> u32 {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() }
    }
    pub fn kind_reason(ft: &fs::FileType) -> Option<&'static str> {
        if ft.is_socket() {
            Some("a socket")
        } else if ft.is_fifo() {
            Some("a named pipe")
        } else if ft.is_block_device() || ft.is_char_device() {
            Some("a device")
        } else {
            None
        }
    }
    pub fn executable(p: &Path) -> bool {
        fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }

    /// Where trashed entries go.
    #[derive(Default)]
    pub struct Dest {
        pub home_only: bool,
        pub trash_pin: Option<PathBuf>,
    }

    pub fn remove(p: &Path) -> std::io::Result<()> {
        crate::fastdelete::remove_path(p)
    }

    pub fn trash(prog: &str, p: &Path, dest: &Dest) -> Result<(), ()> {
        let opts = crate::put::Opts {
            force: true,
            recursive: true,
            quiet: true,
            home_only: dest.home_only,
            trash_pin: dest.trash_pin.clone(),
            ..crate::put::Opts::default()
        };
        match crate::put::put_one(prog, p, &opts) {
            Ok(Some(_)) => Ok(()),
            Ok(None) | Err(_) => Err(()),
        }
    }
}

/// Windows: no devices to cross, no owner check, and links and junctions
/// stay; trashing goes to the Recycle Bin.
#[cfg(windows)]
mod plat {
    use std::fs;
    use std::os::windows::fs::MetadataExt;
    use std::path::{Path, PathBuf};

    /// FILETIME (100 ns since 1601) to Unix seconds.
    fn secs(ft: u64) -> i64 {
        (ft / 10_000_000) as i64 - 11_644_473_600
    }
    pub fn dev(_m: &fs::Metadata) -> u64 {
        0
    }
    /// Which volume `--when-below` measures for `path`: its serial
    /// number (else its letter or network share), since Windows metadata
    /// carries no device number.
    pub fn fs_key(path: &Path, _m: &fs::Metadata) -> u64 {
        crate::win_recycle::volume_key(path)
    }
    pub fn uid(_m: &fs::Metadata) -> u32 {
        0
    }
    pub fn bytes(m: &fs::Metadata) -> u64 {
        m.len()
    }
    /// Windows hard links are rare enough to count each name.
    pub fn first_link(
        _m: &fs::Metadata,
        _seen: &mut std::collections::HashSet<(u64, u64)>,
    ) -> bool {
        true
    }
    /// Windows gives no extent map here, so a file counts in full.
    pub fn file_unique(
        _p: &Path,
        _m: &fs::Metadata,
        disk: u64,
        _probe: &crate::du_extent::Probe,
        _set: &mut crate::du_extent::SharedSet,
    ) -> u64 {
        disk
    }
    pub fn atime(m: &fs::Metadata) -> i64 {
        secs(m.last_access_time())
    }
    pub fn mtime(m: &fs::Metadata) -> i64 {
        secs(m.last_write_time())
    }
    /// Windows keeps no change time; creation counts instead, which is
    /// new for a file copied in.
    pub fn ctime(m: &fs::Metadata) -> i64 {
        secs(m.creation_time())
    }
    /// Owner checks are skipped: a profile's paths are this user's.
    pub fn euid() -> u32 {
        0
    }
    pub fn kind_reason(ft: &fs::FileType) -> Option<&'static str> {
        ft.is_symlink().then_some("a link or junction")
    }
    pub fn executable(p: &Path) -> bool {
        if p.is_file() {
            return true;
        }
        let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        exts.split(';').filter(|e| !e.is_empty()).any(|e| {
            let mut s = p.as_os_str().to_owned();
            s.push(e);
            Path::new(&s).is_file()
        })
    }

    /// Where trashed entries go: always the Recycle Bin of their drive.
    #[derive(Default)]
    pub struct Dest {
        pub home_only: bool,
        pub trash_pin: Option<PathBuf>,
    }

    pub fn remove(p: &Path) -> std::io::Result<()> {
        if fs::symlink_metadata(p)?.is_dir() {
            fs::remove_dir_all(p)
        } else {
            fs::remove_file(p)
        }
    }

    pub fn trash(prog: &str, p: &Path, dest: &Dest) -> Result<(), ()> {
        let _ = (dest.home_only, &dest.trash_pin);
        trash::delete(p).map_err(|e| eprintln!("{prog}: cannot trash '{}': {e}", p.display()))
    }
}

fn fmt_age(s: i64) -> String {
    let s = s.max(0);
    if s >= 86_400 {
        format!("{}d", s / 86_400)
    } else if s >= 3600 {
        format!("{}h", s / 3600)
    } else if s >= 60 {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

/// Is `cmd` an executable on PATH (or a path to one)?
fn on_path(cmd: &str) -> bool {
    if cmd.contains(['/', '\\']) {
        return plat::executable(Path::new(cmd));
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|d| plat::executable(&d.join(cmd))))
}

fn list_profiles(prog: &str, json: bool, verbose: bool, roots_cli: &[PathBuf]) -> i32 {
    let mut warns = Vec::new();
    let all = reap_profiles::all(crate::config::get(), &mut warns);
    for w in &warns {
        eprintln!("{prog}: warning: {w}");
    }
    if json {
        let items: Vec<String> = all
            .iter()
            .map(|p| {
                let list = |v: &[String]| {
                    v.iter()
                        .map(|s| format!("\"{}\"", json_escape(s)))
                        .collect::<Vec<_>>()
                        .join(",")
                };
                format!(
                    r#"{{"name":"{}","about":"{}","age":{},"action":"{}","paths":[{}],"include":[{}],"tool":[{}],"source":"{}"}}"#,
                    json_escape(&p.name),
                    json_escape(&p.about),
                    p.age,
                    p.action.as_str(),
                    list(&p.paths),
                    list(&p.include),
                    list(&p.tool),
                    if p.from_config { "config" } else { "builtin" }
                )
            })
            .collect();
        println!("[{}]", items.join(","));
        return 0;
    }
    for p in &all {
        let what = if p.include.is_empty() {
            format!("{:>4} {:<9}", fmt_age(p.age), p.action.as_str())
        } else {
            format!("{:<14}", "group")
        };
        let src = if p.from_config { " (config)" } else { "" };
        println!("{:<15} {what} {}{src}", p.name, p.about);
        if verbose {
            for inc in &p.include {
                println!("    + {inc}");
            }
            let proots = reap_profiles::roots(roots_cli, p);
            for e in p
                .paths
                .iter()
                .flat_map(|path| reap_profiles::expand_all(path, &proots))
            {
                println!("    {e}");
            }
            if !p.tool.is_empty() {
                println!("    --tool: {}", p.tool.join(" "));
            }
        }
    }
    0
}

/// Add the directories of one `--roots` value, a path list like PATH.
fn add_roots(out: &mut Vec<PathBuf>, v: &str) -> Result<(), String> {
    for p in std::env::split_paths(v) {
        let c =
            fs::canonicalize(&p).map_err(|e| format!("cannot use root '{}': {e}", p.display()))?;
        if !out.contains(&c) {
            out.push(c);
        }
    }
    Ok(())
}

/// Set when `--purge-trash` emptied trash items, so the history line
/// says `purge` instead of `reap`.
static PURGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn purged(dry_run: bool, n: u64) {
    if !dry_run && n > 0 {
        PURGED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

pub fn run(prog: &str, args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--timer-status") {
        let json = args.iter().any(|a| a == "--json");
        if let Some(a) = args
            .iter()
            .find(|a| !matches!(a.as_str(), "--timer-status" | "--json"))
        {
            return usage_err(
                prog,
                &format!("--timer-status takes only --json, not '{a}'"),
            );
        }
        return crate::reap_timer::status(prog, json);
    }
    let timer = args.iter().any(|a| a.starts_with("--install-timer"));
    if let Some(i) = args
        .iter()
        .position(|a| a == "--watch" || a.starts_with("--watch="))
        .filter(|_| !timer)
    {
        return watch(prog, args, i);
    }
    #[cfg(windows)]
    let bins = crate::trash_history::bin_snapshot();
    #[cfg(unix)]
    let finder = crate::finder_total::snapshot();
    let code = run_inner(prog, args);
    let event = if PURGED.load(std::sync::atomic::Ordering::Relaxed) {
        "purge"
    } else {
        "reap"
    };
    #[cfg(unix)]
    crate::trash_total::log_touched(event);
    #[cfg(unix)]
    crate::finder_total::log(event, &finder);
    #[cfg(windows)]
    crate::trash_history::log_bins(event, &bins);
    code
}

/// The paths whose file systems the last `--when-below` pass measured.
static WATCHED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// A full pass at least every this many ticks, so entries on a file
/// system the last pass did not see are still found.
const FULL_EVERY: u64 = 10;

/// Whether a tick can skip the full pass: every file system the last pass
/// measured is still above `floor`, by one statfs each.
fn all_above(floor: &Floor) -> Option<usize> {
    let paths = WATCHED.lock().ok()?.clone();
    if paths.is_empty() {
        return None;
    }
    paths
        .iter()
        .all(|p| crate::space::query(p).is_ok_and(|s| !floor.below(&s)))
        .then_some(paths.len())
}

/// `--watch`: one reap pass every interval, for good. Between full passes
/// a tick first checks the file systems the last pass measured, one
/// statfs each, and skips the pass while all of them are above the
/// target. A debug build stops after `RTRASH_TEST_WATCH_ROUNDS` ticks so
/// tests can run it.
fn watch(prog: &str, args: &[String], i: usize) -> i32 {
    let every = match args[i].split_once('=') {
        None => 60,
        Some((_, v)) => match parse_age(v) {
            Some(s) if s >= 1 => s,
            _ => {
                return usage_err(
                    prog,
                    &format!("invalid interval '{v}' (at least 1s, such as 5m)"),
                )
            }
        },
    };
    let rest: Vec<String> = args
        .iter()
        .enumerate()
        .filter(|&(j, _)| j != i)
        .map(|(_, a)| a.clone())
        .collect();
    if !rest.iter().any(|a| a.starts_with("--when-below=")) {
        return usage_err(prog, "--watch goes with --when-below");
    }
    if let Some(a) = rest.iter().find(|a| {
        a.starts_with("--install-timer")
            || matches!(
                a.as_str(),
                "--remove-timer" | "--list-profiles" | "--help" | "--version" | "-n" | "--dry-run"
            )
    }) {
        return usage_err(prog, &format!("--watch does not go with '{a}'"));
    }
    let rounds: Option<u64> = if cfg!(debug_assertions) {
        std::env::var("RTRASH_TEST_WATCH_ROUNDS")
            .ok()
            .and_then(|v| v.parse().ok())
    } else {
        None
    };
    let floor = rest
        .iter()
        .find_map(|a| a.strip_prefix("--when-below="))
        .and_then(Floor::parse);
    let verbose = rest.iter().any(|a| a == "-v" || a == "--verbose");
    let mut done = 0u64;
    let mut since_full = 0u64;
    let mut code = 0;
    loop {
        let skip = (since_full > 0 && since_full < FULL_EVERY)
            .then(|| floor.as_ref().and_then(all_above))
            .flatten();
        if let Some(n) = skip {
            since_full += 1;
            if verbose {
                eprintln!(
                    "{prog}: free space is above the target on {}; skipping this pass",
                    crate::util::plural(n as u64, "file system", "file systems")
                );
            }
        } else {
            since_full = 1;
            code = code.max(run(prog, &rest));
        }
        if code == 2 {
            return code;
        }
        done += 1;
        if rounds.is_some_and(|r| done >= r) {
            return code;
        }
        std::thread::sleep(std::time::Duration::from_secs(every as u64));
    }
}

fn run_inner(prog: &str, args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--remove-timer") {
        let dry = args.iter().any(|a| a == "-n" || a == "--dry-run");
        if let Some(a) = args
            .iter()
            .find(|a| !matches!(a.as_str(), "--remove-timer" | "-n" | "--dry-run"))
        {
            return usage_err(prog, &format!("--remove-timer takes only -n, not '{a}'"));
        }
        return crate::reap_timer::remove(prog, dry);
    }
    if let Some(i) = args
        .iter()
        .position(|a| a == "--install-timer" || a.starts_with("--install-timer="))
    {
        let every = match args[i].split_once('=') {
            None => 3600,
            Some((_, v)) => match parse_age(v) {
                Some(s) if s >= 60 => s,
                _ => {
                    return usage_err(
                        prog,
                        &format!("invalid interval '{v}' (at least 1m, such as 1h)"),
                    )
                }
            },
        };
        let dry = args.iter().any(|a| a == "-n" || a == "--dry-run");
        let rest: Vec<String> = args
            .iter()
            .enumerate()
            .filter(|&(j, a)| j != i && a != "-n" && a != "--dry-run")
            .map(|(_, a)| a.clone())
            .collect();
        if rest
            .iter()
            .any(|a| a == "--list-profiles" || a == "--help" || a == "--version")
        {
            return usage_err(prog, "--install-timer takes the options of the reap to run");
        }
        if rest
            .iter()
            .any(|a| a == "--watch" || a.starts_with("--watch="))
            && !rest.iter().any(|a| a.starts_with("--when-below="))
        {
            return usage_err(prog, "--watch goes with --when-below");
        }
        return crate::reap_timer::install(prog, &rest, every, dry);
    }
    let mut older: Option<i64> = None;
    let mut age_by_set = false;
    let mut age_by = AgeBy {
        atime: true,
        ctime: true,
        mtime: true,
    };
    let mut dry_run = false;
    let mut action: Option<Action> = None;
    let mut verbose = false;
    let mut json = false;
    let mut shared = false;
    let mut purge_trash = false;
    let mut when_below: Option<Floor> = None;
    let mut until: Option<Floor> = None;
    let mut list = false;
    let mut use_tool = false;
    let mut profiles: Vec<String> = Vec::new();
    let mut dest = plat::Dest::default();
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut no_more = false;
    let mut want_profile = false;
    let mut want_roots = false;
    let mut roots_cli: Vec<PathBuf> = Vec::new();
    let mut depth: Option<usize> = None;
    for arg in args {
        if want_profile {
            profiles.push(arg.clone());
            want_profile = false;
            continue;
        }
        if want_roots {
            want_roots = false;
            match add_roots(&mut roots_cli, arg) {
                Ok(()) => continue,
                Err(e) => return usage_err(prog, &e),
            }
        }
        if no_more || !arg.starts_with('-') || arg == "-" {
            dirs.push(crate::util::arg_path(arg));
            continue;
        }
        match arg.as_str() {
            "--" => no_more = true,
            "-n" | "--dry-run" => dry_run = true,
            "-v" | "--verbose" => verbose = true,
            "--permanent" => action = Some(Action::Permanent),
            "--trash" => action = Some(Action::Trash),
            "--json" => json = true,
            "--shared" => shared = true,
            "--purge-trash" => purge_trash = true,
            a if a.starts_with("--when-below=") || a.starts_with("--until=") => {
                let (name, v) = a.split_once('=').unwrap_or((a, ""));
                let Some(f) = Floor::parse(v) else {
                    return usage_err(
                        prog,
                        &format!("invalid {name} '{v}' (a size such as 10G, a percent such as 10%, and ,inodes=PCT)"),
                    );
                };
                if name == "--until" {
                    until = Some(f);
                } else {
                    when_below = Some(f);
                }
            }
            "--home-only" => dest.home_only = true,
            "--list-profiles" => list = true,
            "--tool" => use_tool = true,
            "-p" | "--profile" => want_profile = true,
            "--roots" => want_roots = true,
            a if a.starts_with("--roots=") => {
                if let Err(e) = add_roots(&mut roots_cli, &a["--roots=".len()..]) {
                    return usage_err(prog, &e);
                }
            }
            a if a.starts_with("--depth=") => match a["--depth=".len()..].parse::<usize>() {
                Ok(d) if d <= 16 => depth = Some(d),
                _ => return usage_err(prog, "--depth takes a number from 0 to 16"),
            },
            "--help" => {
                print!("{}", HELP.replace("{prog}", prog));
                return 0;
            }
            "--version" => {
                println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                return 0;
            }
            a if a.starts_with("--profile=") => profiles.push(a["--profile=".len()..].to_string()),
            a if a.starts_with("--older-than=") => match parse_age(&a["--older-than=".len()..]) {
                Some(s) => older = Some(s),
                None => {
                    return usage_err(
                        prog,
                        &format!("invalid age '{}'", &a["--older-than=".len()..]),
                    )
                }
            },
            a if a.starts_with("--age-by=") => match AgeBy::parse(&a["--age-by=".len()..]) {
                Some(x) => {
                    age_by = x;
                    age_by_set = true;
                }
                None => return usage_err(prog, "--age-by takes letters from a, c and m"),
            },
            a if a.starts_with("--trash-dir=") => {
                dest.trash_pin = Some(crate::util::arg_path(&a["--trash-dir=".len()..]));
            }
            other => return usage_err(prog, &format!("unrecognized option '{other}'")),
        }
    }
    if want_profile {
        return usage_err(prog, "option '--profile' requires an argument");
    }
    if want_roots {
        return usage_err(prog, "option '--roots' requires an argument");
    }
    if list {
        return list_profiles(prog, json, verbose, &roots_cli);
    }
    if until.is_some() && when_below.is_none() {
        return usage_err(prog, "--until goes with --when-below");
    }
    if purge_trash && when_below.is_none() {
        return usage_err(prog, "--purge-trash goes with --when-below");
    }
    let now = crate::util::now_epoch();
    let mut status = 0;
    let mut groups: Vec<Group> = Vec::new();

    // The plain directories: given ones, or the temp dirs without profiles.
    let explicit = !dirs.is_empty();
    if !explicit && profiles.is_empty() {
        dirs = default_dirs();
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    for d in &dirs {
        match fs::canonicalize(d) {
            Ok(c) if c.is_dir() => {
                if c.parent().is_none() || is_home(&c) {
                    eprintln!("{prog}: refusing to reap '{}'", d.display());
                    status = 1;
                } else if !roots.contains(&c) {
                    roots.push(c);
                }
            }
            Ok(_) if explicit => {
                eprintln!("{prog}: '{}' is not a directory", d.display());
                status = 1;
            }
            Err(e) if explicit => {
                eprintln!("{prog}: cannot reap '{}': {e}", d.display());
                status = 1;
            }
            _ => {}
        }
    }
    // Without --older-than each directory takes its tmpfiles.d age, else
    // 10 days; one group per age.
    let ages = if older.is_none() && !roots.is_empty() {
        tmpfiles_ages()
    } else {
        Vec::new()
    };
    // In the default temp dirs a glob age line names directories of its
    // own, such as `e /tmp/job-* - - - 1h`: each match becomes a root with
    // that age, as systemd-tmpfiles cleans each match.
    if !explicit {
        let tops = roots.clone();
        for (pat, _) in &ages {
            let text = pat.to_string_lossy();
            if !text.contains(['*', '?', '[']) {
                continue;
            }
            let Ok(under) = fs::canonicalize(
                pat.ancestors()
                    .find(|a| !a.to_string_lossy().contains(['*', '?', '[']))
                    .unwrap_or(Path::new("/")),
            ) else {
                continue;
            };
            if !tops.iter().any(|t| under.starts_with(t)) {
                continue;
            }
            for m in reap_profiles::glob(&text, 16) {
                let is_dir = fs::symlink_metadata(&m).is_ok_and(|md| md.is_dir());
                if let (true, Ok(c)) = (is_dir, fs::canonicalize(&m)) {
                    if !roots.contains(&c) {
                        roots.push(c);
                    }
                }
            }
        }
    }
    // A root inside another is judged as a root, not as an entry of the
    // outer one, as systemd-tmpfiles leaves a path with a line of its own
    // to that line.
    let all_roots = roots.clone();
    // A line's time letters count unless --age-by was given, and a ~ age
    // reaps inside each subdirectory instead of the directory itself.
    type Key = (i64, Option<AgeBy>);
    let mut by_age: Vec<(Key, Vec<PathBuf>)> = Vec::new();
    for root in &roots {
        let rule = if older.is_none() {
            tmpfiles_age_for(root, &ages)
        } else {
            None
        };
        let age = older.or(rule.map(|r| r.secs)).unwrap_or(10 * 86_400);
        let by = rule.and_then(|r| r.by).filter(|_| !age_by_set);
        let targets: Vec<PathBuf> = if rule.is_some_and(|r| r.below) {
            let mut subs: Vec<PathBuf> = fs::read_dir(root)
                .map(|rd| {
                    rd.flatten()
                        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                        .map(|e| e.path())
                        .collect()
                })
                .unwrap_or_default();
            subs.sort();
            subs
        } else {
            vec![root.clone()]
        };
        for t in targets {
            match by_age.iter_mut().find(|(k, _)| *k == (age, by)) {
                Some((_, v)) => v.push(t),
                None => by_age.push(((age, by), vec![t])),
            }
        }
    }
    for ((older, by), roots) in by_age {
        let mut entries = Vec::new();
        for root in &roots {
            let Ok(root_meta) = fs::symlink_metadata(root) else {
                continue;
            };
            let Ok(rd) = fs::read_dir(root) else {
                eprintln!("{prog}: cannot read '{}'", root.display());
                status = 1;
                continue;
            };
            let mut names: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| !all_roots.contains(p))
                .collect();
            names.sort();
            entries.extend(
                names
                    .into_iter()
                    .map(|p| (p, plat::fs_key(root, &root_meta))),
            );
        }
        groups.push(Group {
            name: "tmp".into(),
            action: action.unwrap_or(Action::Trash),
            older,
            age_by: by,
            cutoff: now - older,
            marker: None,
            busy: Vec::new(),
            guard: false,
            tool: None,
            entries,
        });
    }

    if !profiles.is_empty() {
        let mut warns = Vec::new();
        let all = reap_profiles::all(crate::config::get(), &mut warns);
        for w in &warns {
            eprintln!("{prog}: warning: {w}");
        }
        let chosen = match reap_profiles::select(&all, &profiles) {
            Ok(c) => c,
            Err(e) => return usage_err(prog, &e),
        };
        for p in chosen {
            let age = older.unwrap_or(p.age);
            let mut entries: Vec<(PathBuf, u64)> = Vec::new();
            let proots = reap_profiles::roots(&roots_cli, &p);
            let max = depth.or(p.depth).unwrap_or(reap_profiles::STAR_DEPTH);
            for full in p
                .paths
                .iter()
                .flat_map(|pat| reap_profiles::expand_all(pat, &proots))
            {
                for e in reap_profiles::glob(&full, max) {
                    let dev = e
                        .parent()
                        .and_then(|d| fs::symlink_metadata(d).ok().map(|m| plat::fs_key(d, &m)))
                        .unwrap_or(u64::MAX);
                    if !entries.iter().any(|(x, _)| *x == e) {
                        entries.push((e, dev));
                    }
                }
            }
            if p.keep_newest {
                spare_newest_versions(&mut entries);
            }
            let tool = (!p.tool.is_empty()).then(|| reap_profiles::tool_command(&p, age));
            groups.push(Group {
                name: p.name.clone(),
                action: action.unwrap_or(p.action),
                older: age,
                age_by: None,
                cutoff: now - age,
                marker: p.marker.clone(),
                busy: p.busy.clone(),
                guard: p.guard,
                tool,
                entries,
            });
        }
    }

    // --when-below: keep only entries on file systems under the target,
    // with what each must free, and stop here when none is.
    let mut needs: Vec<(u64, (u64, u64))> = Vec::new();
    // A path on each file system in `needs`, for --purge-trash.
    let mut on: Vec<(u64, PathBuf)> = Vec::new();
    if let Some(wb) = when_below {
        let target = until.unwrap_or(wb);
        let need_of = |space: &crate::space::Space| {
            if wb.below(space) {
                target.need(space)
            } else {
                (0, 0)
            }
        };
        // The plain directories count even when empty, for --purge-trash.
        for r in &roots {
            if let (Ok(m), Ok(space)) = (fs::symlink_metadata(r), crate::space::query(r)) {
                let key = plat::fs_key(r, &m);
                if !needs.iter().any(|(d, _)| *d == key) {
                    needs.push((key, need_of(&space)));
                    on.push((key, r.clone()));
                }
            }
        }
        for g in &mut groups {
            g.entries.retain(|(e, dev)| {
                if let Some((_, n)) = needs.iter().find(|(d, _)| d == dev) {
                    return *n != (0, 0);
                }
                let Some(space) = e.parent().and_then(|p| crate::space::query(p).ok()) else {
                    return false;
                };
                let n = need_of(&space);
                // Below the trigger yet above --until still frees nothing.
                needs.push((*dev, n));
                on.push((*dev, e.clone()));
                n != (0, 0)
            });
        }
        // For --watch: the file systems this pass measured, which the next
        // tick checks with one statfs each before a full pass.
        if let Ok(mut w) = WATCHED.lock() {
            *w = on.iter().map(|(_, p)| p.clone()).collect();
        }
        if purge_trash {
            for (dev, need) in needs.iter_mut().filter(|(_, n)| *n != (0, 0)) {
                let at = on.iter().find(|(d, _)| d == dev).map(|(_, p)| p.as_path());
                *need = purge_oldest(prog, *dev, at, *need, target, dry_run, verbose);
            }
            for g in &mut groups {
                g.entries
                    .retain(|(_, dev)| needs.iter().any(|(d, n)| d == dev && *n != (0, 0)));
            }
        }
        if groups.iter().all(|g| g.entries.is_empty()) {
            if json {
                println!(
                    r#"{{"dry_run":{dry_run},"space_ok":true,"reaped":[],"skipped":[],"failed":0}}"#
                );
            } else if verbose {
                eprintln!("{prog}: free space is above the target; nothing to do");
            }
            return status;
        }
        use_tool = false;
    }

    // One process scan covers every entry, by way of the dirs they sit in.
    let mut scan_roots: Vec<PathBuf> = Vec::new();
    for g in &groups {
        for (e, _) in &g.entries {
            if let Some(parent) = e.parent() {
                let parent = parent.to_path_buf();
                if !scan_roots.contains(&parent) {
                    scan_roots.push(parent);
                }
            }
        }
    }
    let snap = Snapshot::take_under(&scan_roots);
    if !snap.supported {
        eprintln!(
            "{prog}: cannot tell which processes hold files on this platform; nothing reaped"
        );
        return 1;
    }
    let euid = plat::euid();
    if snap.unreadable > 0 && euid != 0 && verbose {
        eprintln!(
            "{prog}: {} could not be read; only your own entries are reaped",
            crate::util::plural(snap.unreadable as u64, "process", "processes")
        );
    }
    let ctx = Ctx {
        now,
        euid,
        age_by,
        snap,
        excl: tmpfiles_exclusions(),
        runtime: std::env::var_os("XDG_RUNTIME_DIR").and_then(|d| fs::canonicalize(d).ok()),
        home: crate::util::home_dir().and_then(|h| fs::canonicalize(h).ok()),
    };

    // A profile with a tool on PATH hands its cleanup to it under --tool.
    let mut tool_runs: Vec<Option<(String, Option<i32>)>> = vec![None; groups.len()];
    let mut outcomes: Vec<Outcome> = Vec::new();
    for (gi, g) in groups.iter().enumerate() {
        if use_tool {
            if let Some(cmd) = g.tool.as_ref().filter(|c| on_path(&c[0])) {
                let shown = cmd.join(" ");
                let code = if dry_run {
                    None
                } else {
                    match std::process::Command::new(&cmd[0]).args(&cmd[1..]).status() {
                        Ok(s) => s.code(),
                        Err(e) => {
                            eprintln!("{prog}: cannot run '{shown}': {e}");
                            Some(127)
                        }
                    }
                };
                if code.is_some_and(|c| c != 0) {
                    status = 1;
                }
                tool_runs[gi] = Some((shown, code));
                continue;
            }
        }
        for (path, dev) in &g.entries {
            if let Some(o) = judge(path, *dev, gi, g, &ctx) {
                outcomes.push(o);
            }
        }
    }

    // --when-below: oldest first within each profile, in profile order,
    // until each file system has what it needs. A dry run estimates from
    // the sizes; a real run measures the file system before each entry.
    if when_below.is_some() {
        outcomes.sort_by(|a, b| a.group.cmp(&b.group).then(b.age.cmp(&a.age)));
    }
    if when_below.is_some() && dry_run {
        for (dev, need) in &needs {
            let idx: Vec<usize> = (0..outcomes.len())
                .filter(|&i| outcomes[i].dev == *dev && outcomes[i].skip.is_none())
                .collect();
            let sizes: Vec<(u64, u64)> = idx
                .iter()
                .map(|&i| (outcomes[i].bytes, outcomes[i].inodes))
                .collect();
            for &i in &idx[enough(&sizes, *need)..] {
                outcomes[i].skip = Some("free space is back above the target".into());
            }
        }
    }
    // Measured before anything goes, while the extents still exist.
    let mut shared_set = crate::du_extent::SharedSet::default();
    if shared {
        let probe = crate::du_extent::Probe::default();
        for o in outcomes.iter_mut().filter(|o| o.skip.is_none()) {
            o.unique = Some(unique_bytes(&o.path, &probe, &mut shared_set));
        }
    }
    let shared_once = shared_set.distinct();
    let mut failed = 0usize;
    let target = until
        .or(when_below)
        .filter(|_| when_below.is_some() && !dry_run);
    // Bytes and inodes moved to a trash per device: they free space only
    // once the trash is emptied, but count toward the target so a trash
    // on the same disk does not take every entry.
    let mut credit: std::collections::HashMap<u64, (u64, u64)> = std::collections::HashMap::new();
    let mut reaped_idx: Vec<usize> = Vec::new();
    for (i, o) in outcomes.iter_mut().enumerate() {
        if o.skip.is_some() {
            continue;
        }
        if let Some(t) = target {
            if let Some(space) = o.path.parent().and_then(|p| crate::space::query(p).ok()) {
                let (nb, ni) = t.need(&space);
                let (cb, ci) = credit.get(&o.dev).copied().unwrap_or_default();
                if nb <= cb && ni <= ci {
                    o.skip = Some("free space is back above the target".into());
                    continue;
                }
            }
        }
        if !dry_run {
            let done = if groups[o.group].action == Action::Permanent {
                plat::remove(&o.path).map_err(|e| {
                    eprintln!("{prog}: cannot remove '{}': {e}", o.path.display());
                })
            } else {
                plat::trash(prog, &o.path, &dest)
            };
            if done.is_err() {
                failed += 1;
                status = 1;
                continue;
            }
            if groups[o.group].action != Action::Permanent {
                let c = credit.entry(o.dev).or_default();
                c.0 = c.0.saturating_add(o.bytes);
                c.1 = c.1.saturating_add(o.inodes);
            }
        }
        reaped_idx.push(i);
    }
    let reaped: Vec<&Outcome> = reaped_idx.iter().map(|&i| &outcomes[i]).collect();
    // Per group: entries reaped, their bytes, entries kept.
    let totals: Vec<(usize, u64, usize)> = (0..groups.len())
        .map(|gi| {
            let r = reaped.iter().filter(|o| o.group == gi);
            let kept = outcomes
                .iter()
                .filter(|o| o.group == gi && o.skip.is_some())
                .count();
            (r.clone().count(), r.map(|o| o.bytes).sum(), kept)
        })
        .collect();

    if json {
        let path_json = |p: &Path| json_escape(&str_from_os_ref(p.as_os_str()));
        let r: Vec<String> = reaped
            .iter()
            .map(|o| {
                format!(
                    r#"{{"path":"{}","profile":"{}","age":{},"bytes":{}{}}}"#,
                    path_json(&o.path),
                    json_escape(&groups[o.group].name),
                    o.age,
                    o.bytes,
                    o.unique
                        .map_or(String::new(), |u| format!(r#","unique":{u}"#))
                )
            })
            .collect();
        let s: Vec<String> = outcomes
            .iter()
            .filter_map(|o| {
                o.skip.as_ref().map(|why| {
                    format!(
                        r#"{{"path":"{}","profile":"{}","reason":"{}"}}"#,
                        path_json(&o.path),
                        json_escape(&groups[o.group].name),
                        json_escape(why)
                    )
                })
            })
            .collect();
        let g: Vec<String> = groups
            .iter()
            .enumerate()
            .map(|(gi, g)| {
                let (n, bytes, kept) = totals[gi];
                let (tool, code) = match &tool_runs[gi] {
                    Some((t, c)) => (
                        format!("\"{}\"", json_escape(t)),
                        c.map_or("null".to_string(), |c| c.to_string()),
                    ),
                    None => ("null".to_string(), "null".to_string()),
                };
                format!(
                    r#"{{"name":"{}","action":"{}","older_than":{},"entries":{n},"bytes":{bytes},"kept":{kept},"tool":{tool},"tool_status":{code}}}"#,
                    json_escape(&g.name),
                    g.action.as_str(),
                    g.older
                )
            })
            .collect();
        println!(
            r#"{{"dry_run":{dry_run},"older_than":{},"profiles":[{}],"reaped":[{}],"skipped":[{}],"failed":{failed}{}}}"#,
            older.unwrap_or(10 * 86_400),
            g.join(","),
            r.join(","),
            s.join(","),
            if shared {
                let u: u64 = reaped.iter().filter_map(|o| o.unique).sum();
                format!(r#","unique":{u},"shared":{shared_once}"#)
            } else {
                String::new()
            }
        );
        return status;
    }
    for o in &reaped {
        let verb = if dry_run {
            "would reap"
        } else if groups[o.group].action == Action::Permanent {
            "deleted"
        } else {
            "trashed"
        };
        let frees = o.unique.map_or(String::new(), |u| {
            format!(", frees {}", crate::util::format_bytes(u))
        });
        println!(
            "{verb} '{}' ({} old, {}{frees})",
            crate::util::term_path(&o.path),
            fmt_age(o.age),
            crate::util::format_bytes(o.bytes)
        );
    }
    if verbose {
        for o in &outcomes {
            if let Some(why) = &o.skip {
                println!(
                    "kept '{}': {}",
                    crate::util::term_path(&o.path),
                    crate::util::term_text(why)
                );
            }
        }
    }
    let verb = if dry_run { "would reap" } else { "reaped" };
    if !profiles.is_empty() {
        for (gi, g) in groups.iter().enumerate() {
            let (n, bytes, kept) = totals[gi];
            match &tool_runs[gi] {
                Some((t, _)) if dry_run => println!("{}: would run '{t}'", g.name),
                Some((t, _)) => println!("{}: ran '{t}'", g.name),
                None => println!(
                    "{}: {verb} {}, {} ({}); {kept} kept",
                    g.name,
                    crate::util::plural(n as u64, "entry", "entries"),
                    crate::util::format_bytes(bytes),
                    g.action.as_str()
                ),
            }
        }
    }
    let total: u64 = reaped.iter().map(|o| o.bytes).sum();
    eprintln!(
        "{prog}: {verb} {}, {}; {} kept",
        crate::util::plural(reaped.len() as u64, "entry", "entries"),
        crate::util::format_bytes(total),
        outcomes.iter().filter(|o| o.skip.is_some()).count()
    );
    if shared {
        let u: u64 = reaped.iter().filter_map(|o| o.unique).sum();
        eprintln!(
            "{prog}: frees {}, and up to {} more in extents shared with other files",
            crate::util::format_bytes(u),
            crate::util::format_bytes(shared_once)
        );
    }
    status
}

/// Empty the oldest items of this user's trashes on device `dev` until
/// `need` (bytes, inodes) is freed, and return what is still needed:
/// measured again after a real purge, estimated on a dry run.
#[cfg(unix)]
fn purge_oldest(
    prog: &str,
    dev: u64,
    at: Option<&Path>,
    need: (u64, u64),
    target: Floor,
    dry_run: bool,
    verbose: bool,
) -> (u64, u64) {
    use crate::retention::{prune, Policy, Prune};
    use std::os::unix::fs::MetadataExt;
    let dirs: Vec<crate::trashdir::TrashDir> = crate::trashdir::all()
        .into_iter()
        .filter(|d| fs::metadata(&d.root).is_ok_and(|m| m.dev() == dev))
        .collect();
    // On macOS a put lands in the Finder Trash, so purge that first.
    let mut left = need;
    if crate::finder::is_default(false) {
        let before = left;
        left = crate::finder::purge_oldest(prog, dev, left, dry_run);
        purged(dry_run, u64::from(left != before));
        if !dry_run && left != before {
            if let Some(at) = at {
                left = crate::space::query(at).map_or(left, |s| target.need(&s));
            }
        }
    }
    for dir in &dirs {
        if left.0 == 0 && left.1 == 0 {
            break;
        }
        let one = std::slice::from_ref(dir);
        let opts = |max| Prune {
            max,
            policy: Policy::Oldest,
            dry_run: true,
            verbose: false,
            protect_batch: None,
            gone_before: None,
            free_inodes: 0,
        };
        let total = prune(prog, one, &opts(u64::MAX)).left;
        let max = total.saturating_sub(left.0);
        let done = prune(
            prog,
            one,
            &Prune {
                dry_run,
                verbose,
                free_inodes: left.1,
                ..opts(max)
            },
        );
        purged(dry_run, done.removed);
        if done.removed > 0 {
            eprintln!(
                "{prog}: {} {} ({}) from trash '{}'",
                if dry_run { "would empty" } else { "emptied" },
                crate::util::plural(done.removed, "item", "items"),
                crate::util::format_bytes(done.bytes),
                dir.root.display()
            );
        }
        left = if dry_run {
            (
                left.0.saturating_sub(done.bytes),
                left.1.saturating_sub(done.inodes),
            )
        } else {
            crate::space::query(&dir.root).map_or(left, |s| target.need(&s))
        };
    }
    left
}

/// The Windows side: the oldest items in the Recycle Bin of the drive
/// that holds `at`, until `need` (bytes, files) is freed.
#[cfg(windows)]
fn purge_oldest(
    prog: &str,
    _dev: u64,
    at: Option<&Path>,
    need: (u64, u64),
    target: Floor,
    dry_run: bool,
    verbose: bool,
) -> (u64, u64) {
    // The volume that holds `at`: a drive, or a volume mounted on a
    // folder, whose Recycle Bin is its own.
    // Without the `\\?\` prefix canonicalize adds, so the drive reads
    // `C:\` in messages.
    let Some(at) = at.map(|p| {
        let c = fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let s = c.to_string_lossy();
        s.strip_prefix(r"\\?\")
            .map_or_else(|| c.clone(), PathBuf::from)
    }) else {
        return need;
    };
    let drive =
        crate::win_recycle::volume_root(&at).or_else(|| crate::win_recycle::drive_root_of(&at));
    let Some(drive) = drive else {
        return need;
    };
    let (n, bytes, files) = crate::win_recycle::purge_oldest_on(prog, &at, need, dry_run, verbose);
    purged(dry_run, n);
    if n > 0 {
        eprintln!(
            "{prog}: {} {} ({}) from the Recycle Bin of {}",
            if dry_run { "would empty" } else { "emptied" },
            crate::util::plural(n, "item", "items"),
            crate::util::format_bytes(bytes),
            drive.display()
        );
    }
    if dry_run {
        (need.0.saturating_sub(bytes), need.1.saturating_sub(files))
    } else {
        crate::space::query(&drive).map_or(need, |s| target.need(&s))
    }
}

/// Bytes on disk only the tree at `path` uses: what deleting it frees
/// at least. Shared extents of its files go to `set`, counted once
/// across every entry. Without an extent map a file counts in full.
fn unique_bytes(
    path: &Path,
    probe: &crate::du_extent::Probe,
    set: &mut crate::du_extent::SharedSet,
) -> u64 {
    let mut total = 0u64;
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(m) = fs::symlink_metadata(&p) else {
            continue;
        };
        if !plat::first_link(&m, &mut seen) {
            continue;
        }
        let disk = plat::bytes(&m);
        if m.is_dir() {
            total = total.saturating_add(disk);
            if let Ok(rd) = fs::read_dir(&p) {
                stack.extend(rd.flatten().map(|e| e.path()));
            }
            continue;
        }
        total = total.saturating_add(plat::file_unique(&p, &m, disk, probe, set));
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_read_units_and_bare_days() {
        assert_eq!(parse_age("90s"), Some(90));
        assert_eq!(parse_age("2h"), Some(7200));
        assert_eq!(parse_age("10"), Some(864_000));
        assert_eq!(parse_age("1w"), Some(604_800));
        assert_eq!(parse_age("3y"), None);
        assert_eq!(parse_age(""), None);
    }

    #[test]
    fn floors_parse_and_say_what_to_free() {
        assert_eq!(
            Floor::parse("10%"),
            Some(Floor {
                pct: Some(10),
                ..Default::default()
            })
        );
        assert_eq!(
            Floor::parse("20G,inodes=5%"),
            Some(Floor {
                bytes: Some(20 << 30),
                inodes_pct: Some(5),
                ..Default::default()
            })
        );
        assert_eq!(
            Floor::parse("inodes=5"),
            Some(Floor {
                inodes_pct: Some(5),
                ..Default::default()
            })
        );
        for bad in ["", "101%", "x", "inodes=", "10%,inodes=200"] {
            assert_eq!(Floor::parse(bad), None, "{bad}");
        }
        let s = crate::space::Space {
            total: 1000,
            avail: 50,
            inodes: Some(100),
            inodes_avail: Some(2),
        };
        let f = Floor::parse("10%,inodes=5%").unwrap();
        assert!(f.below(&s));
        assert_eq!(f.need(&s), (50, 3));
        assert_eq!(Floor::parse("40").unwrap().need(&s), (0, 0));
        assert!(!Floor::parse("5%").unwrap().below(&s));
    }

    #[test]
    fn enough_takes_entries_until_the_need_is_met() {
        let sizes = [(30, 1), (30, 1), (30, 1)];
        assert_eq!(enough(&sizes, (0, 0)), 0);
        assert_eq!(enough(&sizes, (31, 0)), 2);
        assert_eq!(enough(&sizes, (10, 3)), 3);
        assert_eq!(enough(&sizes, (1000, 0)), 3);
    }

    #[test]
    fn tmpfiles_ages_read_cleaning_lines_and_systemd_spans() {
        let body = "# c\nq /tmp 1777 root root 10d\nD /var/tmp 1777 root root 1w2d\nd /run/user/%b 0700 - - 1h\nd %V/sys 0700 - - 2h\ne /srv/scratch - - - ~12h30min\nd /keep 0755 - - -\nd /zero 0755 - - 0\nx /tmp/x\nL /tmp/link - - - 3d\ne /srv/cache-* - - - cmA:1d\ne /srv/odd - - - xyz:1d\n";
        let got = parse_tmpfiles_ages(body, false);
        let age = |secs, by, below| TmpAge { secs, by, below };
        let cm = Some(AgeBy {
            atime: true,
            ctime: true,
            mtime: true,
        });
        assert_eq!(
            got,
            vec![
                (PathBuf::from("/tmp"), age(10 * 86_400, None, false)),
                (PathBuf::from("/var/tmp"), age(9 * 86_400, None, false)),
                (PathBuf::from("/var/tmp/sys"), age(2 * 3600, None, false)),
                (
                    PathBuf::from("/srv/scratch"),
                    age(12 * 3600 + 30 * 60, None, true)
                ),
                (PathBuf::from("/srv/cache-*"), age(86_400, cm, false)),
            ]
        );
        assert_eq!(
            parse_tmp_age("~mM:1h"),
            Some(age(
                3600,
                Some(AgeBy {
                    atime: false,
                    ctime: false,
                    mtime: true
                }),
                true
            ))
        );
        assert_eq!(parse_tmp_age("bB:1h"), Some(age(3600, None, false)));
        assert_eq!(parse_span("90"), Some(90));
        assert_eq!(parse_span("1M"), Some(2_629_800));
        assert_eq!(parse_span("5q"), None);
        assert_eq!(
            tmpfiles_age_for(Path::new("/var/tmp"), &got).map(|a| a.secs),
            Some(9 * 86_400)
        );
        // A glob line covers a match with as many components, not deeper.
        assert_eq!(
            tmpfiles_age_for(Path::new("/srv/cache-pip"), &got).map(|a| a.secs),
            Some(86_400)
        );
        assert_eq!(tmpfiles_age_for(Path::new("/srv/cache-pip/x"), &got), None);
        assert_eq!(tmpfiles_age_for(Path::new("/nope"), &got), None);
    }

    #[test]
    fn only_older_versions_of_each_name_stay_to_reap() {
        let mut e: Vec<(PathBuf, u64)> = [
            "/c/chromium-1140",
            "/c/chromium-1148",
            "/c/chromium_headless_shell-1140",
            "/c/firefox-1466",
            "/c/firefox-990",
            "/c/notes",
            "/c/ffmpeg-beta",
            "/p/linux-131.0.6778.85",
            "/p/linux-131.0.6778.204",
            "/p/linux-99.1",
            "/j/IntelliJIdea2024.3",
            "/j/IntelliJIdea2025.1",
            "/j/PyCharm2024.2",
            "/j/acme",
        ]
        .iter()
        .map(|p| (PathBuf::from(p), 0))
        .collect();
        spare_newest_versions(&mut e);
        let got: Vec<&Path> = e.iter().map(|(p, _)| p.as_path()).collect();
        assert_eq!(
            got,
            [
                Path::new("/c/chromium-1140"),
                Path::new("/c/firefox-990"),
                Path::new("/p/linux-131.0.6778.85"),
                Path::new("/p/linux-99.1"),
                Path::new("/j/IntelliJIdea2024.3")
            ]
        );
    }

    #[test]
    fn tmpfiles_x_lines_become_patterns() {
        let body = "# c\nx /tmp/systemd-private-%b-*\nX! /var/tmp/keep\nd /tmp 1777 root root 10d\nq /var/tmp\nr! /tmp/.X0-lock\nR /var/tmp/once-%b\n";
        assert_eq!(
            parse_tmpfiles(body, false),
            vec![
                "/tmp/systemd-private-*-*".to_string(),
                "/var/tmp/keep".to_string(),
                "/tmp/.X0-lock".to_string(),
                "/var/tmp/once-*".to_string()
            ]
        );
    }

    #[test]
    fn exclusions_cover_matches_and_their_parents() {
        let pats = vec![
            "/tmp/systemd-private-*-*".to_string(),
            "/tmp/a/b/keep".to_string(),
        ];
        assert!(excluded(Path::new("/tmp/systemd-private-x-y"), &pats).is_some());
        assert!(excluded(Path::new("/tmp/a"), &pats).is_some());
        assert!(excluded(Path::new("/tmp/other"), &pats).is_none());
    }
}
