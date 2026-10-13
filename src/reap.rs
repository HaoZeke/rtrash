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
use std::os::unix::fs::{FileTypeExt, MetadataExt};
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
                          default 10d for DIR, the profile's age for one
      --age-by=LETTERS  which times count, from a (access), c (change) and
                          m (modification); default acm; a directory's
                          access time never counts
  -n, --dry-run         show what would go and each profile's total
      --permanent       delete instead of moving to the trash
      --trash           move to the trash, even for a permanent profile
      --trash-dir=PATH  put into this trash root instead of the usual one
      --home-only       always use the home trash
  -v, --verbose         also show each skipped entry and why
      --json            print a JSON object ({dry_run, older_than,
                          profiles, reaped, skipped, failed})
      --help            display this help and exit
      --version         output version information and exit

An entry stays when a live process has it or anything in it open or mapped
or as its working directory, when it is or holds a socket, pipe, device or
mount point, when another user owns any of it (unless run as root), when
it is a lock or pid file naming a live process, when it holds
$XDG_RUNTIME_DIR, or when a tmpfiles.d x or X line covers it. A cargo
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
            t = t.max(m.atime());
        }
        if self.ctime {
            t = t.max(m.ctime());
        }
        if self.mtime {
            t = t.max(m.mtime());
        }
        if t == i64::MIN {
            // A directory aged by atime alone: fall back to its mtime.
            t = m.mtime();
        }
        t
    }
}

/// tmpfiles.d paths to leave alone: the path field of every `x` and `X`
/// line, with specifiers widened to `*`.
pub fn tmpfiles_exclusions() -> Vec<String> {
    let mut out = Vec::new();
    for dir in ["/etc/tmpfiles.d", "/run/tmpfiles.d", "/usr/lib/tmpfiles.d"] {
        let Ok(rd) = fs::read_dir(dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("conf") {
                continue;
            }
            // A conf file is small; a huge one is not a conf file.
            if e.metadata()
                .map_or(true, |m| !m.is_file() || m.len() > 1 << 20)
            {
                continue;
            }
            if let Ok(body) = fs::read_to_string(&p) {
                out.extend(parse_tmpfiles(&body));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn parse_tmpfiles(body: &str) -> Vec<String> {
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
        if kind != "x" && kind != "X" {
            continue;
        }
        let mut pat = String::new();
        let mut chars = path.chars();
        while let Some(c) = chars.next() {
            if c == '%' {
                match chars.next() {
                    Some('%') => pat.push('%'),
                    Some(_) => pat.push('*'),
                    None => pat.push('%'),
                }
            } else {
                pat.push(c);
            }
        }
        out.push(pat);
    }
    out
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
    /// Why it must stay, found during the walk.
    keep: Option<String>,
}

fn kind_reason(ft: &fs::FileType) -> Option<&'static str> {
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

/// Walk an entry for its newest time and size, stopping at the first
/// reason to keep it; a time newer than `cutoff` is such a reason, so a
/// busy tree is never walked in full.
fn walk(entry: &Path, top: &fs::Metadata, dev: u64, ctx: &Ctx, g: &Group) -> Walk {
    let mut w = Walk {
        newest: ctx.age_by.newest(top),
        bytes: top.blocks() * 512,
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
            if m.dev() != dev {
                w.keep = Some(format!("holds the mount point '{}'", p.display()));
                return w;
            }
            if let Some(k) = kind_reason(&m.file_type()) {
                w.keep = Some(format!("holds {k} '{}'", p.display()));
                return w;
            }
            if ctx.euid != 0 && m.uid() != ctx.euid {
                w.keep = Some(format!("holds '{}' of user {}", p.display(), m.uid()));
                return w;
            }
            if g.guard && secret_name(&e.file_name().to_string_lossy()) {
                w.keep = Some(format!(
                    "holds '{}', which looks like settings, history or a credential",
                    p.display()
                ));
                return w;
            }
            w.newest = w.newest.max(ctx.age_by.newest(&m));
            if w.newest > g.cutoff {
                return w;
            }
            w.bytes += m.blocks() * 512;
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
    skip: Option<String>,
}

/// The entries of one profile, or of the plain directories (`tmp`).
struct Group {
    name: String,
    action: Action,
    older: i64,
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
        age: ctx.now - ctx.age_by.newest(&m),
        bytes: 0,
        skip: None,
    };
    let name = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    o.skip = if path == Path::new("/") || ctx.home.as_ref().is_some_and(|h| h.starts_with(path)) {
        Some("holds your home directory".to_string())
    } else if name.starts_with(".Trash") {
        Some("a trash directory".to_string())
    } else if ctx.runtime.as_ref().is_some_and(|r| r.starts_with(path)) {
        Some("the XDG runtime directory".to_string())
    } else if let Some(pat) = excluded(path, &ctx.excl) {
        Some(format!("excluded by tmpfiles.d '{pat}'"))
    } else if m.dev() != parent_dev {
        Some("a mount point".to_string())
    } else if let Some(k) = kind_reason(&m.file_type()) {
        Some(k.to_string())
    } else if ctx.euid != 0 && m.uid() != ctx.euid {
        Some(format!("owned by user {}", m.uid()))
    } else if g.guard && secret_name(&name) {
        Some("looks like settings, history or a credential".to_string())
    } else {
        None
    };
    if o.skip.is_some() {
        return Some(o);
    }
    let w = walk(path, &m, parent_dev, ctx, g);
    o.age = ctx.now - w.newest;
    o.bytes = w.bytes;
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

fn default_dirs() -> Vec<PathBuf> {
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
    use std::os::unix::fs::PermissionsExt;
    let exec = |p: &Path| {
        fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if cmd.contains('/') {
        return exec(Path::new(cmd));
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|d| exec(&d.join(cmd))))
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

pub fn run(prog: &str, args: &[String]) -> i32 {
    let mut older: Option<i64> = None;
    let mut age_by = AgeBy {
        atime: true,
        ctime: true,
        mtime: true,
    };
    let mut dry_run = false;
    let mut action: Option<Action> = None;
    let mut verbose = false;
    let mut json = false;
    let mut list = false;
    let mut use_tool = false;
    let mut profiles: Vec<String> = Vec::new();
    let mut opts = crate::put::Opts {
        force: true,
        recursive: true,
        quiet: true,
        ..crate::put::Opts::default()
    };
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
            "--home-only" => opts.home_only = true,
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
                Some(x) => age_by = x,
                None => return usage_err(prog, "--age-by takes letters from a, c and m"),
            },
            a if a.starts_with("--trash-dir=") => {
                opts.trash_pin = Some(crate::util::arg_path(&a["--trash-dir=".len()..]));
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
                if c == Path::new("/") || crate::safety::is_home(&c) {
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
    if !roots.is_empty() {
        let older = older.unwrap_or(10 * 86_400);
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
            let mut names: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
            names.sort();
            entries.extend(names.into_iter().map(|p| (p, root_meta.dev())));
        }
        groups.push(Group {
            name: "tmp".into(),
            action: action.unwrap_or(Action::Trash),
            older,
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
                        .and_then(|d| fs::symlink_metadata(d).ok())
                        .map_or(u64::MAX, |m| m.dev());
                    if !entries.iter().any(|(x, _)| *x == e) {
                        entries.push((e, dev));
                    }
                }
            }
            let tool = (!p.tool.is_empty()).then(|| reap_profiles::tool_command(&p, age));
            groups.push(Group {
                name: p.name.clone(),
                action: action.unwrap_or(p.action),
                older: age,
                cutoff: now - age,
                marker: p.marker.clone(),
                busy: p.busy.clone(),
                guard: p.guard,
                tool,
                entries,
            });
        }
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
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
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

    let mut failed = 0usize;
    let mut reaped: Vec<&Outcome> = Vec::new();
    for o in &outcomes {
        if o.skip.is_some() {
            continue;
        }
        if !dry_run {
            let done = if groups[o.group].action == Action::Permanent {
                crate::fastdelete::remove_path(&o.path).map_err(|e| {
                    eprintln!("{prog}: cannot remove '{}': {e}", o.path.display());
                })
            } else {
                match crate::put::put_one(prog, &o.path, &opts) {
                    Ok(Some(_)) => Ok(()),
                    Ok(None) | Err(_) => Err(()),
                }
            };
            if done.is_err() {
                failed += 1;
                status = 1;
                continue;
            }
        }
        reaped.push(o);
    }
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
                    r#"{{"path":"{}","profile":"{}","age":{},"bytes":{}}}"#,
                    path_json(&o.path),
                    json_escape(&groups[o.group].name),
                    o.age,
                    o.bytes
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
            r#"{{"dry_run":{dry_run},"older_than":{},"profiles":[{}],"reaped":[{}],"skipped":[{}],"failed":{failed}}}"#,
            older.unwrap_or(10 * 86_400),
            g.join(","),
            r.join(","),
            s.join(",")
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
        println!(
            "{verb} '{}' ({} old, {})",
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
    status
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
    fn tmpfiles_x_lines_become_patterns() {
        let body = "# c\nx /tmp/systemd-private-%b-*\nX! /var/tmp/keep\nd /tmp 1777 root root 10d\nq /var/tmp\n";
        assert_eq!(
            parse_tmpfiles(body),
            vec![
                "/tmp/systemd-private-*-*".to_string(),
                "/var/tmp/keep".to_string()
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
