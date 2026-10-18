//! `rtrash du -o` snapshots, `du -f` to read one back, and `du diff`.
//!
//! A snapshot holds every directory total of a scan (up to `--max-depth`),
//! one record per line, so it stays readable with grep and diffable with
//! any text tool:
//!
//! ```text
//! # rtrash du snapshot 1
//! checksum crc32
//! created 1791650000
//! max_depth -
//! one_file_system 0
//! unreadable COUNT
//! unread REASON PATH
//! root DISK APPARENT FILES DIRS PATH
//! dir DISK APPARENT MTIME ARTEFACT PATH
//! end LINES CRC32
//! ```
//!
//! Fields are space-separated; PATH comes last, percent-encoded (spaces,
//! `%` and control bytes), and ARTEFACT reads `-` when the row has none.
//!
//! `unreadable` counts the entries the scan could not read, and each
//! `unread` record (the first 100) names one with its percent-encoded
//! reason. Writers before them left both out: such a snapshot loads with
//! the count unknown, not 0. The magic line stays at version 1, since a
//! reader skips records it does not know, so an older rtrash still reads
//! a newer snapshot (without the count).
//!
//! `checksum crc32` says the snapshot ends with an `end` record: LINES
//! counts the lines before it, the magic line too, and CRC32 (8 hex
//! digits) is the CRC-32 of their bytes. A reader refuses such a snapshot
//! when the record is missing or does not match, so a file cut off or
//! edited by hand is never read as if whole. CRC-32 catches accidents,
//! not forgery: whoever can edit the file can recompute it. A snapshot
//! from before the record still loads, and every reader refuses a known
//! record with the wrong number of fields, and a file with no `root` or
//! `dir` record at all. A `checksum` line naming any other sum is
//! refused, so one damaged byte there cannot turn the check off, and an
//! `end` record is checked wherever it appears.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use crate::du::{self, Item, Report, RootTotals, ScanOpts, SortKey};
use crate::file_replace::replace_file;
use crate::util::{format_bytes, json_escape, url_decode, url_encode};

const MAGIC: &str = "# rtrash du snapshot 1";

/// The CRC-32 (IEEE, as zlib and gzip use) table, one entry per byte.
const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// A running CRC-32 over the bytes of a snapshot.
#[derive(Debug, Clone, Copy)]
struct Crc32(u32);

impl Crc32 {
    fn new() -> Self {
        Crc32(!0)
    }

    fn update(&mut self, bytes: &[u8]) {
        let mut c = self.0;
        for &b in bytes {
            c = CRC_TABLE[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8);
        }
        self.0 = c;
    }

    fn value(self) -> u32 {
        !self.0
    }
}

/// A writer that sums and counts the lines it passes on, for the `end`
/// record.
struct Summed<W> {
    w: W,
    crc: Crc32,
    lines: u64,
}

impl<W: Write> Write for Summed<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.w.write(buf)?;
        self.crc.update(&buf[..n]);
        self.lines += buf[..n].iter().filter(|&&b| b == b'\n').count() as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}

const DIFF_HELP: &str = "\
Usage: {prog} diff [OPTION]... OLD [NEW]
Compare two du snapshots (written with du -o FILE, or ncdu exports) and list
the directories that grew or shrank most. Without NEW, scan the roots of OLD
again, with the depth OLD used, and compare against that.

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
    /// Entries the scan could not read; `None` when the snapshot does not
    /// say (an older writer, or a gdu export, which writes no `read_error`).
    pub unreadable: Option<u64>,
    /// The first unreadable entries, with the reason.
    pub unread: Vec<(PathBuf, String)>,
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
pub fn write(report: &Report, opts: &ScanOpts, created: i64, w: impl Write) -> io::Result<()> {
    let mut w = Summed {
        w,
        crc: Crc32::new(),
        lines: 0,
    };
    writeln!(w, "{MAGIC}")?;
    writeln!(w, "checksum crc32")?;
    writeln!(w, "created {created}")?;
    match opts.max_depth {
        Some(d) => writeln!(w, "max_depth {d}")?,
        None => writeln!(w, "max_depth -")?,
    }
    writeln!(w, "one_file_system {}", u8::from(opts.one_file_system))?;
    writeln!(w, "unreadable {}", report.unreadable)?;
    for (p, why) in &report.unread {
        writeln!(w, "unread {} {}", url_encode(why.as_bytes()), enc(p))?;
    }
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
    let Summed { mut w, crc, lines } = w;
    writeln!(w, "end {lines} {:08x}", crc.value())?;
    w.flush()
}

/// Write a snapshot to `path` whole, with [`replace_file`].
pub fn write_file(path: &Path, report: &Report, opts: &ScanOpts, created: i64) -> io::Result<()> {
    replace_file(path, |w, _| write(report, opts, created, w))
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

fn not_a_snapshot() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        concat!(
            "neither an rtrash du snapshot (write one with du -o FILE) ",
            "nor an ncdu export (JSON, zstd-compressed JSON or binary)",
        ),
    )
}

/// The fields each known record has; one with more or fewer was cut off
/// or edited.
fn fields_of(kind: &str) -> Option<usize> {
    match kind {
        "created" | "max_depth" | "one_file_system" | "unreadable" | "checksum" => Some(2),
        "unread" => Some(3),
        "root" | "dir" => Some(6),
        "end" => Some(3),
        _ => None,
    }
}

/// Check the `end` record on line `n` against the `n - 1` lines before
/// it and their CRC-32.
fn check_end(n: usize, f: &[&str], crc: Crc32) -> io::Result<()> {
    let lines: usize = f[1]
        .parse()
        .map_err(|_| bad(n, "bad line count in the end record"))?;
    let sum = u32::from_str_radix(f[2], 16)
        .ok()
        .filter(|_| f[2].len() == 8)
        .ok_or_else(|| bad(n, "bad checksum in the end record"))?;
    if lines != n - 1 {
        return Err(bad(
            n,
            &format!(
                "the end record counts {lines} lines before it, the file has {}: {}",
                n - 1,
                "lines were lost or added since it was written"
            ),
        ));
    }
    if sum != crc.value() {
        return Err(bad(
            n,
            &format!(
                "checksum mismatch (recorded {sum:08x}, read {:08x}): {}",
                crc.value(),
                "the snapshot was damaged or edited since it was written"
            ),
        ));
    }
    Ok(())
}

/// Parse a snapshot.
pub fn parse(mut r: impl BufRead) -> io::Result<Snapshot> {
    let mut snap = Snapshot::default();
    let mut crc = Crc32::new();
    let mut buf = Vec::new();
    let mut n = 0usize;
    // `checksum crc32` was read: the snapshot must end with a matching
    // `end` record.
    let mut sealed = false;
    let mut ended = false;
    let mut records = 0usize;
    loop {
        buf.clear();
        if r.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        n += 1;
        let mut text = &buf[..];
        text = text.strip_suffix(b"\n").unwrap_or(text);
        text = text.strip_suffix(b"\r").unwrap_or(text);
        if ended {
            if text.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            return Err(bad(n, "data after the end record"));
        }
        let line = std::str::from_utf8(text).map_err(|_| {
            if n == 1 {
                not_a_snapshot()
            } else {
                bad(n, "not text")
            }
        })?;
        if n == 1 {
            if line.trim_end() != MAGIC {
                return Err(not_a_snapshot());
            }
            crc.update(&buf);
            continue;
        }
        let f: Vec<&str> = line.split(' ').collect();
        if !line.is_empty() && !line.starts_with('#') {
            if let Some(want) = fields_of(f[0]).filter(|&k| k != f.len()) {
                return Err(bad(
                    n,
                    &format!(
                        "a {} record has {want} fields, this one {}: {}",
                        f[0],
                        f.len(),
                        "the snapshot was cut off or edited"
                    ),
                ));
            }
        }
        if f[0] == "end" {
            check_end(n, &f, crc)?;
            ended = true;
            continue;
        }
        crc.update(&buf);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let num = |k: usize| -> io::Result<u64> {
            f.get(k)
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| bad(n, "bad number"))
        };
        match f[0] {
            // Any other name is refused rather than read unchecked: one
            // damaged byte in this line must not turn the check off.
            "checksum" if f[1] == "crc32" => sealed = true,
            "checksum" => {
                return Err(bad(
                    n,
                    &format!(
                        "unknown checksum '{}': {}",
                        f[1], "the snapshot was damaged, or written by a newer rtrash"
                    ),
                ))
            }
            "created" => snap.created = num(1)? as i64,
            "max_depth" => {
                snap.max_depth = match f.get(1) {
                    Some(&"-") => None,
                    _ => Some(num(1)? as usize),
                }
            }
            "one_file_system" => snap.one_file_system = num(1)? != 0,
            "unreadable" => snap.unreadable = Some(num(1)?),
            "unread" => snap.unread.push((
                bytes_path(url_decode(f[2])),
                String::from_utf8_lossy(&url_decode(f[1])).into_owned(),
            )),
            "root" => {
                records += 1;
                snap.roots.push(RootTotals {
                    path: bytes_path(url_decode(f[5])),
                    disk: num(1)?,
                    apparent: num(2)?,
                    files: num(3)?,
                    dirs: num(4)?,
                    ..RootTotals::default()
                });
            }
            "dir" => {
                records += 1;
                snap.dirs.push(Item {
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
                    filtered_out: 0,
                });
            }
            // Records from a newer writer: skip, so old readers keep working.
            _ => {}
        }
    }
    if n == 0 {
        return Err(not_a_snapshot());
    }
    if sealed && !ended {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "the snapshot is cut off after line {n}: it has a checksum line but no end record"
            ),
        ));
    }
    if records == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the snapshot has no root or dir record: it is empty or cut off",
        ));
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

/// Read a snapshot, or an ncdu export (`ncdu -o FILE`, the same
/// compressed with `ncdu -c`, or the binary `ncdu -O FILE`), told apart
/// by its first bytes.
pub fn read(path: &Path) -> io::Result<Snapshot> {
    let f = fs::File::open(path)?;
    let mut r = io::BufReader::new(f);
    if crate::du_ncdu_bin::looks_like_binary(r.fill_buf()?) {
        // The format carries no date: the scan ended when the file was
        // written.
        let created = r
            .get_ref()
            .metadata()?
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs() as i64);
        return crate::du_ncdu_bin::parse(r, created);
    }
    if crate::du_ncdu::looks_like_zstd(r.fill_buf()?) {
        return crate::du_ncdu::parse_zstd(r);
    }
    if crate::du_ncdu::looks_like_export(r.fill_buf()?) {
        return crate::du_ncdu::parse(r);
    }
    parse(r)
}

/// A snapshot as a du report, ranked by `sort` and cut to `top` rows.
/// An unknown unreadable count reads as 0 here; callers that show it
/// check [`Snapshot::unreadable`] first.
pub fn to_report(snap: Snapshot, sort: SortKey, top: usize) -> Report {
    let mut keep = du::TopN::new(top, sort);
    keep.merge_vec(snap.dirs);
    Report {
        roots: snap.roots,
        dirs: keep.into_vec(),
        files: Vec::new(),
        unreadable: snap.unreadable.unwrap_or(0),
        unread: snap.unread,
        too_deep: 0,
        depth_limit: du::MAX_DEPTH,
        shared: None,
        partial: false,
        types: Vec::new(),
        snapshots: Vec::new(),
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
    out.sort_by(|a, b| crate::du::rank((a.delta().abs(), &a.path), (b.delta().abs(), &b.path)));
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
        unreadable: Some(report.unreadable),
        unread: report.unread,
    })
}

/// An unreadable count for people: the number, or `unknown`.
fn count_text(n: Option<u64>) -> String {
    n.map_or_else(|| "unknown".to_string(), |n| n.to_string())
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
            _ => files.push(crate::util::arg_path(a)),
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
            eprintln!(
                "{prog}: cannot read snapshot '{}': {e}",
                crate::util::err_path(&p)
            );
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
            "{{\"old_created\":{},\"new_created\":{},\"changes\":[{}],\"changed\":{},\"old_unreadable\":{},\"new_unreadable\":{}}}",
            old.created,
            new.created,
            rows.join(","),
            changes.len(),
            opt_json(old.unreadable),
            opt_json(new.unreadable)
        );
        return 0;
    }
    // A change in what could not be read changes what the sizes mean.
    let unread_line = (old.unreadable != new.unreadable).then(|| {
        format!(
            "Unreadable entries: {} -> {}",
            count_text(old.unreadable),
            count_text(new.unreadable)
        )
    });
    if changes.is_empty() {
        println!("No directory changed size.");
        if let Some(l) = &unread_line {
            println!("{l}");
        }
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
            crate::util::term_path(&c.path)
        );
    }
    if changes.len() > shown.len() {
        println!("({} more; -n N lists more)", changes.len() - shown.len());
    }
    if let Some(l) = &unread_line {
        println!("{l}");
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_replace::temp_beside;

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
            filtered_out: 0,
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
            unread: Vec::new(),
            too_deep: 0,
            depth_limit: du::MAX_DEPTH,
            shared: None,
            partial: false,
            types: Vec::new(),
            snapshots: Vec::new(),
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
        assert!(text.contains("\nunreadable 0\n"), "{text}");
        let snap = parse(&buf[..]).unwrap();
        assert_eq!(snap.unreadable, Some(0));
        assert_eq!(snap.created, 42);
        assert_eq!(snap.max_depth, Some(3));
        assert_eq!(snap.roots, report.roots);
        assert_eq!(snap.dirs, report.dirs);
    }

    #[test]
    fn unreadable_count_and_paths_round_trip_and_old_snapshots_say_unknown() {
        let report = Report {
            roots: Vec::new(),
            dirs: vec![item("/w/x", 512)],
            files: Vec::new(),
            unreadable: 3,
            unread: vec![
                (
                    PathBuf::from("/w/a b"),
                    "Permission denied (os error 13)".into(),
                ),
                (PathBuf::from("/w/100%"), "gone".into()),
            ],
            too_deep: 0,
            depth_limit: du::MAX_DEPTH,
            shared: None,
            partial: false,
            types: Vec::new(),
            snapshots: Vec::new(),
        };
        let mut buf = Vec::new();
        write(&report, &snapshot_opts(ScanOpts::default()), 1, &mut buf).unwrap();
        let text = String::from_utf8(buf.clone()).unwrap();
        assert_eq!(
            text.lines().filter(|l| l.starts_with("unread ")).count(),
            2,
            "{text}"
        );
        assert!(
            text.contains("\nunread Permission%20denied%20%28os%20error%2013%29 /w/a%20b\n"),
            "{text}"
        );
        let snap = parse(&buf[..]).unwrap();
        assert_eq!(snap.unreadable, Some(3));
        assert_eq!(snap.unread, report.unread);
        let r = to_report(snap, SortKey::Disk, 10);
        assert_eq!((r.unreadable, r.unread.len()), (3, 2));
        // Written before the count existed: unknown, not 0.
        let old = parse(&b"# rtrash du snapshot 1\ncreated 5\ndir 1 1 0 - /x\n"[..]).unwrap();
        assert_eq!(old.unreadable, None);
        assert!(old.unread.is_empty());
        assert_eq!(count_text(old.unreadable), "unknown");
    }

    #[test]
    fn parse_rejects_other_files_and_skips_unknown_records() {
        assert!(parse(&b"{\"roots\":[]}"[..]).is_err());
        let s = parse(&b"# rtrash du snapshot 1\nfuture 1 2\ndir 1 1 0 - /x\n"[..]).unwrap();
        assert_eq!(s.dirs.len(), 1);
    }

    fn small_report() -> Report {
        Report {
            roots: vec![RootTotals {
                path: PathBuf::from("/w"),
                disk: 512,
                apparent: 256,
                files: 1,
                dirs: 1,
                ..RootTotals::default()
            }],
            dirs: vec![item("/w/x", 512)],
            files: Vec::new(),
            unreadable: 0,
            unread: Vec::new(),
            too_deep: 0,
            depth_limit: du::MAX_DEPTH,
            shared: None,
            partial: false,
            types: Vec::new(),
            snapshots: Vec::new(),
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rtrash-snap-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_snapshot_file_is_replaced_whole_and_leaves_no_temporary() {
        let d = scratch("whole");
        let out = d.join("a.snap");
        fs::write(&out, "old contents").unwrap();
        // A temporary a crashed run left under this process id.
        fs::write(temp_beside(&out), "stale").unwrap();
        let opts = snapshot_opts(ScanOpts::default());
        write_file(&out, &small_report(), &opts, 9).unwrap();
        let snap = read(&out).unwrap();
        assert_eq!((snap.created, snap.dirs.len()), (9, 1));
        let names: Vec<_> = fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["a.snap"]);
        // A directory that is missing fails before anything is written.
        let gone = d.join("no/such/dir/b.snap");
        assert!(write_file(&gone, &small_report(), &opts, 9).is_err());
        assert!(!d.join("no").exists());
        // A fill that fails halfway, as a scan can, keeps the old file.
        let half = replace_file(&out, |w, tmp| {
            let tmp = tmp.expect("a regular file goes through a temporary");
            assert!(tmp.is_file() && tmp.parent() == Some(d.as_path()));
            w.write_all(b"# rtrash du snapshot 1\ncut")?;
            Err::<(), _>(io::Error::other("disk full"))
        });
        assert_eq!(half.unwrap_err().to_string(), "disk full");
        assert_eq!(read(&out).unwrap().created, 9);
        let names: Vec<_> = fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["a.snap"]);
        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_snapshot_to_a_device_or_through_a_link_is_written_in_place() {
        use std::os::unix::fs::PermissionsExt;
        let opts = snapshot_opts(ScanOpts::default());
        // A rename over /dev/null would need /dev; in place, it just works.
        write_file(Path::new("/dev/null"), &small_report(), &opts, 1).unwrap();
        let d = scratch("link");
        let target = d.join("real.snap");
        fs::write(&target, "old").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        let link = d.join("link.snap");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        write_file(&link, &small_report(), &opts, 2).unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(read(&target).unwrap().created, 2);
        // Written over as a regular file, a snapshot keeps its mode.
        write_file(&target, &small_report(), &opts, 3).unwrap();
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn crc32_matches_the_zlib_check_value() {
        let mut c = Crc32::new();
        c.update(b"1234");
        c.update(b"56789");
        assert_eq!(c.value(), 0xcbf4_3926);
        assert_eq!(Crc32::new().value(), 0);
    }

    #[test]
    fn a_snapshot_ends_with_its_line_count_and_crc32() {
        let mut buf = Vec::new();
        write(
            &small_report(),
            &snapshot_opts(ScanOpts::default()),
            9,
            &mut buf,
        )
        .unwrap();
        // The CRC-32 here is zlib.crc32 of the eight lines above the end.
        let want = "# rtrash du snapshot 1\nchecksum crc32\ncreated 9\nmax_depth -\n\
                    one_file_system 0\nunreadable 0\nroot 512 256 1 1 /w\n\
                    dir 512 256 7 - /w/x\nend 8 71f04092\n";
        assert_eq!(String::from_utf8(buf.clone()).unwrap(), want);
        assert_eq!(parse(&buf[..]).unwrap().dirs.len(), 1);
        // Blank lines after the end are harmless.
        buf.extend_from_slice(b"\n\n");
        assert!(parse(&buf[..]).is_ok());
    }

    #[test]
    fn a_cut_off_or_edited_snapshot_is_refused() {
        let mut buf = Vec::new();
        write(
            &small_report(),
            &snapshot_opts(ScanOpts::default()),
            9,
            &mut buf,
        )
        .unwrap();
        // Every cut but the last newline: inside the header, between
        // records, inside a record and inside the end record.
        for cut in 0..buf.len() - 1 {
            assert!(
                parse(&buf[..cut]).is_err(),
                "a cut at {cut} read as whole: {:?}",
                String::from_utf8_lossy(&buf[..cut])
            );
        }
        let text = String::from_utf8(buf).unwrap();
        let err = |t: &str| parse(t.as_bytes()).unwrap_err().to_string();
        let edited = text.replace("dir 512 256", "dir 612 256");
        assert!(
            err(&edited).contains("checksum mismatch"),
            "{}",
            err(&edited)
        );
        let dropped = text.replace("root 512 256 1 1 /w\n", "");
        assert!(
            err(&dropped).contains("lines were lost"),
            "{}",
            err(&dropped)
        );
        let after = format!("{text}dir 1 1 0 - /w/y\n");
        assert!(
            err(&after).contains("data after the end"),
            "{}",
            err(&after)
        );
        let no_end = text.replace("end 8 71f04092\n", "");
        assert!(err(&no_end).contains("cut off"), "{}", err(&no_end));
        // The header is checked too: the magic line, the checksum name,
        // and the checksum line itself, whose loss the end record counts.
        let magic = text.replace("snapshot 1\n", "snapshot 2\n");
        assert!(
            err(&magic).contains("neither an rtrash du snapshot"),
            "{}",
            err(&magic)
        );
        let renamed = text.replace("checksum crc32", "checksum crc64");
        assert!(
            err(&renamed).contains("unknown checksum 'crc64'"),
            "{}",
            err(&renamed)
        );
        let unsealed = text.replace("checksum crc32\n", "");
        assert!(
            err(&unsealed).contains("lines were lost"),
            "{}",
            err(&unsealed)
        );
        for edit in [&magic, &renamed, &unsealed, &edited, &dropped] {
            assert_ne!(edit, &text);
        }
    }

    #[test]
    fn old_snapshots_load_but_cut_records_and_empty_files_do_not() {
        let old = b"# rtrash du snapshot 1\ncreated 5\nroot 2 2 1 1 /x\ndir 1 1 0 - /x\n";
        assert_eq!(parse(&old[..]).unwrap().dirs.len(), 1);
        // Cut inside a record: one field short.
        let e = parse(&old[..old.len() - 4]).unwrap_err().to_string();
        assert!(e.contains("a dir record has 6 fields, this one 5"), "{e}");
        // Cut before the first record.
        let e = parse(&b"# rtrash du snapshot 1\ncreated 5\n"[..]).unwrap_err();
        assert!(e.to_string().contains("no root or dir record"), "{e}");
        // A checksum this reader cannot check is refused, not skipped.
        let newer = b"# rtrash du snapshot 1\nchecksum sha256\ndir 1 1 0 - /x\nend 3 ab\n";
        let e = parse(&newer[..]).unwrap_err().to_string();
        assert!(e.contains("line 2: unknown checksum 'sha256'"), "{e}");
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
