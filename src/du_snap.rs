//! `rtrash du -o` snapshots, `du -f` to read one back, and `du diff`.
//!
//! A snapshot holds every directory total of a scan (up to `--max-depth`),
//! one record per line, so it stays readable with grep and diffable with
//! any text tool:
//!
//! ```text
//! # rtrash du snapshot 1
//! created 1791650000
//! max_depth -
//! one_file_system 0
//! root DISK APPARENT FILES DIRS PATH
//! dir DISK APPARENT MTIME ARTEFACT PATH
//! ```
//!
//! Fields are space-separated; PATH comes last, percent-encoded (spaces,
//! `%` and control bytes), and ARTEFACT reads `-` when the row has none.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use crate::du::{self, Item, Report, RootTotals, ScanOpts, SortKey};
use crate::util::{format_bytes, json_escape, url_decode, url_encode};

const MAGIC: &str = "# rtrash du snapshot 1";

const DIFF_HELP: &str = "\
Usage: {prog} diff [OPTION]... OLD [NEW]
Compare two du snapshots (written with du -o FILE) and list the directories
that grew or shrank most. Without NEW, scan the roots of OLD again, with
the depth OLD used, and compare against that.

  -n, --top=N      list the N largest changes (default 20)
      --apparent   compare apparent sizes instead of disk use
      --json       emit a JSON object (scripts)
      --help       display this help and exit
";

/// A snapshot read back from disk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub created: i64,
    pub max_depth: Option<usize>,
    pub one_file_system: bool,
    pub roots: Vec<RootTotals>,
    pub dirs: Vec<Item>,
}

#[cfg(unix)]
fn path_bytes(p: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(p: &Path) -> Vec<u8> {
    p.to_string_lossy().into_owned().into_bytes()
}

#[cfg(unix)]
fn bytes_path(b: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(b))
}

#[cfg(not(unix))]
fn bytes_path(b: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&b).into_owned())
}

fn enc(p: &Path) -> String {
    url_encode(&path_bytes(p))
}

/// Scan settings for a snapshot: every directory level, no file rows, and
/// no top-N cut, so two snapshots compare row for row.
pub fn snapshot_opts(mut opts: ScanOpts) -> ScanOpts {
    opts.all_dirs = true;
    opts.top = usize::MAX;
    opts.no_files = true;
    opts.artefacts_only = false;
    opts.older_cutoff = None;
    opts.newer_cutoff = None;
    opts
}

/// Write `report` (scanned with [`snapshot_opts`]) as a snapshot.
pub fn write(report: &Report, opts: &ScanOpts, created: i64, mut w: impl Write) -> io::Result<()> {
    writeln!(w, "{MAGIC}")?;
    writeln!(w, "created {created}")?;
    match opts.max_depth {
        Some(d) => writeln!(w, "max_depth {d}")?,
        None => writeln!(w, "max_depth -")?,
    }
    writeln!(w, "one_file_system {}", u8::from(opts.one_file_system))?;
    for r in &report.roots {
        writeln!(
            w,
            "root {} {} {} {} {}",
            r.disk,
            r.apparent,
            r.files,
            r.dirs,
            enc(&r.path)
        )?;
    }
    for d in report.dirs.iter().filter(|d| d.is_dir) {
        writeln!(
            w,
            "dir {} {} {} {} {}",
            d.disk,
            d.apparent,
            d.mtime,
            d.artefact.unwrap_or("-"),
            enc(&d.path)
        )?;
    }
    w.flush()
}

fn bad(line: usize, what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("line {line}: {what}"))
}

/// The `&'static str` artefact kind for a name read from a snapshot.
fn artefact_static(name: &str) -> Option<&'static str> {
    if name == "-" {
        return None;
    }
    du::ARTEFACT_KINDS
        .iter()
        .copied()
        .find(|k| *k == name)
        .or(Some("artefact"))
}

/// Parse a snapshot.
pub fn parse(r: impl BufRead) -> io::Result<Snapshot> {
    let mut snap = Snapshot::default();
    let mut lines = r.lines();
    match lines.next() {
        Some(Ok(l)) if l.trim_end() == MAGIC => {}
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not an rtrash du snapshot (write one with du -o FILE)",
            ))
        }
    }
    for (i, line) in lines.enumerate() {
        let n = i + 2;
        let line = line?;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split(' ').collect();
        let num = |k: usize| -> io::Result<u64> {
            f.get(k)
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| bad(n, "bad number"))
        };
        match f[0] {
            "created" => snap.created = num(1)? as i64,
            "max_depth" => {
                snap.max_depth = match f.get(1) {
                    Some(&"-") => None,
                    _ => Some(num(1)? as usize),
                }
            }
            "one_file_system" => snap.one_file_system = num(1)? != 0,
            "root" if f.len() == 6 => snap.roots.push(RootTotals {
                path: bytes_path(url_decode(f[5])),
                disk: num(1)?,
                apparent: num(2)?,
                files: num(3)?,
                dirs: num(4)?,
                ..RootTotals::default()
            }),
            "dir" if f.len() == 6 => snap.dirs.push(Item {
                path: bytes_path(url_decode(f[5])),
                is_dir: true,
                disk: num(1)?,
                apparent: num(2)?,
                mtime: f[3].parse().map_err(|_| bad(n, "bad mtime"))?,
                items: 0,
                artefact: artefact_static(f[4]),
                repos: Vec::new(),
                git: None,
                unique: None,
            }),
            // Records from a newer writer: skip, so old readers keep working.
            _ => {}
        }
    }
    Ok(snap)
}

/// Folded stacks for flame graph tools (`flamegraph.pl`, inferno,
/// speedscope): one line per directory, its frames joined by `;` and then
/// its own bytes, the part outside the directories listed below it. The
/// first frame names the scanned root. A `;` or control character inside a
/// name becomes `_`, since the format has no escape. A directory whose own
/// part is zero gets no line; its children still draw it.
pub fn write_stacks(
    roots: &[RootTotals],
    dirs: &[Item],
    apparent: bool,
    mut w: impl Write,
) -> io::Result<()> {
    use std::ffi::OsStr;
    let size = |disk: u64, app: u64| if apparent { app } else { disk };
    // Keyed by the raw path text: hashing and ordering a Path walks its
    // components, which showed up when folding 12000 directories.
    let mut below: HashMap<&OsStr, u64> = HashMap::with_capacity(dirs.len());
    for d in dirs {
        if let Some(p) = d.path.parent() {
            let e = below.entry(p.as_os_str()).or_default();
            *e = e.saturating_add(size(d.disk, d.apparent));
        }
    }
    let own =
        |p: &Path, total: u64| total.saturating_sub(below.get(p.as_os_str()).copied().unwrap_or(0));
    let mut sorted: Vec<&Item> = dirs.iter().collect();
    sorted.sort_unstable_by(|a, b| a.path.as_os_str().cmp(b.path.as_os_str()));
    let mut line = String::new();
    for r in roots {
        let head = frame(&r.path.to_string_lossy());
        let n = own(&r.path, size(r.disk, r.apparent));
        if n > 0 {
            writeln!(w, "{head} {n}")?;
        }
        for d in &sorted {
            let Ok(rel) = d.path.strip_prefix(&r.path) else {
                continue;
            };
            let n = own(&d.path, size(d.disk, d.apparent));
            if n == 0 || rel.as_os_str().is_empty() {
                continue;
            }
            line.clear();
            line.push_str(&head);
            for c in rel.components() {
                line.push(';');
                push_frame(&mut line, &c.as_os_str().to_string_lossy());
            }
            writeln!(w, "{line} {n}")?;
        }
    }
    w.flush()
}

fn frame(name: &str) -> String {
    let mut s = String::with_capacity(name.len());
    push_frame(&mut s, name);
    s
}

fn push_frame(out: &mut String, name: &str) {
    if !name
        .bytes()
        .any(|b| b == b';' || b.is_ascii_control() || b >= 0x80)
    {
        out.push_str(name);
        return;
    }
    out.extend(
        name.chars()
            .map(|c| if c == ';' || c.is_control() { '_' } else { c }),
    );
}

pub fn read(path: &Path) -> io::Result<Snapshot> {
    let f = fs::File::open(path)?;
    parse(io::BufReader::new(f))
}

/// A snapshot as a du report, ranked by `sort` and cut to `top` rows.
pub fn to_report(snap: Snapshot, sort: SortKey, top: usize) -> Report {
    let mut keep = du::TopN::new(top, sort);
    keep.merge_vec(snap.dirs);
    Report {
        roots: snap.roots,
        dirs: keep.into_vec(),
        files: Vec::new(),
        unreadable: 0,
        too_deep: 0,
        shared: None,
        partial: false,
        types: Vec::new(),
    }
}

/// One directory whose size changed between two snapshots. `None` means
/// the directory was absent from that side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: PathBuf,
    pub old: Option<u64>,
    pub new: Option<u64>,
}

impl Change {
    pub fn delta(&self) -> i128 {
        self.new.unwrap_or(0) as i128 - self.old.unwrap_or(0) as i128
    }
}

/// Sizes to compare; `du diff` offers disk use and apparent size only.
fn size(it: &Item, sort: SortKey) -> u64 {
    match sort {
        SortKey::Apparent => it.apparent,
        _ => it.disk,
    }
}

/// Directories and roots whose size differs, largest change first.
pub fn diff(old: &Snapshot, new: &Snapshot, sort: SortKey) -> Vec<Change> {
    let mut sizes: HashMap<&Path, (Option<u64>, Option<u64>)> = HashMap::new();
    let root_size = |r: &RootTotals| match sort {
        SortKey::Apparent => r.apparent,
        _ => r.disk,
    };
    for r in &old.roots {
        sizes.entry(&r.path).or_default().0 = Some(root_size(r));
    }
    for r in &new.roots {
        sizes.entry(&r.path).or_default().1 = Some(root_size(r));
    }
    for d in &old.dirs {
        sizes.entry(&d.path).or_default().0 = Some(size(d, sort));
    }
    for d in &new.dirs {
        sizes.entry(&d.path).or_default().1 = Some(size(d, sort));
    }
    let mut out: Vec<Change> = sizes
        .into_iter()
        .map(|(p, (old, new))| Change {
            path: p.to_path_buf(),
            old,
            new,
        })
        .filter(|c| c.old != c.new)
        .collect();
    out.sort_by(|a, b| {
        b.delta()
            .abs()
            .cmp(&a.delta().abs())
            .then_with(|| a.path.cmp(&b.path))
    });
    out
}

fn signed(d: i128) -> String {
    let mag = format_bytes(d.unsigned_abs().min(u64::MAX as u128) as u64);
    if d >= 0 {
        format!("+{mag}")
    } else {
        format!("-{mag}")
    }
}

fn opt_json(v: Option<u64>) -> String {
    v.map_or_else(|| "null".to_string(), |n| n.to_string())
}

/// Scan the roots of `old` again with the settings it was written with.
fn rescan(old: &Snapshot) -> io::Result<Snapshot> {
    let opts = snapshot_opts(ScanOpts {
        max_depth: old.max_depth,
        one_file_system: old.one_file_system,
        ..ScanOpts::default()
    });
    let paths: Vec<PathBuf> = old.roots.iter().map(|r| r.path.clone()).collect();
    let report = du::scan(&paths, &opts)?;
    Ok(Snapshot {
        created: crate::util::now_epoch(),
        max_depth: old.max_depth,
        one_file_system: old.one_file_system,
        roots: report.roots,
        dirs: report.dirs,
    })
}

/// `rtrash du diff`.
pub fn run_diff(prog: &str, args: &[String]) -> i32 {
    let mut top = 20usize;
    let mut json = false;
    let mut sort = SortKey::Disk;
    let mut files: Vec<PathBuf> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--json" => json = true,
            "--apparent" => sort = SortKey::Apparent,
            "--help" => {
                print!("{}", DIFF_HELP.replace("{prog}", prog));
                return 0;
            }
            "-n" | "--top" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse().ok()) {
                    Some(n) => top = n,
                    None => {
                        eprintln!("{prog}: {a} needs a count");
                        return 2;
                    }
                }
            }
            _ if a.starts_with("--top=") => match a["--top=".len()..].parse() {
                Ok(n) => top = n,
                Err(_) => {
                    eprintln!("{prog}: --top needs a count");
                    return 2;
                }
            },
            _ if a.starts_with('-') && a != "-" => {
                eprintln!("{prog}: unrecognized option '{a}'");
                eprintln!("Try '{prog} diff --help' for more information.");
                return 2;
            }
            _ => files.push(PathBuf::from(a)),
        }
        i += 1;
    }
    if files.is_empty() || files.len() > 2 {
        eprintln!("{prog}: diff takes OLD and an optional NEW snapshot");
        eprintln!("Try '{prog} diff --help' for more information.");
        return 2;
    }
    let load = |p: &Path| {
        read(p).map_err(|e| {
            eprintln!("{prog}: cannot read snapshot '{}': {e}", p.display());
        })
    };
    let Ok(old) = load(&files[0]) else {
        return 1;
    };
    let new = match files.get(1) {
        Some(p) => match load(p) {
            Ok(s) => s,
            Err(()) => return 1,
        },
        None => match rescan(&old) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{prog}: cannot scan: {e}");
                return 1;
            }
        },
    };
    let changes = diff(&old, &new, sort);
    let shown = &changes[..changes.len().min(top)];
    if json {
        let rows: Vec<String> = shown
            .iter()
            .map(|c| {
                format!(
                    "{{\"path\":\"{}\",\"old\":{},\"new\":{},\"delta\":{}}}",
                    json_escape(&c.path.to_string_lossy()),
                    opt_json(c.old),
                    opt_json(c.new),
                    c.delta()
                )
            })
            .collect();
        println!(
            "{{\"old_created\":{},\"new_created\":{},\"changes\":[{}],\"changed\":{}}}",
            old.created,
            new.created,
            rows.join(","),
            changes.len()
        );
        return 0;
    }
    if changes.is_empty() {
        println!("No directory changed size.");
        return 0;
    }
    println!("{:>11}  {:>10}  {:>10}  PATH", "CHANGE", "BEFORE", "AFTER");
    for c in shown {
        let side =
            |v: Option<u64>, missing: &str| v.map_or_else(|| missing.to_string(), format_bytes);
        println!(
            "{:>11}  {:>10}  {:>10}  {}",
            signed(c.delta()),
            side(c.old, "new"),
            side(c.new, "gone"),
            c.path.display()
        );
    }
    if changes.len() > shown.len() {
        println!("({} more; -n N lists more)", changes.len() - shown.len());
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(p: &str, disk: u64) -> Item {
        Item {
            path: PathBuf::from(p),
            is_dir: true,
            apparent: disk / 2,
            disk,
            mtime: 7,
            items: 0,
            artefact: None,
            repos: Vec::new(),
            git: None,
            unique: None,
        }
    }

    #[test]
    fn snapshot_roundtrips_odd_paths_and_artefacts() {
        let mut target = item("/w/a b/target", 4096);
        target.artefact = Some("cargo-target");
        let report = Report {
            roots: vec![RootTotals {
                path: PathBuf::from("/w"),
                disk: 9000,
                apparent: 8000,
                files: 3,
                dirs: 2,
                ..RootTotals::default()
            }],
            dirs: vec![target.clone(), item("/w/100%\nnew", 512)],
            files: Vec::new(),
            unreadable: 0,
            too_deep: 0,
            shared: None,
            partial: false,
            types: Vec::new(),
        };
        let opts = snapshot_opts(ScanOpts {
            max_depth: Some(3),
            ..ScanOpts::default()
        });
        let mut buf = Vec::new();
        write(&report, &opts, 42, &mut buf).unwrap();
        let text = String::from_utf8(buf.clone()).unwrap();
        assert!(
            text.contains("dir 4096 2048 7 cargo-target /w/a%20b/target\n"),
            "{text}"
        );
        let snap = parse(&buf[..]).unwrap();
        assert_eq!(snap.created, 42);
        assert_eq!(snap.max_depth, Some(3));
        assert_eq!(snap.roots, report.roots);
        assert_eq!(snap.dirs, report.dirs);
    }

    #[test]
    fn parse_rejects_other_files_and_skips_unknown_records() {
        assert!(parse(&b"{\"roots\":[]}"[..]).is_err());
        let s = parse(&b"# rtrash du snapshot 1\nfuture 1 2\ndir 1 1 0 - /x\n"[..]).unwrap();
        assert_eq!(s.dirs.len(), 1);
    }

    #[test]
    fn diff_orders_by_change_and_marks_new_and_gone() {
        let old = Snapshot {
            dirs: vec![item("/w/a", 100), item("/w/gone", 50), item("/w/same", 10)],
            ..Snapshot::default()
        };
        let new = Snapshot {
            dirs: vec![item("/w/a", 1000), item("/w/new", 70), item("/w/same", 10)],
            ..Snapshot::default()
        };
        let c = diff(&old, &new, SortKey::Disk);
        let paths: Vec<&str> = c.iter().map(|c| c.path.to_str().unwrap()).collect();
        assert_eq!(paths, ["/w/a", "/w/new", "/w/gone"]);
        assert_eq!(c[0].delta(), 900);
        assert_eq!((c[1].old, c[1].new), (None, Some(70)));
        assert_eq!((c[2].old, c[2].new), (Some(50), None));
        assert_eq!(signed(-50), "-50 B");
    }
}
