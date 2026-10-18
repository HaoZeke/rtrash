use std::fs;
use std::path::PathBuf;

use crate::info;
use crate::trashdir::{self, TrashDir};

const HELP: &str = "\
Usage: {prog} [OPTION]... [PATTERN]
List trashed items as 'DELETION-DATE ORIGINAL-PATH', oldest first.
PATTERN keeps items whose file name holds it, or matches it when it has
a glob character (* ? [), like --glob.
Payloads with no .trashinfo come last, dated ????-??-??, under their path
in the trash (\"orphan\":true in JSON); an age filter leaves them out.

      --home-only       only the home trash ($XDG_DATA_HOME/Trash)
      --trash-dir=PATH  list only this trash directory (repeatable);
                     default is the home trash plus every mounted volume
      --all-users       list every user's trash this account can read
                     (root reads all) and add an OWNER column; read-only
                     (as trash-list, no OWNER column, as trash-cli)
      --volumes         print the mount points searched for trash and exit
      --trash-dirs      print the trash directories read and exit
      --older-than=AGE  only items trashed more than AGE ago; AGE is DAYS
                     or a duration such as 2h, 30m or 1d12h
      --newer-than=AGE  only items trashed within the last AGE
      --larger-than=SIZE  only items taking more than SIZE on disk
      --smaller-than=SIZE only items taking less than SIZE on disk
                     (SIZE as in `empty --max-size`: 500M, 1.5G, 64k;
                     sizes as --size shows them; orphans included)
      --glob=PATTERN    only items whose original path (PATTERN with a /)
                     or file name matches a shell glob
      --regex=RE        only items whose original path matches RE
      --name=NAME       only items whose file name is NAME
      --cwd-only        only items that came from the current directory
                     or below it
      --directory=DIR   only items that came from DIR or below it (DIR
                     need not exist any more)
      --size            add each item's disk usage (payload plus its
                     .trashinfo, as status counts it); as trash-list,
                     trash-cli's row: the payload's size in bytes in
                     place of the date. trash-cli's hidden --files works
                     there too and adds ' -> FILE', the item in the trash
      --show-trashpath  under each row, show where the item sits in the
                     trash (\"trash_path\" in --json; not with --format),
                     as gtrash find --show-trashpath
      --sort=KEY        order by date (default), size, name (file name)
                     or path (original path); size implies --size
      --reverse         reverse the order
      --depth=N         under each trashed directory, show what it holds,
                     N levels down, indented (plain rows only)
      --last=N          only the last N rows of the listing (after --sort and
                     --reverse), like tail -n: the N newest by default
      --json            emit a JSON array (scripts; one object per item;
                     \"size\" in bytes with --size; \"missing\" is true
                     when the payload is gone); field names are stable,
                     and new fields only get added
      --format=TEMPLATE print each item through TEMPLATE, unescaped:
                     {date} {epoch} {path} {name} {trash} {batch}
                     {size} (bytes) {owner}; {{ and }} for braces,
                     \\t \\n \\0 for tab, newline, NUL; no newline added
      --help            display this help and exit
      --version         output version information and exit
";

pub struct Entry {
    pub date: Option<String>,
    pub epoch: i64,
    pub original: std::path::PathBuf,
    pub name: String,
    pub dir: TrashDir,
    /// Put batch id from the `.trashinfo`, when rtrash wrote one.
    pub batch: Option<String>,
}

impl Entry {
    /// True when the record's payload under `files/` is gone, so the item
    /// cannot be restored (`rtrash empty --dangling` removes such records).
    pub fn missing(&self) -> bool {
        fs::symlink_metadata(crate::util::join_name(&self.dir.files(), &self.name)).is_err()
    }

    /// Lock this entry's trash and check the entry again: what a scan
    /// read may be stale by the time anything acts on it. The record must
    /// read as the scan read it (same original path, deletion date and put
    /// batch), or for an orphan still be missing with its payload there.
    /// In between, the item may have left the trash and a put may have
    /// given its name to another item, which an action on the old scan
    /// would hit instead. A record whose payload is gone passes: rm takes
    /// such a record, and restore refuses it through [`Locked::gone`].
    pub fn relock(&self) -> Result<Locked<'_>, Relock> {
        let lock = trashdir::TrashLock::acquire(&self.dir.root).map_err(Relock::Lock)?;
        let held = Locked {
            entry: self,
            _lock: lock,
        };
        if fs::symlink_metadata(held.record()).is_ok() {
            let same = read_entry(&self.dir, &held.record(), &self.name, Pathless::Keep).is_ok_and(
                |now| {
                    now.original == self.original
                        && now.date == self.date
                        && now.batch == self.batch
                },
            );
            return if same { Ok(held) } else { Err(Relock::Changed) };
        }
        let orphan = self.date.is_none() && self.batch.is_none();
        match (held.gone(), orphan) {
            (true, _) => Err(Relock::Gone),
            // An orphan, read as one, is still one.
            (false, true) => Ok(held),
            (false, false) => Err(Relock::Changed),
        }
    }
}

/// Why [`Entry::relock`] handed back no [`Locked`] entry.
#[derive(Debug)]
pub enum Relock {
    /// The trash lock could not be taken.
    Lock(std::io::Error),
    /// The item left the trash: its record and payload are both gone.
    Gone,
    /// The record no longer reads as the scan read it.
    Changed,
}

impl Relock {
    /// Why nothing was done to `entry`, for an error line.
    pub fn reason(&self, entry: &Entry) -> String {
        let name = crate::util::show_name(&entry.name);
        match self {
            Relock::Lock(e) => format!("cannot lock trash '{}': {e}", entry.dir.root.display()),
            Relock::Gone => format!("'{name}' is no longer in the trash"),
            Relock::Changed => format!(
                "'{name}' changed in the trash after it was read (another item now has \
                 its name); left as it is"
            ),
        }
    }
}

/// An entry checked again with its trash locked, by [`Entry::relock`]
/// alone: the record a scan saw is still there (for an orphan, its
/// payload), so what is done through this is done to that item. The lock
/// stays until this is dropped, so no put, rm or empty of that trash runs
/// in between.
pub struct Locked<'a> {
    entry: &'a Entry,
    _lock: trashdir::TrashLock,
}

impl<'a> Locked<'a> {
    /// The entry, as checked.
    pub fn entry(&self) -> &'a Entry {
        self.entry
    }

    /// The item's payload under `files/`.
    pub fn payload(&self) -> PathBuf {
        crate::util::join_name(&self.entry.dir.files(), &self.entry.name)
    }

    /// The item's record under `info/`.
    pub fn record(&self) -> PathBuf {
        crate::util::join_info(&self.entry.dir.info(), &self.entry.name)
    }

    /// Whether the payload is gone from `files/`, read under the lock.
    pub fn gone(&self) -> bool {
        crate::util::is_gone(&self.payload())
    }
}

/// The entry that record `name` in `dir` makes as it reads now, or `None`
/// when the record is missing or cannot be read.
pub fn entry_named(dir: &TrashDir, name: &str) -> Option<Entry> {
    let record = crate::util::join_info(&dir.info(), name);
    read_entry(dir, &record, name, Pathless::Keep).ok()
}

/// Every item the records in `dirs` name, oldest first. A record that
/// cannot be read, or whose relative `Path=` climbs out of its volume, is
/// left out; one with no `Path=` stays, with an empty original (or the
/// volume top), for rm, restore, empty and the totals as before.
pub fn collect(dirs: &[TrashDir]) -> Vec<Entry> {
    collect_with(dirs, Pathless::Keep).entries
}

/// What `list` shows, and the `NAME.trashinfo` records it left out, in
/// path order: those [`collect`] leaves out, and also one with no `Path=`
/// (or an empty one), which names no original. trash-cli's trash-list
/// leaves that one out too. `status --check` names each of them. Other
/// names in `info/` are not records; [`Listing::strays`] counts them.
pub fn collect_counted(dirs: &[TrashDir]) -> Listing {
    let mut listing = collect_with(dirs, Pathless::Skip);
    listing.left.sort_by(|a, b| a.record.cmp(&b.record));
    listing
}

/// The items of some trashes, and what of them could not be listed.
pub struct Listing {
    pub entries: Vec<Entry>,
    pub left: Vec<Unlisted>,
    /// For each trash, in the order given: the names in its `info/` that
    /// are not `NAME.trashinfo` (a `.DS_Store`, a temporary file), which
    /// `status --check` reports as stray.
    pub strays: Vec<u64>,
}

/// A record `list` leaves out, and the line trash-cli's trash-list prints
/// on stderr for it, when trash-cli reports that record too: one it
/// cannot open (`[Errno N] TEXT: 'FILE'`) and one with no line starting
/// `Path=` (`Parse Error: FILE: Unable to parse Path.`). trash-cli lists
/// the rest (an empty `Path=`, one that climbs out of its volume, one
/// over the size cap) or hangs on them (a FIFO), so they get no line.
pub struct Unlisted {
    pub record: PathBuf,
    /// The root of the trash the record is in.
    pub root: PathBuf,
    pub trash_cli: Option<String>,
}

/// What to do with a record that has no `Path=`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pathless {
    Keep,
    Skip,
}

fn collect_with(dirs: &[TrashDir], pathless: Pathless) -> Listing {
    use rayon::prelude::*;
    let mut out = Vec::new();
    let mut left = Vec::new();
    let mut strays = Vec::with_capacity(dirs.len());
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir.info()) else {
            strays.push(0);
            continue;
        };
        let paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        let records = |p: &PathBuf| {
            p.file_name().is_some_and(|f| {
                crate::util::trash_item_name(&crate::util::str_from_os_ref(f)).is_some()
            })
        };
        strays.push(paths.iter().filter(|p| !records(p)).count() as u64);
        // One read per item: spread a big trash over the rayon pool. A
        // name that is not a record gives None.
        let read: Vec<Result<Entry, Unlisted>> = paths
            .into_par_iter()
            .filter_map(|path| {
                let fname = crate::util::str_from_os_ref(path.file_name()?);
                let name = crate::util::trash_item_name(&fname)?;
                Some(
                    read_entry(dir, &path, name, pathless).map_err(|trash_cli| Unlisted {
                        record: path.clone(),
                        root: dir.root.clone(),
                        trash_cli,
                    }),
                )
            })
            .collect();
        for r in read {
            match r {
                Ok(e) => out.push(e),
                Err(u) => left.push(u),
            }
        }
    }
    out.sort_by(|a, b| {
        a.epoch
            .cmp(&b.epoch)
            .then_with(|| a.original.cmp(&b.original))
            .then_with(|| a.name.cmp(&b.name))
    });
    Listing {
        entries: out,
        left,
        strays,
    }
}

/// The entry the record at `path` (for the payload `name`) gives. It is
/// an error when the record cannot be read, its `Path=` does not resolve
/// in `dir`, or it has none and `pathless` says to skip it (as `status
/// --check` flags it); the error carries trash-cli's stderr line for it,
/// if trash-cli prints one (see [`Unlisted`]).
fn read_entry(
    dir: &TrashDir,
    path: &std::path::Path,
    name: &str,
    pathless: Pathless,
) -> Result<Entry, Option<String>> {
    let content =
        info::read_bounded(path, info::RECORD_CAP).map_err(|e| trash_cli_read_line(&e, path))?;
    let parsed = info::parse_bytes(&content);
    if pathless == Pathless::Skip && parsed.path.as_os_str().is_empty() {
        // trash-cli takes the first line that starts with `Path=`, even an
        // empty one, and fails only when there is none.
        let has_line = content
            .split(|&b| b == b'\n')
            .any(|l| l.starts_with(b"Path="));
        return Err((!has_line).then(|| {
            format!(
                "Parse Error: {}: Unable to parse Path.",
                py_text(path, false)
            )
        }));
    }
    let epoch = parsed
        .deletion_date
        .as_deref()
        .and_then(info::parse_local_to_epoch)
        .unwrap_or(0);
    // Skip hostile/corrupt relative Path= that escape the volume topdir.
    let original = dir
        .resolve_original_checked(&parsed.path)
        .map_err(|_| None)?;
    Ok(Entry {
        original,
        date: parsed.deletion_date,
        epoch,
        name: name.to_string(),
        dir: dir.clone(),
        batch: parsed.batch,
    })
}

/// trash-cli's line for a record it cannot open: Python's `str()` of the
/// OSError, `[Errno N] TEXT: 'FILE'`. read_bounded refuses a directory
/// before it opens one, where Python's open() fails with EISDIR. A FIFO,
/// a device or a record over the cap gets no line (trash-cli would hang
/// on a FIFO, or read the rest).
fn trash_cli_read_line(e: &std::io::Error, record: &std::path::Path) -> Option<String> {
    if e.raw_os_error().is_some() {
        return trash_cli_os_line(e, record);
    }
    if fs::metadata(record).is_ok_and(|m| m.is_dir()) {
        return trash_cli_os_line(&std::io::Error::from_raw_os_error(libc::EISDIR), record);
    }
    None
}

fn trash_cli_os_line(e: &std::io::Error, record: &std::path::Path) -> Option<String> {
    let n = e.raw_os_error()?;
    let full = e.to_string();
    let text = full
        .strip_suffix(&format!(" (os error {n})"))
        .unwrap_or(&full);
    Some(format!("[Errno {n}] {text}: {}", py_text(record, true)))
}

/// A path in a trash-list row, as trash-cli 0.26.9 prints it: quoted in
/// GNU ls's shell-escape style on a terminal, as is in a pipe, and a byte
/// that is not UTF-8 as `\xNN` either way. trash-cli prints the name raw
/// on a terminal too under TRASH_RAW_OUTPUT; rtrash does not.
fn cli_row_path(p: &std::path::Path) -> String {
    if crate::util::stdout_is_tty() {
        shell_escape(p)
    } else {
        py_text(p, false)
    }
}

/// trash-cli's `shell_escape` (from GNU ls and rm): a name of safe
/// characters only stays bare; the rest goes in single quotes, a `'` as
/// `'\''`, and each control character outside them as `$'\n'`, or
/// `$'\NNN'` in octal. A byte that is not UTF-8 stays inside the quotes
/// as `\xNN`. The bidirectional overrides, which trash-cli leaves raw,
/// get `$'\uNNNN'` so they cannot reorder the row.
fn shell_escape(p: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    const SAFE: &str = "%+-./:=@_,";
    let bytes = p.as_os_str().as_bytes();
    let safe = |c: char| c.is_ascii_alphanumeric() || SAFE.contains(c);
    if !bytes.is_empty() && std::str::from_utf8(bytes).is_ok_and(|t| t.chars().all(safe)) {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut out = String::new();
    let mut quoted = false;
    let mut open = |out: &mut String, want: bool| {
        if quoted != want {
            out.push('\'');
            quoted = want;
        }
    };
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            let cp = c as u32;
            let control = cp < 0x20 || (0x7f..=0x9f).contains(&cp);
            let bidi = matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
            if control || bidi {
                open(&mut out, false);
                let mnemonic = match c {
                    '\x07' => Some('a'),
                    '\x08' => Some('b'),
                    '\t' => Some('t'),
                    '\n' => Some('n'),
                    '\x0b' => Some('v'),
                    '\x0c' => Some('f'),
                    '\r' => Some('r'),
                    _ => None,
                };
                out.push_str(&match mnemonic {
                    Some(m) => format!("$'\\{m}'"),
                    None if bidi => format!("$'\\u{cp:04x}'"),
                    None => format!("$'\\{cp:03o}'"),
                });
                continue;
            }
            open(&mut out, true);
            if c == '\'' {
                out.push_str("'\\''");
            } else {
                out.push(c);
            }
        }
        for b in chunk.invalid() {
            open(&mut out, true);
            out.push_str(&format!("\\x{b:02x}"));
        }
    }
    open(&mut out, false);
    out
}

/// A path as trash-cli prints it in an error line. With `repr`, as
/// Python's `repr()` of the name: quoted (in double quotes when it holds a
/// `'` and no `"`), with backslashes, that quote and control characters
/// escaped, and a byte that is not UTF-8 as `\udcNN`. Without, as
/// trash-cli's printable(): the text as is, and such a byte as `\xNN`.
fn py_text(p: &std::path::Path, repr: bool) -> String {
    use std::os::unix::ffi::OsStrExt;
    let bytes = p.as_os_str().as_bytes();
    let lossy = String::from_utf8_lossy(bytes);
    let quote = if lossy.contains('\'') && !lossy.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut body = String::new();
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            let cp = c as u32;
            match c {
                _ if !repr => body.push(c),
                '\\' => body.push_str("\\\\"),
                '\t' => body.push_str("\\t"),
                '\n' => body.push_str("\\n"),
                '\r' => body.push_str("\\r"),
                _ if c == quote => {
                    body.push('\\');
                    body.push(c);
                }
                _ if cp < 0x20 || (0x7f..=0x9f).contains(&cp) => {
                    body.push_str(&format!("\\x{cp:02x}"));
                }
                _ => body.push(c),
            }
        }
        for b in chunk.invalid() {
            if repr {
                body.push_str(&format!("\\udc{b:02x}"));
            } else {
                body.push_str(&format!("\\x{b:02x}"));
            }
        }
    }
    if repr {
        format!("{quote}{body}{quote}")
    } else {
        body
    }
}

/// Payloads in `files/` with no `.trashinfo` (left by a crash or another
/// tool), as entries with no date whose original path is the payload
/// itself, oldest-first order kept by name.
pub fn orphans(dirs: &[TrashDir]) -> Vec<Entry> {
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(rd) = fs::read_dir(dir.files()) else {
            continue;
        };
        let info_dir = dir.info();
        let mut found: Vec<Entry> = rd
            .flatten()
            .filter_map(|ent| {
                let name = crate::util::str_from_os_ref(&ent.file_name());
                let info = crate::util::join_info(&info_dir, &name);
                // symlink_metadata: a dangling .trashinfo symlink still counts.
                if fs::symlink_metadata(&info).is_ok() {
                    return None;
                }
                Some(Entry {
                    date: None,
                    epoch: 0,
                    original: ent.path(),
                    name,
                    dir: dir.clone(),
                    batch: None,
                })
            })
            .collect();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        out.extend(found);
    }
    out
}

/// When `entry` was trashed, from its `.trashinfo` date, if readable.
fn entry_epoch(entry: &Entry) -> Option<i64> {
    entry.date.as_deref().and_then(info::parse_local_to_epoch)
}

/// Keep entries trashed at least `older` seconds ago and less than `newer`
/// seconds ago (both when both are set). A missing or unreadable date
/// never passes an age filter.
pub fn filter_age_secs(entries: Vec<Entry>, older: Option<i64>, newer: Option<i64>) -> Vec<Entry> {
    let age = crate::filters::AgeFilter {
        older_than: older,
        newer_than: newer,
    };
    let now = info::now_epoch();
    entries
        .into_iter()
        .filter(|e| age.keeps_at(entry_epoch(e), now))
        .collect()
}

pub use crate::filters::SizeFilter;

pub use crate::listfmt::{parse_format, render_format, Field, FmtPart, FmtRow};
pub use crate::util::json_escape;

pub fn entry_to_json(e: &Entry) -> String {
    entry_to_json_sized(e, None)
}

/// [`entry_to_json`] with a `size` field when `size` is known.
pub fn entry_to_json_sized(e: &Entry, size: Option<u64>) -> String {
    let size = size.map(|n| format!(",\"size\":{n}")).unwrap_or_default();
    let date = e.date.as_deref().unwrap_or("");
    let batch = match &e.batch {
        Some(b) => format!("\"{}\"", json_escape(b)),
        None => "null".to_string(),
    };
    format!(
        "{{\"deletion_date\":\"{}\",\"original\":\"{}\",\"name\":\"{}\",\"trash_dir\":\"{}\",\"batch\":{},\"missing\":{}{}}}",
        json_escape(date),
        json_escape(&e.original.to_string_lossy()),
        json_escape(&crate::util::show_name(&e.name)),
        json_escape(&e.dir.root.to_string_lossy()),
        batch,
        e.missing(),
        size,
    )
}

pub fn entries_to_json_array(entries: &[Entry]) -> String {
    let refs: Vec<&Entry> = entries.iter().collect();
    sized_json_array(&refs, None)
}

/// JSON array of `entries`, with `sizes[i]` as entry `i`'s size when given.
fn sized_json_array(entries: &[&Entry], sizes: Option<&[u64]>) -> String {
    let mut out = String::from("[\n");
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            out.push_str(",\n");
        }
        out.push_str("  ");
        out.push_str(&entry_to_json_sized(e, sizes.map(|s| s[i])));
    }
    if !entries.is_empty() {
        out.push('\n');
    }
    out.push(']');
    out
}

pub fn run(prog: &str, args: &[String]) -> i32 {
    let mut trash_dirs: Vec<PathBuf> = Vec::new();
    let mut home_only = false;
    let mut all_users = false;
    let mut json = false;
    let mut volumes = false;
    let mut show_dirs = false;
    let mut older_than: Option<i64> = None;
    let mut newer_than: Option<i64> = None;
    // Defaults from the config file; options override them.
    let cfg = crate::config::get();
    let mut with_size = cfg.bool("list", "size").unwrap_or(false);
    let mut show_trash = false;
    // trash-cli's trash-list shapes: --size (bytes in place of the date)
    // and its hidden --files (` -> FILE`, the payload in the trash).
    let mut size_flag = false;
    let mut show_files = false;
    let mut sort = SortKey::Date;
    if let Some(s) = cfg.str("list", "sort") {
        match SortKey::parse(s) {
            Some(k) => sort = k,
            None => eprintln!(
                "{prog}: warning: {}",
                cfg.bad_value("list", "sort", "date, size, name or path")
            ),
        }
    }
    let mut reverse = cfg.bool("list", "reverse").unwrap_or(false);
    let mut size_filter = SizeFilter::default();
    let mut matcher: Option<crate::restore::Matcher> = None;
    let mut cwd_only = false;
    let mut directory: Option<PathBuf> = None;
    let mut last: Option<usize> = None;
    let mut depth = 0usize;
    let mut format: Option<Vec<FmtPart>> = None;
    let mut i = 0usize;
    while i < args.len() {
        match size_filter.take(args, &mut i) {
            Some(Ok(())) => {
                i += 1;
                continue;
            }
            Some(Err(e)) => {
                eprintln!("{prog}: {e}");
                return 2;
            }
            None => {}
        }
        let arg = &args[i];
        match arg.as_str() {
            "--home-only" => home_only = true,
            "--all-users" => all_users = true,
            "--json" => json = true,
            a if a.starts_with("--format=") => match parse_format(&a["--format=".len()..]) {
                Ok(f) => format = Some(f),
                Err(e) => {
                    eprintln!("{prog}: {e}");
                    return 2;
                }
            },
            "--volumes" => volumes = true,
            "--trash-dirs" => show_dirs = true,
            "--size" => {
                with_size = true;
                size_flag = true;
            }
            "--files" if prog == "trash-list" => show_files = true,
            "--show-trashpath" => show_trash = true,
            "--reverse" => reverse = true,
            "--cwd-only" => cwd_only = true,
            a if a.starts_with("--directory=") => {
                let d = &a["--directory=".len()..];
                if d.is_empty() {
                    eprintln!("{prog}: --directory needs a non-empty DIR");
                    return 2;
                }
                let d = crate::util::arg_path(d);
                directory = Some(std::path::absolute(&d).unwrap_or(d));
            }
            a if a.starts_with("--depth=") => match a["--depth=".len()..].parse::<usize>() {
                Ok(n) => depth = n,
                Err(_) => {
                    eprintln!(
                        "{prog}: --depth needs a count of levels, not '{}'",
                        &a["--depth=".len()..]
                    );
                    return 2;
                }
            },
            a if a.starts_with("--last=") => match a["--last=".len()..].parse::<usize>() {
                Ok(n) => last = Some(n),
                Err(_) => {
                    eprintln!(
                        "{prog}: --last needs a count of rows, not '{}'",
                        &a["--last=".len()..]
                    );
                    return 2;
                }
            },
            a if a.starts_with("--glob=")
                || a.starts_with("--regex=")
                || a.starts_with("--name=") =>
            {
                if matcher.is_some() {
                    eprintln!("{prog}: give only one of --glob, --regex and --name");
                    return 2;
                }
                let (key, val) = a.split_once('=').unwrap_or((a, ""));
                if val.is_empty() {
                    eprintln!("{prog}: {key} needs a non-empty value");
                    return 2;
                }
                let val = crate::finder_shared::nfc(val);
                matcher = Some(match key {
                    "--glob" => crate::restore::Matcher::Glob(val),
                    "--name" => crate::restore::Matcher::Name(val),
                    _ => match regex_lite::Regex::new(&val) {
                        Ok(re) => crate::restore::Matcher::Regex(re),
                        Err(e) => {
                            eprintln!("{prog}: invalid --regex '{val}': {e}");
                            return 2;
                        }
                    },
                });
            }
            a if a.starts_with("--sort=") => match SortKey::parse(&a["--sort=".len()..]) {
                Some(k) => sort = k,
                None => {
                    eprintln!(
                        "{prog}: invalid --sort '{}' (date, size, name or path)",
                        &a["--sort=".len()..]
                    );
                    return 2;
                }
            },
            "--help" => {
                print!("{}", HELP.replace("{prog}", prog));
                return 0;
            }
            "--version" => {
                println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                return 0;
            }
            a if a.starts_with("--trash-dir=") => {
                trash_dirs.push(crate::util::arg_path(&a["--trash-dir=".len()..]));
            }
            a if a.starts_with("--older-than=") => {
                match crate::filters::parse_age_secs(&a["--older-than=".len()..]) {
                    Ok(d) => older_than = Some(d),
                    Err(e) => {
                        eprintln!("{prog}: {e}");
                        return 2;
                    }
                }
            }
            a if a.starts_with("--newer-than=") => {
                match crate::filters::parse_age_secs(&a["--newer-than=".len()..]) {
                    Ok(d) => newer_than = Some(d),
                    Err(e) => {
                        eprintln!("{prog}: {e}");
                        return 2;
                    }
                }
            }
            "--older-than" | "--newer-than" => {
                let name = arg.as_str();
                i += 1;
                let Some(val) = args.get(i) else {
                    eprintln!("{prog}: {name} requires an AGE");
                    return 2;
                };
                match crate::filters::parse_age_secs(val) {
                    Ok(d) if name == "--older-than" => older_than = Some(d),
                    Ok(d) => newer_than = Some(d),
                    Err(e) => {
                        eprintln!("{prog}: {e}");
                        return 2;
                    }
                }
            }
            a if !a.starts_with('-') => {
                // A bare PATTERN: a glob if it has one, else a piece of the
                // file name, as `gtrash find` and `fd` take it.
                if matcher.is_some() {
                    eprintln!("{prog}: give only one of PATTERN, --glob, --regex and --name");
                    return 2;
                }
                let p = crate::finder_shared::nfc(a);
                let p = if p.contains(['*', '?', '[']) {
                    p
                } else {
                    format!("*{p}*")
                };
                matcher = Some(crate::restore::Matcher::Glob(p));
            }
            a => {
                eprintln!("{prog}: unrecognized option '{a}'");
                eprintln!("Try '{prog} --help' for more information.");
                return 2;
            }
        }
        i += 1;
    }
    if let Err(e) = size_filter.check() {
        eprintln!("{prog}: {e}");
        return 2;
    }
    if cwd_only && directory.is_some() {
        eprintln!("{prog}: give --cwd-only or --directory, not both");
        return 2;
    }
    let cwd = if let Some(d) = directory {
        Some(d)
    } else if cwd_only {
        match std::env::current_dir() {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("{prog}: cannot read the current directory: {e}");
                return 2;
            }
        }
    } else {
        None
    };
    if json && format.is_some() {
        eprintln!("{prog}: give --json or --format, not both");
        return 2;
    }
    if show_trash && format.is_some() {
        eprintln!("{prog}: --show-trashpath works on plain rows and --json, not --format");
        return 2;
    }
    // Under the trash-list name, plain rows keep trash-cli's shape.
    let cli_rows = prog == "trash-list" && !json && format.is_none();
    if show_files && !cli_rows {
        eprintln!("{prog}: --files works on plain rows, not --json or --format");
        return 2;
    }
    // Those rows show no disk usage, so only --sort=size needs it.
    if cli_rows {
        with_size = false;
    }
    if depth > 0 && (json || format.is_some()) {
        eprintln!("{prog}: --depth works on plain rows, not --json or --format");
        return 2;
    }
    // --glob/--regex/--name and --cwd-only, on the original path and the
    // name in the trash.
    let keep = |original: &std::path::Path, name: &str| -> bool {
        matcher
            .as_ref()
            .is_none_or(|m| m.matches_parts(original, name))
            && cwd.as_ref().is_none_or(|d| {
                original.starts_with(d)
                    || std::fs::canonicalize(d).is_ok_and(|r| original.starts_with(r))
            })
    };
    // macOS lists the Finder Trash unless a pin or the setting says otherwise.
    if crate::finder::is_default(!trash_dirs.is_empty() || all_users) {
        let refused = if volumes || show_dirs {
            Some("--volumes or --trash-dirs")
        } else if show_files {
            Some("--files")
        } else {
            None
        };
        let filter = crate::finder_shared::Filter {
            age: crate::filters::AgeFilter {
                older_than,
                newer_than,
            },
            size: size_filter,
        };
        let shape = crate::finder::ListShape {
            json,
            format: format.as_deref(),
            last,
            depth,
            show_trash,
        };
        return crate::finder::default_list(prog, shape, refused, filter, &keep);
    }
    if all_users && (home_only || !trash_dirs.is_empty()) {
        eprintln!("{prog}: --all-users takes no --home-only or --trash-dir");
        return 2;
    }
    let dirs = if all_users {
        trashdir::all_users()
    } else {
        trashdir::resolve_dirs_opts(&trash_dirs, home_only)
    };
    if dirs.is_empty() && !trash_dirs.is_empty() {
        eprintln!("{prog}: no valid --trash-dir pins");
        return 2;
    }
    // trash-cli's --volumes and --trash-dirs print the places looked at.
    if volumes || show_dirs {
        if volumes {
            for m in trashdir::mount_points() {
                println!("{}", m.display());
            }
        }
        if show_dirs {
            for d in &dirs {
                println!("{}", d.root.display());
            }
        }
        return 0;
    }
    let Listing {
        entries: collected,
        left,
        strays,
    } = collect_counted(&dirs);
    // The trash-list name prints trash-cli's own line for each record
    // trash-cli reports, and its hints count only the rest; rtrash list
    // counts them all.
    let trash_cli = prog == "trash-list";
    if trash_cli {
        for line in left.iter().filter_map(|u| u.trash_cli.as_deref()) {
            eprintln!("{}", crate::util::err_text(line));
        }
    }
    let hints = folder_hints(prog, &dirs, &left, &strays, trash_cli);
    let mut unsized_rows = 0usize;
    let mut entries = filter_age_secs(collected, older_than, newer_than);
    entries.retain(|e| keep(&e.original, &e.name));
    // Orphans have no date, so only an unfiltered listing shows them; they
    // come last, after every recorded item.
    let mut recorded = entries.len();
    if older_than.is_none() && newer_than.is_none() {
        entries.extend(
            orphans(&dirs)
                .into_iter()
                .filter(|e| keep(&e.original, &e.name)),
        );
    }
    let with_size = with_size
        || sort == SortKey::Size
        || format
            .as_ref()
            .is_some_and(|f| f.iter().any(|p| matches!(p, FmtPart::Field(Field::Size))));
    let mut sizes = (with_size || size_filter.is_set()).then(|| entry_sizes(&dirs, &entries));
    if let (true, Some(all)) = (size_filter.is_set(), sizes.as_mut()) {
        let keep: Vec<bool> = all.iter().map(|&n| size_filter.keeps(n)).collect();
        recorded = keep[..recorded].iter().filter(|&&k| k).count();
        let mut k = keep.iter();
        entries.retain(|_| *k.next().unwrap_or(&false));
        let mut k = keep.iter();
        all.retain(|_| *k.next().unwrap_or(&false));
    }
    let sizes = sizes.filter(|_| with_size);
    let mut order = sorted_order(&entries, sizes.as_deref(), sort, reverse);
    if sort == SortKey::Date {
        // Keep orphans after the recorded items whichever way dates run.
        order.sort_by_key(|&i| i >= recorded);
    }
    // --last keeps the tail of the listing, as tail -n would.
    if let Some(n) = last {
        let cut = order.len().saturating_sub(n);
        order.drain(..cut);
    }
    let orphan_rows: Vec<bool> = order.iter().map(|&i| i >= recorded).collect();
    let rows: Vec<&Entry> = order.iter().map(|&i| &entries[i]).collect();
    let row_sizes: Option<Vec<u64>> = sizes.map(|s| order.iter().map(|&i| s[i]).collect());
    // Owner of each row's trash root, looked up once per root.
    // The trash-list name keeps trash-cli's plain rows, without OWNER.
    let owners: Option<Vec<String>> = (all_users && (json || prog != "trash-list")).then(|| {
        let mut by_root: std::collections::HashMap<&std::path::Path, String> =
            std::collections::HashMap::new();
        rows.iter()
            .map(|e| {
                by_root
                    .entry(e.dir.root.as_path())
                    .or_insert_with(|| trashdir::owner_name(&e.dir.root))
                    .clone()
            })
            .collect()
    });
    if let Some(parts) = &format {
        use std::io::Write;
        let mut out = std::io::BufWriter::new(std::io::stdout().lock());
        for (k, e) in rows.iter().enumerate() {
            let row = FmtRow {
                date: e.date.as_deref().unwrap_or(""),
                epoch: e.epoch,
                path: &e.original,
                name: &e.name,
                trash: &e.dir.root,
                batch: e.batch.as_deref().unwrap_or(""),
                size: row_sizes.as_ref().map(|s| s[k]),
                owner: owners.as_ref().map(|o| o[k].as_str()),
            };
            if out.write_all(&render_format(parts, &row)).is_err() {
                return 0;
            }
        }
        let _ = out.flush();
    } else if json {
        let items: Vec<String> = rows
            .iter()
            .enumerate()
            .map(|(k, e)| {
                let obj = entry_to_json_sized(e, row_sizes.as_ref().map(|s| s[k]));
                let mut body = obj.strip_suffix('}').unwrap_or(&obj).to_string();
                if orphan_rows[k] {
                    body.push_str(",\"orphan\":true");
                }
                if let Some(owners) = &owners {
                    body.push_str(&format!(",\"owner\":\"{}\"", json_escape(&owners[k])));
                }
                if show_trash {
                    let payload = crate::util::join_name(&e.dir.files(), &e.name);
                    let payload = json_escape(&payload.to_string_lossy());
                    body.push_str(&format!(",\"trash_path\":\"{payload}\""));
                }
                format!("  {body}}}")
            })
            .collect();
        if items.is_empty() {
            println!("[\n]");
        } else {
            println!("[\n{}\n]", items.join(",\n"));
        }
    } else if cli_rows {
        use std::io::Write;
        let mut out = std::io::BufWriter::new(std::io::stdout().lock());
        for entry in &rows {
            let payload = crate::util::join_name(&entry.dir.files(), &entry.name);
            let attr = if size_flag {
                match trash_cli_size(&payload) {
                    Ok(n) => n.to_string(),
                    Err(e) => {
                        // trash-cli stops with a traceback here; say which
                        // payload and go on with the rest.
                        let _ = out.flush();
                        eprintln!(
                            "{prog}: cannot read the size of {}: {e}",
                            crate::util::err_path(&payload)
                        );
                        unsized_rows += 1;
                        continue;
                    }
                }
            } else {
                let date = entry.date.as_deref().unwrap_or("????-??-??T??:??:??");
                crate::util::term_text(date).replacen('T', " ", 1)
            };
            let mut line = format!("{attr} {}", cli_row_path(&entry.original));
            if show_files {
                line.push_str(" -> ");
                line.push_str(&cli_row_path(&payload));
            }
            let res = writeln!(out, "{line}");
            let res = res.and_then(|()| crate::listfmt::trash_line(&mut out, &payload, show_trash));
            let res = res.and_then(|()| write_tree(&mut out, &payload, depth));
            if res.is_err() {
                return 0;
            }
        }
        let _ = out.flush();
    } else {
        use std::io::Write;
        let mut out = std::io::BufWriter::new(std::io::stdout().lock());
        for (k, entry) in rows.iter().enumerate() {
            let date = entry.date.as_deref().unwrap_or("????-??-??T??:??:??");
            let mut date = crate::util::term_text(date).replacen('T', " ", 1);
            if let Some(o) = &owners {
                date = format!("{date} {:<8}", crate::util::term_text(&o[k]));
            }
            let res = match &row_sizes {
                Some(s) => writeln!(
                    out,
                    "{date} {:>10} {}",
                    crate::util::format_bytes(s[k]),
                    crate::util::term_path(&entry.original)
                ),
                None => writeln!(out, "{date} {}", crate::util::term_path(&entry.original)),
            };
            let payload = crate::util::join_name(&entry.dir.files(), &entry.name);
            let res = res.and_then(|()| crate::listfmt::trash_line(&mut out, &payload, show_trash));
            let res = res.and_then(|()| write_tree(&mut out, &payload, depth));
            if res.is_err() {
                return 0;
            }
        }
        let _ = out.flush();
    }
    // After the rows, on stderr, so stdout is the same with or without
    // them. trash-list prints them too, as on Windows, for what trash-cli
    // itself would list or not report.
    for hint in hints {
        let hint = crate::util::err_text(&hint);
        eprintln!("{hint}");
    }
    i32::from(unsized_rows > 0)
}

/// One stderr line per trash with something `list` could not show, in the
/// order of `dirs` (trash-cli #428 proposes one line per trash folder in
/// place of a line per file). Under the trash-list name, a record with a
/// line of trash-cli's own and the names that are not records are left
/// out, since trash-cli 0.26.9.29 says nothing more about either.
fn folder_hints(
    prog: &str,
    dirs: &[TrashDir],
    left: &[Unlisted],
    strays: &[u64],
    trash_cli: bool,
) -> Vec<String> {
    dirs.iter()
        .zip(strays)
        .filter_map(|(dir, &strays)| {
            let records = left
                .iter()
                .filter(|u| u.root == dir.root && !(trash_cli && u.trash_cli.is_some()))
                .count() as u64;
            let strays = if trash_cli { 0 } else { strays };
            folder_hint(prog, &dir.root, records, strays)
        })
        .collect()
}

/// The line [`folder_hints`] prints for the trash at `root`: `records`
/// records it left out and `strays` names in `info/` that are not
/// records, all of which `rtrash status --check` names. None when both
/// are 0.
fn folder_hint(prog: &str, root: &std::path::Path, records: u64, strays: u64) -> Option<String> {
    let records_part = (records > 0).then(|| {
        format!(
            "{} could not be listed",
            crate::util::plural(records, "trash record", "trash records")
        )
    });
    let strays_part = (strays > 0).then(|| match strays {
        1 => "1 name in info/ is not a trash record".to_string(),
        n => format!("{n} names in info/ are not trash records"),
    });
    let what = match (records_part, strays_part) {
        (Some(r), Some(s)) => format!("{r}, and {s}"),
        (Some(one), None) | (None, Some(one)) => one,
        (None, None) => return None,
    };
    let it = if records + strays == 1 { "it" } else { "them" };
    Some(format!(
        "{prog}: {}: {what}; `rtrash status --check` shows {it}",
        root.display()
    ))
}

/// The size trash-cli's `trash-list --size` shows for a payload: its
/// `st_size` through a symlink, so a directory gives its own entry size,
/// not what it holds, and a symlink whose target is gone gives 0.
fn trash_cli_size(payload: &std::path::Path) -> std::io::Result<u64> {
    match fs::metadata(payload) {
        Ok(m) => Ok(m.len()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && payload.is_symlink() => Ok(0),
        Err(e) => Err(e),
    }
}

/// `--depth=N`: what a trashed directory holds, N levels down, one entry
/// per line indented by level, directories with a trailing separator and
/// names in order. Symlinks are not followed; what cannot be read is left
/// out. A plain file, or `depth` 0, prints nothing.
pub(crate) fn write_tree(
    out: &mut impl std::io::Write,
    dir: &std::path::Path,
    depth: usize,
) -> std::io::Result<()> {
    fn walk(
        out: &mut impl std::io::Write,
        dir: &std::path::Path,
        level: usize,
        depth: usize,
    ) -> std::io::Result<()> {
        let Ok(rd) = fs::read_dir(dir) else {
            return Ok(());
        };
        let mut kids: Vec<(String, std::path::PathBuf, bool)> = rd
            .flatten()
            .map(|e| {
                let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
                (
                    e.file_name().to_string_lossy().into_owned(),
                    e.path(),
                    is_dir,
                )
            })
            .collect();
        kids.sort();
        for (name, path, is_dir) in kids {
            let slash = if is_dir { "/" } else { "" };
            let indent = "  ".repeat(level);
            writeln!(out, "    {indent}{}{slash}", crate::util::term_text(&name))?;
            if is_dir && level + 1 < depth {
                walk(out, &path, level + 1, depth)?;
            }
        }
        Ok(())
    }
    let is_dir = fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir());
    if depth == 0 || !is_dir {
        return Ok(());
    }
    walk(out, dir, 0, depth)
}

/// What `list --sort` orders by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Date,
    Size,
    Name,
    Path,
}

impl SortKey {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "date" | "time" => SortKey::Date,
            "size" => SortKey::Size,
            "name" => SortKey::Name,
            "path" => SortKey::Path,
            _ => return None,
        })
    }
}

/// Disk usage of each entry, payload plus `.trashinfo`, through each root's
/// size cache; walked directories go back into the cache.
fn entry_sizes(dirs: &[TrashDir], entries: &[Entry]) -> Vec<u64> {
    use rayon::prelude::*;
    let caches: Vec<trashdir::DirSizes> = dirs.iter().map(trashdir::DirSizes::load).collect();
    let sizes = entries
        .par_iter()
        .map(|e| {
            caches
                .iter()
                .zip(dirs)
                .find(|(_, d)| d.root == e.dir.root)
                .map_or(0, |(c, _)| c.entry_bytes(&e.name))
        })
        .collect();
    for c in caches {
        c.write_back();
    }
    sizes
}

/// Row order for `entries` (already oldest first) under `key`. Ties keep
/// the date order, so equal sizes or names stay oldest first.
pub fn sorted_order(
    entries: &[Entry],
    sizes: Option<&[u64]>,
    key: SortKey,
    reverse: bool,
) -> Vec<usize> {
    let mut order: Vec<usize> = (0..entries.len()).collect();
    let file_name = |i: usize| entries[i].original.file_name().unwrap_or_default();
    match key {
        SortKey::Date => {}
        SortKey::Size => {
            let sizes = sizes.unwrap_or(&[]);
            order.sort_by_key(|&i| std::cmp::Reverse(sizes.get(i).copied().unwrap_or(0)));
        }
        SortKey::Name => order.sort_by(|&a, &b| file_name(a).cmp(file_name(b))),
        SortKey::Path => order.sort_by(|&a, &b| entries[a].original.cmp(&entries[b].original)),
    }
    if reverse {
        order.reverse();
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trashdir::TrashDir;
    use std::path::PathBuf;

    /// A record with no `Path=`, or an empty one, still reaches rm,
    /// restore and the totals through `collect`, but `list` leaves it out
    /// and counts it, as `status --check` flags it.
    #[test]
    fn list_leaves_out_and_counts_records_with_no_path() {
        let root = std::env::temp_dir().join(format!("rtrash-pathless-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("info")).unwrap();
        fs::create_dir_all(root.join("files")).unwrap();
        let date = "DeletionDate=2026-01-02T03:04:05\n";
        let rec = |name: &str, body: &str| {
            fs::write(root.join("files").join(name), b"x").unwrap();
            let info = root.join("info").join(format!("{name}.trashinfo"));
            fs::write(info, format!("[Trash Info]\n{body}{date}")).unwrap();
        };
        rec("good.txt", "Path=/tmp/good.txt\n");
        rec("none.txt", "");
        rec("empty.txt", "Path=\n");
        let dir = TrashDir {
            root: root.clone(),
            topdir: None,
        };
        let dirs = [dir];
        let mut all: Vec<String> = collect(&dirs).into_iter().map(|e| e.name).collect();
        all.sort();
        assert_eq!(all, ["empty.txt", "good.txt", "none.txt"]);
        let Listing {
            entries: listed,
            left,
            strays,
        } = collect_counted(&dirs);
        assert_eq!(strays, [0]);
        let skipped = left.len() as u64;
        // trash-cli reports the record with no Path line, and lists the
        // one with an empty Path= as its volume.
        let lines: Vec<Option<&str>> = left.iter().map(|u| u.trash_cli.as_deref()).collect();
        let none = root.join("info/none.txt.trashinfo");
        let want = format!("Parse Error: {}: Unable to parse Path.", none.display());
        assert_eq!(lines, [None, Some(want.as_str())]);
        let names: Vec<&str> = listed.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["good.txt"]);
        assert_eq!(skipped, 2);
        let flagged = crate::trash_check::scan(&dirs[0]);
        let bad = flagged
            .iter()
            .filter(|p| matches!(p, crate::trash_check::Problem::BadRecord { .. }))
            .count();
        assert_eq!(bad as u64, skipped, "{flagged:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn trash_cli_size_reads_the_entry_through_a_link() {
        let root = std::env::temp_dir().join(format!("rtrash-tl-size-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("d/sub")).unwrap();
        fs::write(root.join("a"), b"hello\n").unwrap();
        fs::write(root.join("d/sub/big"), vec![0u8; 5000]).unwrap();
        std::os::unix::fs::symlink("a", root.join("good")).unwrap();
        std::os::unix::fs::symlink("nowhere", root.join("dangling")).unwrap();
        assert_eq!(trash_cli_size(&root.join("a")).unwrap(), 6);
        assert_eq!(trash_cli_size(&root.join("good")).unwrap(), 6);
        assert_eq!(trash_cli_size(&root.join("dangling")).unwrap(), 0);
        let own = fs::metadata(root.join("d")).unwrap().len();
        assert_eq!(trash_cli_size(&root.join("d")).unwrap(), own);
        let gone = trash_cli_size(&root.join("gone")).unwrap_err();
        assert_eq!(gone.kind(), std::io::ErrorKind::NotFound);
        let _ = fs::remove_dir_all(&root);
    }

    /// trash-list rows on a terminal quote names as trash-cli 0.26.9's
    /// shell_escape does; each expected string is what that function and
    /// its printable() give for the same bytes.
    #[test]
    fn trash_list_rows_quote_names_as_trash_cli_does() {
        use std::os::unix::ffi::OsStrExt;
        let q = |b: &[u8]| shell_escape(std::path::Path::new(std::ffi::OsStr::from_bytes(b)));
        let cases: [(&[u8], &str); 12] = [
            (b"/t/plain-1.txt", "/t/plain-1.txt"),
            (b"/t/a b", "'/t/a b'"),
            (b"/t/it's", r"'/t/it'\''s'"),
            (b"/t/x\ny", r"'/t/x'$'\n''y'"),
            (b"/t/tab\there", r"'/t/tab'$'\t''here'"),
            (b"/t/esc\x1b[31m", r"'/t/esc'$'\033''[31m'"),
            (b"/t/\x7f", r"'/t/'$'\177'"),
            ("/t/\u{85}é".as_bytes(), "'/t/'$'\\205''é'"),
            (b"/t/\xff", r"'/t/\xff'"),
            (b"", ""),
            (b"/t/a'b c", r"'/t/a'\''b c'"),
            // trash-cli leaves a bidi override raw; rtrash escapes it.
            ("/t/a\u{202e}b".as_bytes(), r"'/t/a'$'\u202e''b'"),
        ];
        for (bytes, want) in cases {
            assert_eq!(q(bytes), want, "{:?}", String::from_utf8_lossy(bytes));
        }
        // In a pipe the row keeps the name, with such a byte as \xNN.
        let raw = std::path::Path::new(std::ffi::OsStr::from_bytes(b"/t/x\ny\xff"));
        assert_eq!(py_text(raw, false), "/t/x\ny\\xff");
    }

    /// The stderr lines trash-cli's trash-list prints for a record it
    /// cannot read, with the name as Python's repr() quotes it.
    #[test]
    fn trash_cli_error_lines_quote_the_record_as_python_does() {
        use std::os::unix::ffi::OsStrExt;
        let p = |s: &str| py_text(std::path::Path::new(s), true);
        assert_eq!(p("/t/a b"), "'/t/a b'");
        assert_eq!(p("/t/it's"), "\"/t/it's\"");
        assert_eq!(p("/t/a'\"b"), "'/t/a\\'\"b'");
        assert_eq!(p("/t/x\ny"), "'/t/x\\ny'");
        assert_eq!(p("/t/a\\b"), "'/t/a\\\\b'");
        assert_eq!(p("/t/\u{7f}\u{85}é"), "'/t/\\x7f\\x85é'");
        let raw = std::path::Path::new(std::ffi::OsStr::from_bytes(b"/t/\xff"));
        assert_eq!(py_text(raw, true), "'/t/\\udcff'");
        assert_eq!(py_text(raw, false), "/t/\\xff");

        let root = std::env::temp_dir().join(format!("rtrash-tl-errs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("dir.trashinfo")).unwrap();
        std::os::unix::fs::symlink("nowhere", root.join("dangling.trashinfo")).unwrap();
        fs::write(root.join("big.trashinfo"), vec![b'x'; (1 << 20) + 1]).unwrap();
        let line = |name: &str| {
            let path = root.join(name);
            let e = info::read_bounded(&path, info::RECORD_CAP).unwrap_err();
            trash_cli_read_line(&e, &path)
        };
        let dir = line("dir.trashinfo").unwrap();
        let shown = format!(": '{}'", root.join("dir.trashinfo").display());
        assert!(
            dir.starts_with("[Errno 21] ") && dir.ends_with(&shown),
            "{dir}"
        );
        assert_eq!(
            line("dangling.trashinfo").unwrap(),
            format!(
                "[Errno 2] No such file or directory: '{}'",
                root.join("dangling.trashinfo").display()
            )
        );
        assert_eq!(
            line("big.trashinfo"),
            None,
            "trash-cli reads it; only the hint counts it"
        );
        let _ = fs::remove_dir_all(&root);
    }

    fn entry_with(date: Option<&str>, epoch: i64) -> Entry {
        Entry {
            date: date.map(str::to_string),
            epoch,
            original: PathBuf::from("/tmp/x"),
            name: "x".into(),
            dir: TrashDir {
                root: PathBuf::from("/tmp/Trash"),
                topdir: None,
            },
            batch: None,
        }
    }

    #[test]
    fn json_escape_quotes_and_controls() {
        assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
        assert!(json_escape("ok").contains("ok"));
    }

    #[test]
    fn age_filters_leave_out_a_missing_date() {
        let e = || entry_with(None, 0);
        assert!(filter_age_secs(vec![e()], Some(0), None).is_empty());
        assert!(filter_age_secs(vec![e()], None, Some(365 * 86_400)).is_empty());
    }

    #[test]
    fn age_newer_recent() {
        let now = info::now_epoch();
        let e = || entry_with(Some(&info::now_local_string()), now);
        assert_eq!(filter_age_secs(vec![e()], None, Some(3600)).len(), 1);
        assert!(filter_age_secs(vec![e()], Some(3600), None).is_empty());
    }

    #[test]
    fn filter_age_and() {
        let now = info::now_epoch();
        let recent = entry_with(Some(&info::now_local_string()), now);
        let old = entry_with(Some("2001-01-01T00:00:00"), 978_307_200);
        let out = filter_age_secs(vec![recent, old], Some(30 * 86_400), None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].epoch, 978_307_200);
    }

    #[test]
    fn sort_keys_order_rows_and_reverse() {
        let mk = |path: &str, epoch: i64| Entry {
            original: PathBuf::from(path),
            ..entry_with(None, epoch)
        };
        let es = vec![mk("/b/zeta", 1), mk("/a/beta", 2), mk("/c/alpha", 3)];
        let sizes = [10, 30, 20];
        assert_eq!(sorted_order(&es, None, SortKey::Date, false), [0, 1, 2]);
        assert_eq!(
            sorted_order(&es, Some(&sizes), SortKey::Size, false),
            [1, 2, 0]
        );
        assert_eq!(sorted_order(&es, None, SortKey::Name, false), [2, 1, 0]);
        assert_eq!(sorted_order(&es, None, SortKey::Path, true), [2, 0, 1]);
        let s = entry_to_json_sized(&es[0], Some(42));
        assert!(s.ends_with(",\"size\":42}"), "{s}");
    }

    #[test]
    fn entries_json_array_shape() {
        let e = entry_with(Some("2020-01-02T03:04:05"), 1);
        let s = entries_to_json_array(std::slice::from_ref(&e));
        assert!(s.starts_with('['));
        assert!(s.contains("\"deletion_date\":\"2020-01-02T03:04:05\""));
        assert!(s.contains("\"original\":\"/tmp/x\""));
        assert!(s.ends_with(']'));
    }

    /// relock hands back an entry only while its record reads as the scan
    /// read it, and holds the trash lock while it is kept (round 244).
    #[test]
    fn relock_refuses_an_entry_whose_name_another_item_took() {
        let root = std::env::temp_dir().join(format!("rtrash-relock-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("info")).unwrap();
        fs::create_dir_all(root.join("files")).unwrap();
        let record = root.join("info/a.txt.trashinfo");
        let body = |batch: &str| {
            format!(
                "[Trash Info]\nPath=/tmp/a.txt\nDeletionDate=2026-01-02T03:04:05\n\
                 X-Rtrash-Batch={batch}\n"
            )
        };
        fs::write(root.join("files/a.txt"), b"old").unwrap();
        fs::write(&record, body("1-1")).unwrap();
        fs::write(root.join("files/stray"), b"s").unwrap();
        let dirs = [TrashDir {
            root: root.clone(),
            topdir: None,
        }];
        let seen = collect(&dirs);
        let strays = orphans(&dirs);
        assert_eq!(seen.len(), 1);
        assert_eq!(strays.len(), 1);
        let held = seen[0].relock().unwrap();
        assert_eq!(held.payload(), root.join("files/a.txt"));
        assert!(trashdir::TrashLock::try_acquire(&root).is_err());
        drop(held);
        assert!(strays[0].relock().is_ok());
        // Another put took the name: the same path and second, its own batch.
        fs::write(&record, body("2-2")).unwrap();
        assert!(matches!(seen[0].relock(), Err(Relock::Changed)));
        // A record whose payload went still passes, for rm to take.
        fs::write(&record, body("1-1")).unwrap();
        fs::remove_file(root.join("files/a.txt")).unwrap();
        assert!(seen[0].relock().unwrap().gone());
        // Record and payload both went.
        fs::remove_file(&record).unwrap();
        assert!(matches!(seen[0].relock(), Err(Relock::Gone)));
        // An orphan that gained a record is another item.
        fs::write(root.join("info/stray.trashinfo"), body("3-3")).unwrap();
        assert!(matches!(strays[0].relock(), Err(Relock::Changed)));
        let _ = fs::remove_dir_all(&root);
    }
}
