//! ncdu JSON exports (`ncdu -o FILE`), read as du snapshots.
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
use std::io::{self, BufRead};
use std::path::PathBuf;

use crate::du::{Item, RootTotals, MTIME_UNKNOWN};
use crate::du_snap::Snapshot;

/// Whether `head` (the first bytes of a file) starts an ncdu export: a
/// JSON array, after any whitespace.
pub fn looks_like_export(head: &[u8]) -> bool {
    head.iter()
        .find(|b| !b.is_ascii_whitespace())
        .is_some_and(|&b| b == b'[')
}

/// One info object, with the fields the totals need.
#[derive(Debug, Default)]
struct Info {
    name: Vec<u8>,
    asize: u64,
    dsize: u64,
    dev: Option<u64>,
    ino: u64,
    nlink: u64,
    hlnkc: bool,
    excluded: bool,
    read_error: bool,
    /// [`MTIME_UNKNOWN`] when the export has none.
    mtime: i64,
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

    let mut root = RootTotals::default();
    let mut dirs: Vec<Item> = Vec::new();
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut path: Vec<u8> = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();

    // The root directory.
    lx.expect(b'[')?;
    let top = lx.info()?;
    path.extend_from_slice(&top.name);
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
    root.path = bytes_path(path.clone());
    stack.push(open_dir(&top, 0, 0));
    root.dirs = 1;
    while let Some(frame) = stack.last() {
        let (parent_dev, parent_excluded) = (frame.dev, frame.excluded);
        // After an element, either `,` and the next one, or `]`.
        if !lx.more(b']')? {
            let done = stack.pop().expect("open frame");
            path.truncate(done.path_len);
            if let Some(i) = done.row {
                let row = &mut dirs[i];
                row.disk = done.disk;
                row.apparent = done.apparent;
                row.items = done.items;
                row.mtime = done.mtime;
            }
            let Some(parent) = stack.last_mut() else {
                root.disk = done.disk;
                root.apparent = done.apparent;
                break;
            };
            if !done.excluded {
                parent.disk = parent.disk.saturating_add(done.disk);
                parent.apparent = parent.apparent.saturating_add(done.apparent);
                parent.items = parent.items.saturating_add(done.items + 1);
                parent.mtime = parent.mtime.max(done.mtime);
            }
            continue;
        }
        match lx.peek_token()? {
            Some(b'{') => {
                let f = lx.info()?;
                if parent_excluded {
                    continue;
                }
                if f.excluded {
                    root.excluded += 1;
                    continue;
                }
                let dev = f.dev.unwrap_or(parent_dev);
                if (f.nlink > 1 || f.hlnkc) && !seen.insert((dev, f.ino)) {
                    root.links += 1;
                    continue;
                }
                root.files += 1;
                let p = stack.last_mut().expect("open frame");
                p.disk = p.disk.saturating_add(f.dsize);
                p.apparent = p.apparent.saturating_add(f.asize);
                p.items += 1;
                p.mtime = p.mtime.max(f.mtime);
            }
            Some(b'[') => {
                lx.bump();
                let d = lx.info()?;
                let path_len = path.len();
                if path.last() != Some(&sep) {
                    path.push(sep);
                }
                path.extend_from_slice(&d.name);
                let mut fr = open_dir(&d, path_len, parent_dev);
                if parent_excluded || d.excluded {
                    fr.excluded = true;
                    if !parent_excluded {
                        root.excluded += 1;
                    }
                } else if d.read_error {
                    // As in a scan: an unreadable directory adds its own
                    // blocks but gets no row.
                    root.dirs += 1;
                } else {
                    root.dirs += 1;
                    fr.row = Some(dirs.len());
                    dirs.push(Item {
                        path: bytes_path(path.clone()),
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
                stack.push(fr);
            }
            _ => return Err(lx.err("expected an info object or a directory array")),
        }
    }
    lx.expect(b']')?;
    if lx.peek_token()?.is_some() {
        return Err(lx.err("data after the end of the export"));
    }
    Ok(Snapshot {
        created,
        max_depth: None,
        one_file_system: false,
        roots: vec![root],
        dirs,
    })
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
}
