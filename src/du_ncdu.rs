//! ncdu JSON exports: `ncdu -o FILE` read as du snapshots, and
//! `rtrash du --ncdu=FILE` writing one.
//!
//! The format is described at <https://dev.yorhel.nl/ncdu/jsonfmt>:
//! `[1, MINOR, {metadata}, DIR]`, where a DIR is an array whose first
//! element is the info object of the directory and whose other elements
//! are info objects (files) or nested DIR arrays (subdirectories).
//!
//! The reader streams: it keeps one frame per open directory and the
//! path being built, never the whole document, and it walks without
//! recursion, so exports of deep trees read like any other.

use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use crate::du::{Item, RootTotals, MTIME_UNKNOWN};
use crate::du_snap::Snapshot;
use crate::{du_filter, du_fs};

/// Whether `head` (the first bytes of a file) starts an ncdu export: a
/// JSON array, after any whitespace.
pub fn looks_like_export(head: &[u8]) -> bool {
    head.iter()
        .find(|b| !b.is_ascii_whitespace())
        .is_some_and(|&b| b == b'[')
}

/// One info object, with the fields the totals need.
#[derive(Debug, Default)]
pub(crate) struct Info {
    pub(crate) name: Vec<u8>,
    pub(crate) asize: u64,
    pub(crate) dsize: u64,
    pub(crate) dev: Option<u64>,
    pub(crate) ino: u64,
    pub(crate) nlink: u64,
    pub(crate) hlnkc: bool,
    pub(crate) excluded: bool,
    pub(crate) read_error: bool,
    /// [`MTIME_UNKNOWN`] when the export has none.
    pub(crate) mtime: i64,
}

/// An open directory: its totals so far and where its name starts in
/// the shared path buffer.
struct Frame {
    path_len: usize,
    dev: u64,
    disk: u64,
    apparent: u64,
    items: u64,
    mtime: i64,
    /// Excluded directories are counted once by name and add nothing.
    excluded: bool,
    /// Index of this directory's row (the root has none).
    row: Option<usize>,
}

/// Byte reader with one byte of lookahead and a position for errors.
struct Lexer<R> {
    r: R,
    pos: u64,
}

impl<R: BufRead> Lexer<R> {
    fn err(&self, what: &str) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("ncdu export, byte {}: {what}", self.pos),
        )
    }

    fn peek(&mut self) -> io::Result<Option<u8>> {
        Ok(self.r.fill_buf()?.first().copied())
    }

    fn bump(&mut self) {
        self.r.consume(1);
        self.pos += 1;
    }

    fn next(&mut self) -> io::Result<u8> {
        match self.peek()? {
            Some(b) => {
                self.bump();
                Ok(b)
            }
            None => Err(self.err("unexpected end of file")),
        }
    }

    /// The next byte that is not whitespace, without consuming it.
    fn peek_token(&mut self) -> io::Result<Option<u8>> {
        while let Some(b) = self.peek()? {
            if !b.is_ascii_whitespace() {
                return Ok(Some(b));
            }
            self.bump();
        }
        Ok(None)
    }

    fn expect(&mut self, want: u8) -> io::Result<()> {
        match self.peek_token()? {
            Some(b) if b == want => {
                self.bump();
                Ok(())
            }
            None => Err(self.err("unexpected end of file")),
            _ => Err(self.err(&format!("expected '{}'", want as char))),
        }
    }

    /// `,` then true, or `close` then false.
    fn more(&mut self, close: u8) -> io::Result<bool> {
        match self.peek_token()? {
            Some(b',') => {
                self.bump();
                Ok(true)
            }
            Some(b) if b == close => {
                self.bump();
                Ok(false)
            }
            None => Err(self.err("unexpected end of file")),
            _ => Err(self.err(&format!("expected ',' or '{}'", close as char))),
        }
    }

    /// A string's raw bytes: escapes decoded, other bytes kept as they
    /// are, since ncdu writes names in the file system's encoding.
    fn string(&mut self) -> io::Result<Vec<u8>> {
        self.expect(b'"')?;
        let mut out = Vec::new();
        loop {
            match self.next()? {
                b'"' => return Ok(out),
                b'\\' => match self.next()? {
                    b'"' => out.push(b'"'),
                    b'\\' => out.push(b'\\'),
                    b'/' => out.push(b'/'),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'u' => {
                        let mut c = self.hex4()?;
                        if (0xD800..0xDC00).contains(&c) {
                            // A high surrogate wants a low one next.
                            if self.peek()? == Some(b'\\') {
                                self.bump();
                                if self.next()? != b'u' {
                                    return Err(self.err("bad escape"));
                                }
                                let lo = self.hex4()?;
                                c = if (0xDC00..0xE000).contains(&lo) {
                                    0x10000 + ((c - 0xD800) << 10) + (lo - 0xDC00)
                                } else {
                                    0xFFFD
                                };
                            } else {
                                c = 0xFFFD;
                            }
                        }
                        let ch = char::from_u32(c).unwrap_or('\u{FFFD}');
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    }
                    _ => return Err(self.err("bad escape")),
                },
                b => out.push(b),
            }
        }
    }

    fn hex4(&mut self) -> io::Result<u32> {
        let mut v = 0;
        for _ in 0..4 {
            let d = (self.next()? as char)
                .to_digit(16)
                .ok_or_else(|| self.err("bad \\u escape"))?;
            v = v * 16 + d;
        }
        Ok(v)
    }

    /// A number's text (ncdu writes integers, and mtimes may carry a
    /// fraction).
    fn number_text(&mut self) -> io::Result<String> {
        self.peek_token()?;
        let mut s = String::new();
        while let Some(b) = self.peek()? {
            if b.is_ascii_digit() || matches!(b, b'-' | b'+' | b'.' | b'e' | b'E') {
                s.push(b as char);
                self.bump();
            } else {
                break;
            }
        }
        if s.is_empty() {
            return Err(self.err("expected a number"));
        }
        Ok(s)
    }

    fn number(&mut self) -> io::Result<u64> {
        let s = self.number_text()?;
        s.parse::<u64>()
            .ok()
            .or_else(|| {
                s.parse::<f64>()
                    .ok()
                    .filter(|f| *f >= 0.0)
                    .map(|f| f as u64)
            })
            .ok_or_else(|| self.err("bad number"))
    }

    fn word(&mut self, w: &[u8]) -> io::Result<()> {
        for &c in w {
            if self.next()? != c {
                return Err(self.err("bad literal"));
            }
        }
        Ok(())
    }

    fn boolean(&mut self) -> io::Result<bool> {
        match self.peek_token()? {
            Some(b't') => self.word(b"true").map(|_| true),
            Some(b'f') => self.word(b"false").map(|_| false),
            Some(b'n') => self.word(b"null").map(|_| false),
            _ => Err(self.err("expected true or false")),
        }
    }

    /// Skip any value, nested or not, without recursion.
    fn skip_value(&mut self) -> io::Result<()> {
        let mut depth = 0usize;
        loop {
            match self.peek_token()? {
                Some(b'[' | b'{') => {
                    self.bump();
                    depth += 1;
                    continue;
                }
                Some(b']' | b'}') if depth > 0 => {
                    self.bump();
                    depth -= 1;
                }
                Some(b'"') => {
                    self.string()?;
                }
                Some(b't' | b'f' | b'n') => {
                    self.boolean()?;
                }
                Some(b',' | b':') if depth > 0 => {
                    self.bump();
                    continue;
                }
                Some(_) => {
                    self.number_text()?;
                }
                None => return Err(self.err("unexpected end of file")),
            }
            if depth == 0 {
                return Ok(());
            }
        }
    }

    /// An info object (the opening `{` already seen by `peek_token`).
    fn info(&mut self) -> io::Result<Info> {
        self.expect(b'{')?;
        let mut info = Info {
            nlink: 1,
            mtime: MTIME_UNKNOWN,
            ..Info::default()
        };
        if self.peek_token()? == Some(b'}') {
            self.bump();
            return Ok(info);
        }
        loop {
            let key = self.string()?;
            self.expect(b':')?;
            match key.as_slice() {
                b"name" => info.name = self.string()?,
                b"asize" => info.asize = self.number()?,
                b"dsize" => info.dsize = self.number()?,
                b"dev" => info.dev = Some(self.number()?),
                b"ino" => info.ino = self.number()?,
                b"nlink" => info.nlink = self.number()?,
                b"hlnkc" => info.hlnkc = self.boolean()?,
                b"read_error" => info.read_error = self.boolean()?,
                b"mtime" => info.mtime = self.number()?.min(i64::MAX as u64) as i64,
                b"excluded" => {
                    // Any string marks the entry excluded; null does not.
                    if self.peek_token()? == Some(b'n') {
                        self.word(b"null")?;
                    } else {
                        self.string()?;
                        info.excluded = true;
                    }
                }
                _ => self.skip_value()?,
            }
            if !self.more(b'}')? {
                return Ok(info);
            }
        }
    }
}

/// Read an ncdu export as a snapshot: one directory row per directory
/// below the root, with totals counted as `rtrash du` counts them.
/// Disk use includes directories' own blocks, apparent size counts file
/// lengths only, and a hard-linked file (same `dev` and `ino`, `nlink`
/// above 1) counts once, where it first appears. Excluded entries add
/// nothing and show in the root's excluded count.
pub fn parse(r: impl BufRead) -> io::Result<Snapshot> {
    let mut lx = Lexer { r, pos: 0 };
    lx.expect(b'[')?;
    let major = lx.number()?;
    if major != 1 {
        return Err(lx.err(&format!("format version {major} is not supported (only 1)")));
    }
    lx.expect(b',')?;
    lx.number()?;
    lx.expect(b',')?;
    // Metadata: only the timestamp matters.
    let mut created = 0i64;
    lx.expect(b'{')?;
    if lx.peek_token()? == Some(b'}') {
        lx.bump();
    } else {
        loop {
            let key = lx.string()?;
            lx.expect(b':')?;
            if key == b"timestamp" {
                created = lx.number()?.min(i64::MAX as u64) as i64;
            } else {
                lx.skip_value()?;
            }
            if !lx.more(b'}')? {
                break;
            }
        }
    }
    lx.expect(b',')?;

    // The root directory, then its entries in order.
    lx.expect(b'[')?;
    let mut tree = Builder::new(&lx.info()?);
    loop {
        // After an element, either `,` and the next one, or `]`.
        if !lx.more(b']')? {
            if tree.close() {
                break;
            }
            continue;
        }
        match lx.peek_token()? {
            Some(b'{') => tree.file(&lx.info()?),
            Some(b'[') => {
                lx.bump();
                tree.open(&lx.info()?);
            }
            _ => return Err(lx.err("expected an info object or a directory array")),
        }
    }
    lx.expect(b']')?;
    if lx.peek_token()?.is_some() {
        return Err(lx.err("data after the end of the export"));
    }
    Ok(tree.finish(created))
}

/// Totals and directory rows of an ncdu tree, fed one entry at a time in
/// a depth-first walk: [`Builder::open`] enters a directory, and
/// [`Builder::close`] leaves it. Both export formats read through it.
pub(crate) struct Builder {
    root: RootTotals,
    dirs: Vec<Item>,
    seen: HashSet<(u64, u64)>,
    /// Path of the innermost open directory.
    path: Vec<u8>,
    stack: Vec<Frame>,
    sep: u8,
}

impl Builder {
    /// Start at the root directory, whose name is its full path.
    pub(crate) fn new(top: &Info) -> Self {
        let mut path = top.name.clone();
        // Names join with the separator the root uses: `\` for an export
        // written on Windows (`C:\Users`), `/` otherwise.
        let sep = if path.contains(&b'\\') && !path.contains(&b'/') {
            b'\\'
        } else {
            b'/'
        };
        // Keep a bare root (`/`, `C:\`) whole.
        let bare = if sep == b'/' { 1 } else { 3 };
        while path.len() > bare && path.last() == Some(&sep) {
            path.pop();
        }
        let root = RootTotals {
            path: bytes_path(path.clone()),
            dirs: 1,
            ..RootTotals::default()
        };
        Builder {
            root,
            dirs: Vec::new(),
            seen: HashSet::new(),
            path,
            stack: vec![open_dir(top, 0, 0)],
            sep,
        }
    }

    /// A file, or any entry that is not an open directory, in the
    /// innermost open directory.
    pub(crate) fn file(&mut self, f: &Info) {
        let Some(p) = self.stack.last_mut() else {
            return;
        };
        if p.excluded {
            return;
        }
        if f.excluded {
            self.root.excluded += 1;
            return;
        }
        let dev = f.dev.unwrap_or(p.dev);
        if (f.nlink > 1 || f.hlnkc) && !self.seen.insert((dev, f.ino)) {
            self.root.links += 1;
            return;
        }
        self.root.files += 1;
        p.disk = p.disk.saturating_add(f.dsize);
        p.apparent = p.apparent.saturating_add(f.asize);
        p.items += 1;
        p.mtime = p.mtime.max(f.mtime);
    }

    /// Enter directory `d`, inside the innermost open one.
    pub(crate) fn open(&mut self, d: &Info) {
        let Some(parent) = self.stack.last() else {
            return;
        };
        let (parent_dev, parent_excluded) = (parent.dev, parent.excluded);
        let path_len = self.path.len();
        if self.path.last() != Some(&self.sep) {
            self.path.push(self.sep);
        }
        self.path.extend_from_slice(&d.name);
        let mut fr = open_dir(d, path_len, parent_dev);
        if parent_excluded || d.excluded {
            fr.excluded = true;
            if !parent_excluded {
                self.root.excluded += 1;
            }
        } else if d.read_error {
            // As in a scan: an unreadable directory adds its own blocks
            // but gets no row.
            self.root.dirs += 1;
        } else {
            self.root.dirs += 1;
            fr.row = Some(self.dirs.len());
            self.dirs.push(Item {
                path: bytes_path(self.path.clone()),
                is_dir: true,
                apparent: 0,
                disk: 0,
                mtime: MTIME_UNKNOWN,
                items: 0,
                artefact: None,
                repos: Vec::new(),
                git: None,
                unique: None,
            });
        }
        self.stack.push(fr);
    }

    /// Leave the innermost open directory, adding its totals to its
    /// parent. True once that was the root.
    pub(crate) fn close(&mut self) -> bool {
        let Some(done) = self.stack.pop() else {
            return true;
        };
        self.path.truncate(done.path_len);
        if let Some(i) = done.row {
            let row = &mut self.dirs[i];
            row.disk = done.disk;
            row.apparent = done.apparent;
            row.items = done.items;
            row.mtime = done.mtime;
        }
        let Some(parent) = self.stack.last_mut() else {
            self.root.disk = done.disk;
            self.root.apparent = done.apparent;
            return true;
        };
        if !done.excluded {
            parent.disk = parent.disk.saturating_add(done.disk);
            parent.apparent = parent.apparent.saturating_add(done.apparent);
            parent.items = parent.items.saturating_add(done.items + 1);
            parent.mtime = parent.mtime.max(done.mtime);
        }
        false
    }

    /// The snapshot, once [`Builder::close`] has left the root.
    pub(crate) fn finish(self, created: i64) -> Snapshot {
        Snapshot {
            created,
            max_depth: None,
            one_file_system: false,
            roots: vec![self.root],
            dirs: self.dirs,
        }
    }
}

fn open_dir(info: &Info, path_len: usize, parent_dev: u64) -> Frame {
    Frame {
        path_len,
        dev: info.dev.unwrap_or(parent_dev),
        disk: info.dsize,
        apparent: 0,
        items: 0,
        mtime: info.mtime,
        excluded: info.excluded,
        row: None,
    }
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

/// What [`export`] left incomplete.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Exported {
    /// Entries written, the root included.
    pub entries: u64,
    /// Entries marked `read_error`: a failed stat or listing.
    pub unreadable: u64,
    /// Directories past the walk's depth or path limits, written with
    /// `read_error` and without their contents.
    pub too_deep: u64,
    /// Entries left out by `-x` or the filters, written by name only.
    pub excluded: u64,
}

/// One open directory of the export walk.
struct ExFrame {
    dir: du_fs::Dir,
    entries: du_fs::Entries,
    /// Entry indices, files before directories (the order a scan counts
    /// them in, so a hard link is charged where a scan charges it).
    order: Vec<usize>,
    next: usize,
    dev: u64,
    chain: Option<std::sync::Arc<du_filter::Chain>>,
}

/// Write `name` as a JSON string. Bytes are kept as they are (ncdu writes
/// names in the file system's encoding); quotes, backslashes and control
/// bytes are escaped.
fn write_name(w: &mut impl Write, name: &[u8]) -> io::Result<()> {
    w.write_all(b"{\"name\":\"")?;
    let mut start = 0;
    for (i, &b) in name.iter().enumerate() {
        let esc: Option<&[u8]> = match b {
            b'"' => Some(b"\\\""),
            b'\\' => Some(b"\\\\"),
            b'\n' => Some(b"\\n"),
            b'\t' => Some(b"\\t"),
            b'\r' => Some(b"\\r"),
            _ => None,
        };
        if esc.is_none() && b >= 0x20 && b != 0x7f {
            continue;
        }
        w.write_all(&name[start..i])?;
        match esc {
            Some(e) => w.write_all(e)?,
            None => write!(w, "\\u{b:04x}")?,
        }
        start = i + 1;
    }
    w.write_all(&name[start..])?;
    w.write_all(b"\"")
}

/// The fields after the name of a stat-ed entry, without the closing `}`.
fn write_stat(w: &mut impl Write, st: &du_fs::Stat, parent_dev: Option<u64>) -> io::Result<()> {
    let (asize, dsize) = st.sizes();
    write!(w, ",\"asize\":{asize},\"dsize\":{dsize}")?;
    if parent_dev != Some(st.dev) {
        write!(w, ",\"dev\":{}", st.dev)?;
    }
    if !st.is_dir() && st.nlink > 1 {
        write!(
            w,
            ",\"ino\":{},\"hlnkc\":true,\"nlink\":{}",
            st.ino,
            st.nlink.min(u32::MAX as u64)
        )?;
    }
    if !st.is_dir() && !st.is_file() {
        w.write_all(b",\"notreg\":true")?;
    }
    if st.mtime > 0 {
        write!(w, ",\"mtime\":{}", st.mtime)?;
    }
    Ok(())
}

/// Write the tree at `root` as an ncdu export (format 1.2), streaming
/// as it walks: `ncdu -f FILE` browses it and `rtrash du -f FILE` lists
/// it. Directories on other file systems (with `one_file_system`) and
/// entries the filters leave out are written by name, marked excluded.
/// The walk keeps one open directory per level and stops at the same
/// depth and path limits as a scan.
pub fn export(
    root: &Path,
    one_file_system: bool,
    spec: Option<&du_filter::Spec>,
    created: i64,
    w: &mut impl Write,
) -> io::Result<Exported> {
    let abs = crate::du::absolute(root)?;
    let st = du_fs::lstat(&abs)?;
    if !st.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "an ncdu export starts at a directory",
        ));
    }
    du_fs::raise_nofile_limit();
    let filter = spec.map(|s| du_filter::Filter::new(s, &abs));
    let mut out = Exported::default();
    let mut path = abs.clone();
    let root_dev = st.dev;

    write!(
        w,
        "[1,2,{{\"progname\":\"rtrash\",\"progver\":\"{}\",\"timestamp\":{created}}},\n[",
        env!("CARGO_PKG_VERSION")
    )?;
    write_name(w, path_bytes(&abs).as_slice())?;
    write_stat(w, &st, None)?;
    let dir = du_fs::Dir::open(&abs)?;
    let (entries, failed) = dir.entries();
    if failed {
        w.write_all(b",\"read_error\":true")?;
        out.unreadable += 1;
    }
    w.write_all(b"}")?;
    out.entries += 1;
    let chain = filter.as_ref().and_then(|f| {
        let (git, ignore) = repo_markers(&entries);
        f.chain_for(f.root_chain.as_ref(), &abs, git, ignore)
    });
    let mut stack = vec![ExFrame {
        dir,
        order: files_first(&entries),
        entries,
        next: 0,
        dev: st.dev,
        chain,
    }];

    while let Some(depth) = stack.len().checked_sub(1) {
        let top = &mut stack[depth];
        if top.next == top.order.len() {
            stack.pop();
            path.pop();
            w.write_all(b"]")?;
            continue;
        }
        let i = top.order[top.next];
        top.next += 1;
        let (name, kind) = top.entries.get(i);
        let excluded = |is_dir: bool, chain: Option<&du_filter::Chain>, path: &Path| {
            filter
                .as_ref()
                .is_some_and(|f| f.excluded(chain, path, name.to_bytes(), is_dir))
        };
        w.write_all(b",\n")?;
        out.entries += 1;
        let chain = top.chain.clone();
        if kind != du_fs::Kind::Unknown
            && excluded(kind == du_fs::Kind::Dir, chain.as_deref(), &path)
        {
            write_name(w, name.to_bytes())?;
            w.write_all(b",\"excluded\":\"pattern\"}")?;
            out.excluded += 1;
            continue;
        }
        let st = match top
            .entries
            .stat(i)
            .map_or_else(|| top.dir.stat_at(name), Ok)
        {
            Ok(st) => st,
            Err(_) => {
                write_name(w, name.to_bytes())?;
                w.write_all(b",\"read_error\":true}")?;
                out.unreadable += 1;
                continue;
            }
        };
        if kind == du_fs::Kind::Unknown && excluded(st.is_dir(), chain.as_deref(), &path) {
            write_name(w, name.to_bytes())?;
            w.write_all(b",\"excluded\":\"pattern\"}")?;
            out.excluded += 1;
            continue;
        }
        let parent_dev = top.dev;
        write_name_or_dir(w, name.to_bytes(), st.is_dir())?;
        if !st.is_dir() {
            write_stat(w, &st, Some(parent_dev))?;
            w.write_all(b"}")?;
            continue;
        }
        if one_file_system && st.dev != root_dev {
            write!(w, ",\"dev\":{},\"excluded\":\"otherfs\"}}]", st.dev)?;
            out.excluded += 1;
            continue;
        }
        let child_path = path.join(du_fs::os(name));
        if depth + 1 >= crate::du::MAX_DEPTH
            || child_path.as_os_str().len() > crate::du::MAX_PATH_BYTES
        {
            write_stat(w, &st, Some(parent_dev))?;
            w.write_all(b",\"read_error\":true}]")?;
            out.too_deep += 1;
            continue;
        }
        let opened = top.dir.open_child(name);
        let Ok(dir) = opened else {
            write_stat(w, &st, Some(parent_dev))?;
            w.write_all(b",\"read_error\":true}]")?;
            out.unreadable += 1;
            continue;
        };
        let (entries, failed) = dir.entries();
        let (git, ignore) = repo_markers(&entries);
        if let Some(f) = &filter {
            // A tagged cache is left out whole, as a scan leaves it out.
            if f.exclude_caches
                && has_name(&entries, b"CACHEDIR.TAG")
                && du_filter::has_cachedir_tag(&child_path)
            {
                write_stat(w, &st, Some(parent_dev))?;
                w.write_all(b",\"excluded\":\"pattern\"}]")?;
                out.excluded += 1;
                continue;
            }
        }
        write_stat(w, &st, Some(parent_dev))?;
        if failed {
            w.write_all(b",\"read_error\":true")?;
            out.unreadable += 1;
        }
        w.write_all(b"}")?;
        let chain = filter
            .as_ref()
            .and_then(|f| f.chain_for(chain.as_ref(), &child_path, git, ignore));
        path = child_path;
        stack.push(ExFrame {
            dir,
            order: files_first(&entries),
            entries,
            next: 0,
            dev: st.dev,
            chain,
        });
    }
    w.write_all(b"]\n")?;
    w.flush()?;
    Ok(out)
}

/// `[` then the name object for a directory, the name object alone for
/// anything else.
fn write_name_or_dir(w: &mut impl Write, name: &[u8], is_dir: bool) -> io::Result<()> {
    if is_dir {
        w.write_all(b"[")?;
    }
    write_name(w, name)
}

/// Indices of `entries`, known directories last.
fn files_first(entries: &du_fs::Entries) -> Vec<usize> {
    let mut v: Vec<usize> = (0..entries.len()).collect();
    v.sort_by_key(|&i| entries.get(i).1 == du_fs::Kind::Dir);
    v
}

fn has_name(entries: &du_fs::Entries, want: &[u8]) -> bool {
    (0..entries.len()).any(|i| entries.get(i).0.to_bytes() == want)
}

/// Whether a listing holds `.git` and `.gitignore`.
fn repo_markers(entries: &du_fs::Entries) -> (bool, bool) {
    (has_name(entries, b".git"), has_name(entries, b".gitignore"))
}

#[cfg(unix)]
fn path_bytes(p: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(p: &Path) -> Vec<u8> {
    p.as_os_str().as_encoded_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn read(s: &str) -> Snapshot {
        parse(s.as_bytes()).unwrap()
    }

    fn row<'a>(snap: &'a Snapshot, p: &str) -> &'a Item {
        snap.dirs
            .iter()
            .find(|d| d.path == Path::new(p))
            .unwrap_or_else(|| panic!("no row {p}: {:?}", snap.dirs))
    }

    #[test]
    fn the_documented_example_reads_as_totals() {
        let snap = read(
            r#"[1, 0, {"progname":"ncdu","progver":"1.9","timestamp":1354477149},
            [{"name":"/media/harddrive","dsize":4096,"asize":422,"dev":39123423,"ino":29342345},
             {"name":"SomeFile","dsize":32768,"asize":32414,"ino":91245479284},
             [{"name":"EmptyDir","dsize":4096,"asize":10,"ino":3924}]]]"#,
        );
        assert_eq!(snap.created, 1354477149);
        let r = &snap.roots[0];
        assert_eq!(r.path, Path::new("/media/harddrive"));
        // Directories add their blocks to disk use, not to apparent size.
        assert_eq!((r.disk, r.apparent), (4096 + 32768 + 4096, 32414));
        assert_eq!((r.files, r.dirs, r.links, r.excluded), (1, 2, 0, 0));
        let d = row(&snap, "/media/harddrive/EmptyDir");
        assert_eq!((d.disk, d.apparent, d.items), (4096, 0, 0));
        // Written without -e: no mtime, so no age either.
        assert_eq!(d.mtime, MTIME_UNKNOWN);
        assert_eq!(snap.dirs.len(), 1);
    }

    #[test]
    fn hard_links_count_once_and_excluded_entries_add_nothing() {
        let snap = read(
            r#"[1,2,{},[{"name":"/r/","dsize":0,"dev":5},
              [{"name":"a","dsize":100},
                {"name":"x","dsize":4000,"asize":3000,"ino":9,"nlink":2,"mtime":50.75},
                {"name":"skip","excluded":"pattern"}],
              [{"name":"b","dsize":100,"mtime":70},
                {"name":"x2","dsize":4000,"asize":3000,"ino":9,"hlnkc":true}],
              [{"name":"c","dsize":100,"dev":6},
                {"name":"y","dsize":10,"ino":9,"nlink":2}],
              [{"name":"mnt","excluded":"otherfs"},{"name":"hidden","dsize":1}],
              [{"name":"locked","dsize":7,"read_error":true}],
              {"name":"link","asize":5,"notreg":true,"extra":[1,{"k":null}]}]]"#,
        );
        let r = &snap.roots[0];
        assert_eq!(r.path, Path::new("/r"));
        assert_eq!(r.links, 1, "x2 is the second name of x");
        // Same inode on another device is another file.
        assert_eq!(r.files, 3);
        assert_eq!(r.excluded, 2);
        assert_eq!(r.dirs, 5);
        assert_eq!(r.disk, 300 + 4000 + 10 + 7);
        assert_eq!(r.apparent, 3005);
        let a = row(&snap, "/r/a");
        assert_eq!((a.disk, a.apparent, a.items, a.mtime), (4100, 3000, 1, 50));
        let b = row(&snap, "/r/b");
        assert_eq!((b.disk, b.items, b.mtime), (100, 0, 70));
        assert!(snap.dirs.iter().all(|d| !d.path.ends_with("mnt")));
        assert_eq!(snap.dirs.len(), 3);
    }

    #[test]
    fn names_keep_their_bytes_and_escapes_decode() {
        let mut doc = br#"[1,2,{},[{"name":"/"},[{"name":"\u00e9t\u00e9 \"q\"\\\ud83d\ude00\ud800x"}],[{"name":""#
            .to_vec();
        doc.extend_from_slice(b"raw\xff");
        doc.extend_from_slice(br#""}]]]"#);
        let snap = parse(&doc[..]).unwrap();
        assert_eq!(snap.roots[0].path, Path::new("/"));
        let names: Vec<String> = snap
            .dirs
            .iter()
            .map(|d| d.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names[0], "/été \"q\"\\😀\u{FFFD}x");
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            assert_eq!(snap.dirs[1].path.as_os_str().as_bytes(), b"/raw\xff");
        }
    }

    #[test]
    fn names_join_with_the_separator_of_the_root() {
        let snap =
            read(r#"[1,2,{},[{"name":"C:\\Users\\me\\"},[{"name":"src"}],[{"name":"tmp"}]]]"#);
        assert_eq!(snap.roots[0].path, Path::new("C:\\Users\\me"));
        assert_eq!(snap.dirs[0].path, Path::new("C:\\Users\\me\\src"));
        let snap = read(r#"[1,2,{},[{"name":"C:\\"},[{"name":"src"}]]]"#);
        assert_eq!(snap.dirs[0].path, Path::new("C:\\src"));
        let snap = read(r#"[1,2,{},[{"name":"/"},[{"name":"etc"}]]]"#);
        assert_eq!(snap.dirs[0].path, Path::new("/etc"));
    }

    #[test]
    fn deep_exports_read_without_recursion() {
        let n = 20_000;
        let mut doc = String::from(r#"[1,2,{},[{"name":"/d","dsize":1}"#);
        for _ in 0..n {
            doc.push_str(r#",[{"name":"d","dsize":1}"#);
        }
        doc.push_str(r#",{"name":"f","dsize":5,"asize":5}"#);
        for _ in 0..n {
            doc.push(']');
        }
        doc.push_str("]]");
        let snap = read(&doc);
        assert_eq!(snap.dirs.len(), n);
        assert_eq!(snap.roots[0].disk, n as u64 + 1 + 5);
        assert_eq!(snap.roots[0].files, 1);
    }

    #[test]
    fn broken_exports_say_where() {
        for (doc, want) in [
            ("[2,0,{},[{\"name\":\"/\"}]]", "version 2"),
            ("[1,0,{},[{\"name\":\"/\"}", "end of file"),
            ("[1,0,{},[{\"name\":\"/\"}] x]", "expected ']'"),
            ("[1,0,{},[{\"name\":\"/\"}]] x", "after the end"),
            ("[1,0,{},[{\"name\":\"/\"},7]]", "info object"),
        ] {
            let e = parse(doc.as_bytes()).unwrap_err().to_string();
            assert!(e.contains(want), "{doc}: {e}");
            assert!(e.starts_with("ncdu export, byte "), "{e}");
        }
        assert!(looks_like_export(b"  \n[1,"));
        assert!(!looks_like_export(b"# rtrash du snapshot 1"));
    }

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "rtrash-ncdu-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Export `root`, read the export back, and scan `root` live.
    fn roundtrip(
        root: &Path,
        spec: Option<&du_filter::Spec>,
    ) -> (Exported, Snapshot, crate::du::Report) {
        let mut buf = Vec::new();
        let done = export(root, false, spec, 1_700_000_000, &mut buf).unwrap();
        let snap =
            parse(&buf[..]).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&buf)));
        let opts = crate::du_snap::snapshot_opts(crate::du::ScanOpts {
            filter: spec.map(|s| std::sync::Arc::new(s.clone())),
            ..crate::du::ScanOpts::default()
        });
        let report = crate::du::scan(&[root.to_path_buf()], &opts).unwrap();
        (done, snap, report)
    }

    #[test]
    fn an_export_reads_back_with_the_totals_of_a_scan() {
        use std::fs;
        let root = tmp("rt");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("logs")).unwrap();
        fs::write(root.join("a/f"), vec![1u8; 50_000]).unwrap();
        fs::write(root.join("a/b/g"), vec![2u8; 9_000]).unwrap();
        fs::write(root.join("logs/x.log"), vec![3u8; 70_000]).unwrap();
        #[cfg(unix)]
        {
            // Quotes, backslashes and newlines are not file names on Windows.
            fs::write(root.join("q\"uo\\te\nnl"), b"x").unwrap();
            fs::hard_link(root.join("a/f"), root.join("a/b/f-link")).unwrap();
            std::os::unix::fs::symlink("f", root.join("a/sym")).unwrap();
        }
        let (done, snap, scan) = roundtrip(&root, None);
        assert_eq!((done.unreadable, done.too_deep, done.excluded), (0, 0, 0));
        assert_eq!(snap.created, 1_700_000_000);
        let (r, s) = (&snap.roots[0], &scan.roots[0]);
        assert_eq!(
            (r.disk, r.apparent, r.files, r.links, r.dirs),
            (s.disk, s.apparent, s.files, s.links, s.dirs)
        );
        let mut got: Vec<_> = snap
            .dirs
            .iter()
            .map(|d| (d.path.clone(), d.disk, d.apparent, d.mtime))
            .collect();
        let mut want: Vec<_> = scan
            .dirs
            .iter()
            .map(|d| (d.path.clone(), d.disk, d.apparent, d.mtime))
            .collect();
        got.sort();
        want.sort();
        assert_eq!(got, want);
        assert!(snap
            .dirs
            .iter()
            .any(|d| d.path.ends_with("a/b") || d.path.ends_with("a\\b")));

        // Filters leave the same entries out as in a scan, by name.
        let spec = du_filter::Spec {
            patterns: vec!["*.log".into()],
            ..du_filter::Spec::default()
        };
        let (done, snap, scan) = roundtrip(&root, Some(&spec));
        assert_eq!(done.excluded, 1);
        assert_eq!(snap.roots[0].excluded, 1);
        assert_eq!(snap.roots[0].disk, scan.roots[0].disk);
        assert_eq!(snap.roots[0].apparent, scan.roots[0].apparent);
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn exports_mark_what_they_could_not_read_or_reach() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("err");
        fs::create_dir_all(root.join("locked/inner")).unwrap();
        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        // Deeper than a scan goes, built below PATH_MAX by renames.
        let deep = root.join("deep");
        fs::create_dir(&deep).unwrap();
        let levels = crate::du::MAX_DEPTH + 10;
        let c = std::ffi::CString::new(deep.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: plain descriptor calls on NUL-terminated names.
        unsafe {
            let mut fd = libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
            for _ in 0..levels {
                assert_eq!(libc::mkdirat(fd, c"d".as_ptr(), 0o755), 0);
                let next = libc::openat(fd, c"d".as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
                libc::close(fd);
                fd = next;
            }
            libc::close(fd);
        }
        let mut buf = Vec::new();
        let done = export(&root, false, None, 0, &mut buf).unwrap();
        let snap = parse(&buf[..]).unwrap();
        let root_only = unsafe { libc::geteuid() } == 0;
        if !root_only {
            assert!(done.unreadable >= 1, "{done:?}");
            let text = String::from_utf8_lossy(&buf);
            assert!(text.contains("{\"name\":\"locked\""), "locked is named");
        }
        // The walk stops at the scan's depth limit, or earlier when the
        // descriptor limit is lower: either way the export stays whole.
        assert!(done.too_deep + done.unreadable >= 1, "{done:?}");
        assert!(snap.roots[0].dirs as usize <= crate::du::MAX_DEPTH + 4);
        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        crate::fastdelete::remove_path(&root).unwrap();
    }

    #[test]
    fn names_are_escaped_for_json_but_keep_other_bytes() {
        let mut out = Vec::new();
        write_name(&mut out, b"a\"b\\c\nd\x01e\xffz").unwrap();
        assert_eq!(out, b"{\"name\":\"a\\\"b\\\\c\\nd\\u0001e\xffz\"".to_vec());
    }
}
