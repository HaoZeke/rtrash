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
use crate::util::{json_escape, str_from_os_ref};

const HELP: &str = "\
Usage: {prog} [OPTION]... [DIR]...
Trash the entries in DIR that nobody has touched for --older-than and that
no live process holds. Without DIR: $TMPDIR, /tmp and /var/tmp.

      --older-than=AGE  only entries whose newest time is older than AGE:
                          a number with s, m, h, d or w (days without one);
                          default 10d, as systemd-tmpfiles uses for /tmp
      --age-by=LETTERS  which times count, from a (access), c (change) and
                          m (modification); default acm; a directory's
                          access time never counts
  -n, --dry-run         show what would go, change nothing
      --permanent       delete instead of moving to the trash
      --trash-dir=PATH  put into this trash root instead of the usual one
      --home-only       always use the home trash
  -v, --verbose         also show each skipped entry and why
      --json            print a JSON object ({dry_run, older_than,
                          reaped, skipped, failed})
      --help            display this help and exit
      --version         output version information and exit

An entry stays when a live process has it or anything in it open or mapped
or as its working directory, when it is or holds a socket, pipe, device or
mount point, when another user owns any of it (unless run as root), when
it is a lock or pid file naming a live process, when it holds
$XDG_RUNTIME_DIR, or when a tmpfiles.d x or X line covers it.
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

fn walk(entry: &Path, top: &fs::Metadata, dev: u64, age_by: AgeBy, euid: u32) -> Walk {
    let mut w = Walk {
        newest: age_by.newest(top),
        bytes: top.blocks() * 512,
        keep: None,
    };
    if !top.is_dir() {
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
            if euid != 0 && m.uid() != euid {
                w.keep = Some(format!("holds '{}' of user {}", p.display(), m.uid()));
                return w;
            }
            w.newest = w.newest.max(age_by.newest(&m));
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
    age: i64,
    bytes: u64,
    skip: Option<String>,
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

pub fn run(prog: &str, args: &[String]) -> i32 {
    let mut older = 10 * 86_400;
    let mut age_by = AgeBy {
        atime: true,
        ctime: true,
        mtime: true,
    };
    let mut dry_run = false;
    let mut permanent = false;
    let mut verbose = false;
    let mut json = false;
    let mut opts = crate::put::Opts {
        force: true,
        recursive: true,
        quiet: true,
        ..crate::put::Opts::default()
    };
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut no_more = false;
    for arg in args {
        if no_more || !arg.starts_with('-') || arg == "-" {
            dirs.push(crate::util::arg_path(arg));
            continue;
        }
        match arg.as_str() {
            "--" => no_more = true,
            "-n" | "--dry-run" => dry_run = true,
            "-v" | "--verbose" => verbose = true,
            "--permanent" => permanent = true,
            "--json" => json = true,
            "--home-only" => opts.home_only = true,
            "--help" => {
                print!("{}", HELP.replace("{prog}", prog));
                return 0;
            }
            "--version" => {
                println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                return 0;
            }
            a if a.starts_with("--older-than=") => match parse_age(&a["--older-than=".len()..]) {
                Some(s) => older = s,
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
    let explicit = !dirs.is_empty();
    if !explicit {
        dirs = default_dirs();
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut status = 0;
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
    let snap = Snapshot::take_under(&roots);
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
    let excl = tmpfiles_exclusions();
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").and_then(|d| fs::canonicalize(d).ok());
    let now = crate::util::now_epoch();
    let cutoff = now - older;
    let mut outcomes: Vec<Outcome> = Vec::new();
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
        for path in names {
            let Ok(m) = fs::symlink_metadata(&path) else {
                continue;
            };
            let mut o = Outcome {
                path: path.clone(),
                age: now - age_by.newest(&m),
                bytes: 0,
                skip: None,
            };
            let name = path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            o.skip = if name.starts_with(".Trash") {
                Some("a trash directory".to_string())
            } else if runtime.as_ref().is_some_and(|r| r.starts_with(&path)) {
                Some("the XDG runtime directory".to_string())
            } else if let Some(pat) = excluded(&path, &excl) {
                Some(format!("excluded by tmpfiles.d '{pat}'"))
            } else if m.dev() != root_meta.dev() {
                Some("a mount point".to_string())
            } else if let Some(k) = kind_reason(&m.file_type()) {
                Some(k.to_string())
            } else if euid != 0 && m.uid() != euid {
                Some(format!("owned by user {}", m.uid()))
            } else {
                None
            };
            if o.skip.is_none() {
                let w = walk(&path, &m, root_meta.dev(), age_by, euid);
                o.age = now - w.newest;
                o.bytes = w.bytes;
                o.skip = if let Some(k) = w.keep {
                    Some(k)
                } else if w.newest > cutoff {
                    Some(format!("touched {} ago", fmt_age(now - w.newest)))
                } else if let Some(pid) = live_lock(&path, &m) {
                    Some(format!("lock file of live pid {pid}"))
                } else {
                    let holds = snap.under(&path);
                    holds.first().map(|h| {
                        let more = holds.iter().filter(|x| x.pid != h.pid).count();
                        let more = if more > 0 { " and others" } else { "" };
                        format!("in use by pid {} ({}, {}){more}", h.pid, h.command, h.how)
                    })
                };
            }
            outcomes.push(o);
        }
    }
    let mut failed = 0usize;
    let mut reaped: Vec<&Outcome> = Vec::new();
    for o in &outcomes {
        if o.skip.is_some() {
            continue;
        }
        if !dry_run {
            let done = if permanent {
                crate::fastdelete::remove_path(&o.path).map_err(|e| {
                    eprintln!("{prog}: cannot remove '{}': {e}", o.path.display());
                })
            } else {
                match crate::put::put_one(prog, &o.path, &opts) {
                    Ok(Some(_)) => Ok(()),
                    Ok(None) => Err(()),
                    Err(_) => Err(()),
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
    if json {
        let item = |o: &Outcome| {
            format!(
                r#"{{"path":"{}","age":{},"bytes":{}}}"#,
                json_escape(&str_from_os_ref(o.path.as_os_str())),
                o.age,
                o.bytes
            )
        };
        let r: Vec<String> = reaped.iter().map(|o| item(o)).collect();
        let s: Vec<String> = outcomes
            .iter()
            .filter_map(|o| {
                o.skip.as_ref().map(|why| {
                    format!(
                        r#"{{"path":"{}","reason":"{}"}}"#,
                        json_escape(&str_from_os_ref(o.path.as_os_str())),
                        json_escape(why)
                    )
                })
            })
            .collect();
        println!(
            r#"{{"dry_run":{dry_run},"older_than":{older},"reaped":[{}],"skipped":[{}],"failed":{failed}}}"#,
            r.join(","),
            s.join(",")
        );
        return status;
    }
    let verb = if dry_run {
        "would reap"
    } else if permanent {
        "deleted"
    } else {
        "trashed"
    };
    for o in &reaped {
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
    let total: u64 = reaped.iter().map(|o| o.bytes).sum();
    eprintln!(
        "{prog}: {} {}, {}; {} kept",
        verb,
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
