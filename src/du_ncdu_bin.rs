//! ncdu 2's binary exports (`ncdu -O FILE`, ncdu 2.6 and later) read as
//! du snapshots, through the same [`Builder`] as the JSON exports, and
//! written by [`Writer`] for `rtrash du --ncdu-binary=FILE`.
//!
//! The format is described at <https://dev.yorhel.nl/ncdu/binfmt>: a
//! signature, then blocks, each a 4-byte big-endian type and length,
//! the content, and the type and length again. Data blocks hold one
//! zstd frame of CBOR items; the index block at the end maps block
//! numbers to file offsets and names the root item. A directory item
//! points at its last entry (`sub`), and each entry at the one before it
//! (`prev`), so the walk below follows those links with a stack, loading
//! blocks as it reaches them and keeping only a few in memory.
//!
//! Every offset in the file is checked before use, and a walk that
//! visits more items than the blocks it loaded can hold (a loop of
//! references) stops with an error.

use std::io::{self, Read, Seek, SeekFrom, Write};

use crate::du::MTIME_UNKNOWN;
use crate::du_fs;
use crate::du_ncdu::{Builder, DirLeaf, Info, Leaf, Sink};
use crate::du_snap::Snapshot;

/// First bytes of every binary export.
pub const SIGNATURE: &[u8; 8] = b"\xbfncduEX1";

/// Largest block, and largest decompressed block, the format allows.
const BLOCK_MAX: usize = (1 << 24) - 1;

/// Decompressed blocks kept at once, and their most bytes together (the
/// newest block stays whatever its size). Entries of one directory can
/// lie across many blocks, and the walk visits each twice.
const CACHED: usize = 64;
const CACHED_BYTES: usize = 16 << 20;

/// Whether `head` (the first bytes of a file) starts a binary export.
pub fn looks_like_binary(head: &[u8]) -> bool {
    head.starts_with(SIGNATURE)
}

fn bad(what: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("ncdu binary export: {what}"),
    )
}

/// Read a binary export into a snapshot dated `created`. Totals follow
/// the JSON reader: sizes are summed from the items, not taken from the
/// cumulative fields, so both formats of one scan read alike. Entries
/// are read in the order they were written, as the JSON reader reads
/// them, so a hard link is charged to the same directory by both.
pub fn parse(r: impl Read + Seek, created: i64) -> io::Result<Snapshot> {
    let mut file = Blocks::open(r)?;
    let root = file.item(file.root)?;
    if root.kind != 0 {
        return Err(bad("the root item is not a directory"));
    }
    let mut tree = Builder::new(&root.info);
    let mut visits = 1u64;
    // Per open directory, the entries still to read, the next one last.
    let mut open: Vec<Vec<u64>> = vec![file.entries(root.sub, &mut visits)?];
    while let Some(entries) = open.last_mut() {
        let Some(at) = entries.pop() else {
            open.pop();
            if tree.close() {
                break;
            }
            continue;
        };
        let it = file.item(at)?;
        if it.kind == 0 {
            tree.open(&it.info);
            let below = file.entries(it.sub, &mut visits)?;
            open.push(below);
        } else {
            tree.file(&it.info);
        }
    }
    // The binary format marks unreadable entries (type -1, or a
    // directory's read error), so the count is known.
    Ok(tree.finish(created, true))
}

/// The blocks of an export, decompressed on demand.
struct Blocks<R> {
    r: R,
    /// Per block number: file offset in the high 40 bits, length in the
    /// low 24; zero for a missing block.
    pointers: Vec<u64>,
    /// Absolute reference to the root item.
    root: u64,
    /// Recently used blocks: number, last use, contents.
    cache: Vec<(usize, u64, Vec<u8>)>,
    tick: u64,
    /// Whether each block has been loaded at least once.
    seen: Vec<bool>,
    /// Decompressed bytes of every block loaded so far: no well-formed
    /// walk visits more items than that.
    loaded_bytes: u64,
    zstd: ruzstd::decoding::FrameDecoder,
}

impl<R: Read + Seek> Blocks<R> {
    fn open(mut r: R) -> io::Result<Self> {
        let mut sig = [0u8; 8];
        r.read_exact(&mut sig)
            .map_err(|_| bad("too short for a signature"))?;
        if &sig != SIGNATURE {
            return Err(bad("no ncdu signature"));
        }
        let len = r.seek(SeekFrom::End(0))?;
        if len < 8 + 16 {
            return Err(bad("too short for an index block"));
        }
        // The index block is last; its length is in its final 4 bytes.
        r.seek(SeekFrom::Start(len - 4))?;
        let tail = read_u32(&mut r)?;
        let (kind, ilen) = (tail >> 28, u64::from(tail & 0x0fff_ffff));
        if kind != 1 {
            return Err(bad("the last block is not an index block"));
        }
        if ilen < 16 || ilen % 8 != 0 || ilen > len - 8 {
            return Err(bad(format_args!("index block length {ilen} does not fit")));
        }
        r.seek(SeekFrom::Start(len - ilen))?;
        let mut index = vec![0u8; ilen as usize];
        r.read_exact(&mut index)?;
        if index[..4] != tail.to_be_bytes() {
            return Err(bad("index block header and footer differ"));
        }
        let words: Vec<u64> = index[4..index.len() - 4]
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| u64::from_be_bytes(*c))
            .collect();
        let (root, pointers) = words.split_last().ok_or_else(|| bad("empty index"))?;
        let index_at = len - ilen;
        for (n, &p) in pointers.iter().enumerate() {
            let (at, blen) = (p >> 24, p & 0xff_ffff);
            if p != 0 && (at < 8 || blen < 12 || at + blen > index_at) {
                return Err(bad(format_args!("block {n} lies outside the file")));
            }
        }
        Ok(Blocks {
            r,
            seen: vec![false; pointers.len()],
            pointers: pointers.to_vec(),
            root: *root,
            cache: Vec::new(),
            tick: 0,
            loaded_bytes: 0,
            zstd: ruzstd::decoding::FrameDecoder::new(),
        })
    }

    /// The decompressed contents of block `n`.
    fn block(&mut self, n: usize) -> io::Result<&[u8]> {
        self.tick += 1;
        if let Some(i) = self.cache.iter().position(|c| c.0 == n) {
            self.cache[i].1 = self.tick;
            return Ok(&self.cache[i].2);
        }
        let p = *self
            .pointers
            .get(n)
            .filter(|&&p| p != 0)
            .ok_or_else(|| bad(format_args!("no data block {n}")))?;
        let (at, blen) = (p >> 24, (p & 0xff_ffff) as usize);
        self.r.seek(SeekFrom::Start(at))?;
        let mut raw = vec![0u8; blen];
        self.r.read_exact(&mut raw)?;
        let head = u32::from_be_bytes(raw[..4].try_into().expect("4 bytes"));
        if head >> 28 != 0 || (head & 0x0fff_ffff) as usize != blen || raw[blen - 4..] != raw[..4] {
            return Err(bad(format_args!("block {n} has a bad header")));
        }
        if u32::from_be_bytes(raw[4..8].try_into().expect("4 bytes")) as usize != n {
            return Err(bad(format_args!("block {n} carries another number")));
        }
        let frame = &raw[8..blen - 4];
        // The frame states its size; without it, allow the format's most.
        let size = match self.zstd.init(frame) {
            Ok(()) => self.zstd.content_size() as usize,
            Err(e) => return Err(bad(format_args!("block {n}: {e}"))),
        };
        let size = if size > 0 && size <= BLOCK_MAX {
            size
        } else {
            BLOCK_MAX
        };
        let mut data = Vec::with_capacity(size);
        self.zstd
            .decode_all_to_vec(frame, &mut data)
            .map_err(|e| bad(format_args!("block {n}: {e}")))?;
        data.shrink_to_fit();
        if !std::mem::replace(&mut self.seen[n], true) {
            self.loaded_bytes += data.len() as u64;
        }
        // Make room: drop the least recently used blocks.
        let mut held: usize = self.cache.iter().map(|c| c.2.len()).sum();
        while !self.cache.is_empty()
            && (self.cache.len() >= CACHED || held + data.len() > CACHED_BYTES)
        {
            let i = (0..self.cache.len())
                .min_by_key(|&i| self.cache[i].1)
                .expect("a block");
            held -= self.cache.swap_remove(i).2.len();
        }
        self.cache.push((n, self.tick, data));
        Ok(&self.cache.last().expect("just pushed").2)
    }

    /// The entries of a directory whose last entry is `last`, found by
    /// walking back along `prev`: the last first, so popping them gives
    /// the order they were written in. `visits` counts the items read, and
    /// more of them than the loaded blocks can hold means a loop.
    fn entries(&mut self, last: Option<u64>, visits: &mut u64) -> io::Result<Vec<u64>> {
        let mut out = Vec::new();
        let mut next = last;
        while let Some(at) = next {
            *visits += 1;
            if *visits > self.loaded_bytes {
                return Err(bad("item references form a loop"));
            }
            out.push(at);
            next = self.prev_of(at)?;
        }
        Ok(out)
    }

    /// The item at absolute reference `at`.
    fn item(&mut self, at: u64) -> io::Result<Item> {
        let (n, off) = ((at >> 24) as usize, (at & 0xff_ffff) as usize);
        let data = self.block(n)?;
        let mut item =
            Item::parse(data, off).map_err(|e| bad(format_args!("item {n}:{off}: {e}")))?;
        item.prev = resolve(n, off, item.prev_ref.take())?;
        item.sub = resolve(n, off, item.sub_ref.take())?;
        Ok(item)
    }

    /// The `prev` of the item at `at`, skipping the rest of it: the walk
    /// along a directory's entries reads nothing more, and reads each
    /// entry in full afterwards.
    fn prev_of(&mut self, at: u64) -> io::Result<Option<u64>> {
        let (n, off) = ((at >> 24) as usize, (at & 0xff_ffff) as usize);
        let data = self.block(n)?;
        let prev =
            Item::prev_only(data, off).map_err(|e| bad(format_args!("item {n}:{off}: {e}")))?;
        resolve(n, off, prev)
    }
}

/// An item reference made absolute, for the item at `off` in block `n`.
fn resolve(n: usize, off: usize, r: Option<Ref>) -> io::Result<Option<u64>> {
    Ok(match r {
        None => None,
        Some(Ref::Absolute(a)) => Some(a),
        Some(Ref::Back(b)) => {
            let to = (off as u64)
                .checked_sub(b)
                .ok_or_else(|| bad(format_args!("item {n}:{off} points before its block")))?;
            Some(((n as u64) << 24) | to)
        }
    })
}

fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

/// An item reference as written: absolute, or this many bytes back from
/// the item holding it, in the same block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ref {
    Absolute(u64),
    Back(u64),
}

/// One decoded item.
#[derive(Debug, Default)]
struct Item {
    /// 0 directory, 1 file, 2 other, 3 hard link candidate, negative
    /// for excluded or unreadable entries.
    kind: i64,
    info: Info,
    prev_ref: Option<Ref>,
    sub_ref: Option<Ref>,
    prev: Option<u64>,
    sub: Option<u64>,
}

impl Item {
    /// Only the `prev` reference of the item at `off`.
    fn prev_only(data: &[u8], off: usize) -> Result<Option<Ref>, String> {
        let mut c = Cbor { data, pos: off };
        let (major, len) = c.head()?;
        if major != 5 {
            return Err("not a CBOR map".into());
        }
        let mut prev = None;
        let mut left = len;
        loop {
            match left {
                Some(0) => break,
                Some(ref mut k) => *k -= 1,
                None if c.peek()? == 0xff => break,
                None => {}
            }
            if c.uint()? == 2 {
                prev = Some(c.reference()?);
            } else {
                c.skip()?;
            }
        }
        Ok(prev)
    }

    fn parse(data: &[u8], off: usize) -> Result<Item, String> {
        let mut c = Cbor { data, pos: off };
        let (major, len) = c.head()?;
        if major != 5 {
            return Err("not a CBOR map".into());
        }
        let mut item = Item {
            info: Info {
                mtime: MTIME_UNKNOWN,
                ..Info::default()
            },
            ..Item::default()
        };
        let mut kind = None;
        let mut rderr = false;
        let mut left = len;
        loop {
            match left {
                Some(0) => break,
                Some(ref mut k) => *k -= 1,
                None if c.peek()? == 0xff => break,
                None => {}
            }
            let key = c.uint()?;
            match key {
                0 => kind = Some(c.int()?),
                1 => item.info.name = c.bytes()?,
                2 => item.prev_ref = Some(c.reference()?),
                3 => item.info.asize = c.uint()?,
                4 => item.info.dsize = c.uint()?,
                5 => item.info.dev = Some(c.uint()?),
                6 => rderr = c.boolean()?,
                12 => item.sub_ref = Some(c.reference()?),
                13 => item.info.ino = c.uint()?,
                14 => item.info.nlink = c.uint()?,
                18 => item.info.mtime = c.uint()?.min(i64::MAX as u64) as i64,
                _ => c.skip()?,
            }
        }
        let kind = kind.ok_or("no type")?;
        if item.info.name.is_empty() {
            return Err("no name".into());
        }
        item.kind = kind;
        match kind {
            // Unreadable: an entry with no size, as the JSON reader sees
            // an entry with `read_error`.
            -1 => item.info.read_error = true,
            k if k < 0 => item.info.excluded = true,
            0 => item.info.read_error = rderr,
            3 => item.info.hlnkc = true,
            _ => {}
        }
        if kind != 0 {
            // Only directories carry `dev`; entries take their parent's.
            item.info.dev = None;
            item.sub_ref = None;
        }
        Ok(item)
    }
}

/// The CBOR the items use: integers, strings, booleans, and whatever
/// unknown keys hold, skipped.
struct Cbor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Cbor<'_> {
    fn peek(&self) -> Result<u8, String> {
        self.data
            .get(self.pos)
            .copied()
            .ok_or_else(|| "runs past the end of its block".into())
    }

    fn byte(&mut self) -> Result<u8, String> {
        let b = self.peek()?;
        self.pos += 1;
        Ok(b)
    }

    /// Major type and argument; `None` for an indefinite length.
    fn head(&mut self) -> Result<(u8, Option<u64>), String> {
        let b = self.byte()?;
        let (major, info) = (b >> 5, b & 0x1f);
        let n = match info {
            0..=23 => u64::from(info),
            24..=27 => {
                let width = 1usize << (info - 24);
                let mut n = 0u64;
                for _ in 0..width {
                    n = (n << 8) | u64::from(self.byte()?);
                }
                n
            }
            31 if (2..=5).contains(&major) => return Ok((major, None)),
            _ => return Err(format!("CBOR byte {b:#04x} is not supported")),
        };
        Ok((major, Some(n)))
    }

    fn uint(&mut self) -> Result<u64, String> {
        match self.head()? {
            (0, Some(n)) => Ok(n),
            _ => Err("expected an unsigned integer".into()),
        }
    }

    fn int(&mut self) -> Result<i64, String> {
        match self.head()? {
            (0, Some(n)) => Ok(n.min(i64::MAX as u64) as i64),
            (1, Some(n)) => Ok(-1 - n.min(i64::MAX as u64 - 1) as i64),
            _ => Err("expected an integer".into()),
        }
    }

    fn reference(&mut self) -> Result<Ref, String> {
        match self.head()? {
            (0, Some(n)) => Ok(Ref::Absolute(n)),
            (1, Some(n)) => Ok(Ref::Back(n.saturating_add(1))),
            _ => Err("expected an item reference".into()),
        }
    }

    fn boolean(&mut self) -> Result<bool, String> {
        match self.byte()? {
            0xf4 => Ok(false),
            0xf5 => Ok(true),
            _ => Err("expected a boolean".into()),
        }
    }

    /// A byte or text string of known length.
    fn bytes(&mut self) -> Result<Vec<u8>, String> {
        match self.head()? {
            (2 | 3, Some(n)) => {
                let end = usize::try_from(n)
                    .ok()
                    .and_then(|n| self.pos.checked_add(n))
                    .filter(|&e| e <= self.data.len())
                    .ok_or("string runs past the end of its block")?;
                let s = self.data[self.pos..end].to_vec();
                self.pos = end;
                Ok(s)
            }
            _ => Err("expected a string of known length".into()),
        }
    }

    /// Skip one value of any type, nested ones included, without
    /// recursion.
    fn skip(&mut self) -> Result<(), String> {
        // Values still to skip per open array or map; `None` runs to a
        // break byte.
        let mut open: Vec<Option<u64>> = vec![Some(1)];
        while let Some(top) = open.last_mut() {
            match top {
                Some(0) => {
                    open.pop();
                    continue;
                }
                Some(n) => *n -= 1,
                None if self.peek()? == 0xff => {
                    self.pos += 1;
                    open.pop();
                    continue;
                }
                None => {}
            }
            let (major, n) = self.head()?;
            match (major, n) {
                (0 | 1 | 7, _) => {}
                (2 | 3, Some(len)) => {
                    self.pos = usize::try_from(len)
                        .ok()
                        .and_then(|l| self.pos.checked_add(l))
                        .filter(|&e| e <= self.data.len())
                        .ok_or("string runs past the end of its block")?;
                }
                // Chunks of an indefinite string, then a break.
                (2 | 3, None) => open.push(None),
                (4, Some(k)) => open.push(Some(k)),
                (5, Some(k)) => open.push(Some(k.saturating_mul(2))),
                (4 | 5, None) => open.push(None),
                (6, _) => open.push(Some(1)),
                _ => return Err("bad CBOR value".into()),
            }
            if open.len() > 64 {
                return Err("CBOR nests too deep".into());
            }
        }
        Ok(())
    }
}

/// Uncompressed bytes a data block collects before it is written.
const BLOCK_TARGET: usize = 128 << 10;

/// Blocks waiting for the compressing thread, beyond the one it works on.
const QUEUED_BLOCKS: usize = 2;

/// Writes the binary export the export walk sends it.
///
/// Items go out in post-order: each entry as the walk reaches it, and a
/// directory when it closes, since only then are its cumulative sizes
/// and its last entry known. A reference within one block is relative,
/// as ncdu writes it, and absolute across blocks. Items gather in a
/// block of about [`BLOCK_TARGET`] bytes, which a second thread
/// compresses while the walk goes on; frames are written in order as
/// they come back, and the index block follows the last one, so the
/// output needs no seeking and `-` (stdout) works.
pub struct Writer<'w, W: Write> {
    w: &'w mut W,
    /// Bytes written so far.
    at: u64,
    block: Vec<u8>,
    /// Data blocks handed to the compressing thread so far; the number
    /// of the one being filled.
    sent: usize,
    /// Per data block written: offset (high 40 bits) and length.
    pointers: Vec<u64>,
    stack: Vec<OpenDir>,
    root: Option<u64>,
    zstd: Compressor,
}

/// A thread that compresses blocks in the order it gets them.
struct Compressor {
    /// `None` once the last block is sent.
    tx: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    rx: std::sync::mpsc::Receiver<io::Result<Vec<u8>>>,
}

impl Compressor {
    fn start() -> Self {
        let (tx, blocks) = std::sync::mpsc::sync_channel::<Vec<u8>>(QUEUED_BLOCKS);
        let (done, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for block in blocks {
                let frame = ruzstd::encoding::compress_to_vec(
                    &block[..],
                    ruzstd::encoding::CompressionLevel::Fastest,
                );
                if done.send(with_content_size(frame, block.len())).is_err() {
                    break;
                }
            }
        });
        Compressor { tx: Some(tx), rx }
    }

    fn send(&self, block: Vec<u8>) -> io::Result<()> {
        self.tx
            .as_ref()
            .and_then(|tx| tx.send(block).ok())
            .ok_or_else(|| bad("the compressing thread stopped"))
    }
}

/// Hard links below a directory, by device and inode.
type Links = std::collections::HashMap<(u64, u64), Link>;

/// One hard-linked inode below a directory.
#[derive(Clone, Copy)]
struct Link {
    /// Its links seen below the directory so far.
    seen: u64,
    nlink: u64,
    asize: u64,
    dsize: u64,
}

/// Merge the hard links of a closed directory into its parent's; the
/// sizes of the inodes the parent had already counted.
fn merge_links(parent: &mut Links, mut child: Links) -> (u64, u64) {
    if child.len() > parent.len() {
        std::mem::swap(parent, &mut child);
    }
    let mut dup = (0u64, 0u64);
    for (key, l) in child {
        match parent.entry(key) {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                e.get_mut().seen += l.seen;
                dup.0 = dup.0.saturating_add(l.asize);
                dup.1 = dup.1.saturating_add(l.dsize);
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(l);
            }
        }
    }
    dup
}

/// A directory whose item is written when it closes.
struct OpenDir {
    name: Vec<u8>,
    asize: u64,
    dsize: u64,
    dev: u64,
    /// Whether the item names its device (it differs from the parent's).
    own_dev: bool,
    mtime: i64,
    read_error: bool,
    /// An entry somewhere below could not be read.
    sub_error: bool,
    cum_asize: u64,
    cum_dsize: u64,
    items: u64,
    /// The entry written last, which the next one points back to.
    last: Option<u64>,
    /// Hard links below, each counted once in the cumulative sizes.
    links: Links,
}

impl OpenDir {
    fn new(name: &[u8], st: &du_fs::Stat, own_dev: bool, read_error: bool) -> Self {
        let (asize, dsize) = st.sizes();
        OpenDir {
            name: name.to_vec(),
            asize,
            dsize,
            dev: st.dev,
            own_dev,
            mtime: st.mtime,
            read_error,
            sub_error: false,
            cum_asize: asize,
            cum_dsize: dsize,
            items: 0,
            last: None,
            links: Links::new(),
        }
    }
}

/// Item types, as the format numbers them.
const DIR: i64 = 0;
const FILE: i64 = 1;
const OTHER: i64 = 2;
const LINK: i64 = 3;
const ERROR: i64 = -1;
const EXCLUDED: i64 = -2;
const OTHER_FS: i64 = -3;

/// A CBOR head: major type and argument, in the shortest form.
fn put_head(out: &mut Vec<u8>, major: u8, n: u64) {
    let m = major << 5;
    if n < 24 {
        out.push(m | n as u8);
    } else if n <= 0xff {
        out.extend([m | 24, n as u8]);
    } else if n <= 0xffff {
        out.push(m | 25);
        out.extend((n as u16).to_be_bytes());
    } else if n <= 0xffff_ffff {
        out.push(m | 26);
        out.extend((n as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend(n.to_be_bytes());
    }
}

/// One item: a CBOR map built field by field.
struct ItemOut(Vec<u8>, u64);

impl ItemOut {
    fn new(kind: i64, name: &[u8]) -> Self {
        let mut it = ItemOut(Vec::with_capacity(32 + name.len()), 0);
        it.int(0, kind);
        put_head(&mut it.0, 0, 1);
        put_head(&mut it.0, 2, name.len() as u64);
        it.0.extend(name);
        it.1 += 1;
        it
    }

    fn uint(&mut self, key: u64, v: u64) {
        put_head(&mut self.0, 0, key);
        put_head(&mut self.0, 0, v);
        self.1 += 1;
    }

    fn int(&mut self, key: u64, v: i64) {
        put_head(&mut self.0, 0, key);
        if v < 0 {
            put_head(&mut self.0, 1, (-1 - v) as u64);
        } else {
            put_head(&mut self.0, 0, v as u64);
        }
        self.1 += 1;
    }

    fn boolean(&mut self, key: u64, v: bool) {
        put_head(&mut self.0, 0, key);
        self.0.push(if v { 0xf5 } else { 0xf4 });
        self.1 += 1;
    }

    /// A reference from an item at `here` to the earlier item at `to`
    /// (both absolute): relative in the same block, as ncdu writes it
    /// (shorter, and better compressed), absolute otherwise.
    fn reference(&mut self, key: u64, to: u64, here: u64) {
        put_head(&mut self.0, 0, key);
        match here.checked_sub(to) {
            Some(back) if back > 0 && to >> 24 == here >> 24 => {
                put_head(&mut self.0, 1, back - 1);
            }
            _ => put_head(&mut self.0, 0, to),
        }
        self.1 += 1;
    }

    /// The map, with its length in front.
    fn bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.0.len() + 9);
        put_head(&mut out, 5, self.1);
        out.extend(self.0);
        out
    }
}

/// Give a zstd frame from ruzstd its content size, which the format
/// requires and ruzstd leaves out: set the 4-byte size flag in the
/// frame header descriptor and insert the size after the window byte.
fn with_content_size(mut frame: Vec<u8>, size: usize) -> io::Result<Vec<u8>> {
    const MAGIC: [u8; 4] = 0xfd2f_b528u32.to_le_bytes();
    let desc = *frame.get(4).ok_or_else(|| bad("short zstd frame"))?;
    // Expected: no content size, not single-segment, no dictionary, so a
    // window byte follows the descriptor.
    if frame[..4] != MAGIC || desc & 0xe3 != 0 || frame.len() < 6 {
        return Err(bad("unexpected zstd frame header from the encoder"));
    }
    frame[4] = desc | 0x80;
    let size = u32::try_from(size).map_err(|_| bad("block too large"))?;
    frame.splice(6..6, size.to_le_bytes());
    Ok(frame)
}

impl<'w, W: Write> Writer<'w, W> {
    pub fn new(w: &'w mut W) -> Self {
        Writer {
            w,
            at: 0,
            block: Vec::with_capacity(BLOCK_TARGET + 4096),
            sent: 0,
            pointers: Vec::new(),
            stack: Vec::new(),
            root: None,
            zstd: Compressor::start(),
        }
    }

    fn write(&mut self, b: &[u8]) -> io::Result<()> {
        self.w.write_all(b)?;
        self.at += b.len() as u64;
        Ok(())
    }

    /// Add an item to the current block, with its `prev` and `sub`
    /// references (keys 2 and 12) once its place is known; its absolute
    /// reference.
    fn put(&mut self, mut item: ItemOut, prev: Option<u64>, sub: Option<u64>) -> io::Result<u64> {
        // Room for both references at their longest.
        let most = item.0.len() + 9 + 2 * 10;
        if !self.block.is_empty() && self.block.len() + most > BLOCK_TARGET {
            self.flush_block()?;
        }
        if self.block.len() + most > BLOCK_MAX {
            return Err(bad("item too large for a block"));
        }
        let at = ((self.sent as u64) << 24) | self.block.len() as u64;
        if let Some(p) = prev {
            item.reference(2, p, at);
        }
        if let Some(s) = sub {
            item.reference(12, s, at);
        }
        self.block.extend(item.bytes());
        Ok(at)
    }

    /// Hand the block being filled to the compressing thread, and write
    /// the frames that came back meanwhile.
    fn flush_block(&mut self) -> io::Result<()> {
        if self.block.is_empty() {
            return Ok(());
        }
        if self.sent >= 1 << 24 {
            return Err(bad("too many blocks"));
        }
        let block = std::mem::replace(&mut self.block, Vec::with_capacity(BLOCK_TARGET + 4096));
        self.zstd.send(block)?;
        self.sent += 1;
        while let Ok(frame) = self.zstd.rx.try_recv() {
            self.write_frame(frame?)?;
        }
        Ok(())
    }

    /// Wait for every frame still being compressed and write it.
    fn drain(&mut self) -> io::Result<()> {
        self.zstd.tx = None;
        while self.pointers.len() < self.sent {
            let frame = self
                .zstd
                .rx
                .recv()
                .map_err(|_| bad("the compressing thread stopped"))?;
            self.write_frame(frame?)?;
        }
        Ok(())
    }

    /// Write the next data block, numbered in the order blocks were sent.
    fn write_frame(&mut self, frame: Vec<u8>) -> io::Result<()> {
        let len = frame.len() + 12;
        if len > BLOCK_MAX || self.at >= 1 << 40 {
            return Err(bad("block or file too large for the format"));
        }
        let number = u32::try_from(self.pointers.len()).map_err(|_| bad("too many blocks"))?;
        let head = (len as u32).to_be_bytes();
        self.pointers.push((self.at << 24) | len as u64);
        self.write(&head)?;
        self.write(&number.to_be_bytes())?;
        self.write(&frame)?;
        self.write(&head)?;
        Ok(())
    }

    /// Write an entry of the innermost open directory, linked to the one
    /// before it, and count it there.
    fn entry(
        &mut self,
        item: ItemOut,
        sub: Option<u64>,
        cum: (u64, u64),
        items: u64,
        error: bool,
    ) -> io::Result<()> {
        let parent = self
            .stack
            .last_mut()
            .ok_or_else(|| bad("entry outside a directory"))?;
        let prev = parent.last;
        parent.cum_asize = parent.cum_asize.saturating_add(cum.0);
        parent.cum_dsize = parent.cum_dsize.saturating_add(cum.1);
        parent.items = parent.items.saturating_add(items + 1);
        parent.sub_error |= error;
        let at = self.put(item, prev, sub)?;
        self.stack.last_mut().expect("parent").last = Some(at);
        Ok(())
    }

    /// The item of a closed directory, but for its last entry (`sub`),
    /// which [`Writer::put`] adds. Like ncdu, the shared sizes are those of
    /// hard links below it that have links elsewhere too.
    fn dir_item(d: &OpenDir) -> ItemOut {
        let (mut shr_a, mut shr_d) = (0u64, 0u64);
        for l in d.links.values().filter(|l| l.seen < l.nlink) {
            shr_a = shr_a.saturating_add(l.asize);
            shr_d = shr_d.saturating_add(l.dsize);
        }
        let mut it = ItemOut::new(DIR, &d.name);
        it.uint(3, d.asize);
        it.uint(4, d.dsize);
        if d.own_dev {
            it.uint(5, d.dev);
        }
        if d.read_error || d.sub_error {
            it.boolean(6, d.read_error);
        }
        it.uint(7, d.cum_asize);
        it.uint(8, d.cum_dsize);
        if shr_a > 0 || shr_d > 0 {
            it.uint(9, shr_a);
            it.uint(10, shr_d);
        }
        it.uint(11, d.items);
        if d.mtime > 0 {
            it.uint(18, d.mtime as u64);
        }
        it
    }
}

impl<W: Write> Sink for Writer<'_, W> {
    fn root(&mut self, path: &[u8], st: &du_fs::Stat, read_error: bool) -> io::Result<()> {
        self.write(SIGNATURE)?;
        self.stack.push(OpenDir::new(path, st, true, read_error));
        Ok(())
    }

    fn leaf(&mut self, name: &[u8], leaf: Leaf<'_>, parent_dev: u64) -> io::Result<()> {
        let (item, cum, error) = match leaf {
            Leaf::Excluded => (ItemOut::new(EXCLUDED, name), (0, 0), false),
            Leaf::Unreadable => (ItemOut::new(ERROR, name), (0, 0), true),
            Leaf::File(st) => {
                let link = st.nlink > 1;
                let kind = if link {
                    LINK
                } else if st.is_file() {
                    FILE
                } else {
                    OTHER
                };
                let (asize, dsize) = st.sizes();
                let mut it = ItemOut::new(kind, name);
                it.uint(3, asize);
                it.uint(4, dsize);
                if link {
                    it.uint(13, st.ino);
                    it.uint(14, st.nlink.min(u64::from(u32::MAX)));
                }
                if st.mtime > 0 {
                    it.uint(18, st.mtime as u64);
                }
                // A directory counts each hard-linked inode below it once.
                let mut first = true;
                if link {
                    let parent = self
                        .stack
                        .last_mut()
                        .ok_or_else(|| bad("entry outside a directory"))?;
                    let l = parent.links.entry((st.dev, st.ino)).or_insert(Link {
                        seen: 0,
                        nlink: st.nlink,
                        asize,
                        dsize,
                    });
                    first = l.seen == 0;
                    l.seen += 1;
                }
                (it, if first { (asize, dsize) } else { (0, 0) }, false)
            }
            Leaf::Dir(_, DirLeaf::OtherFs) => (ItemOut::new(OTHER_FS, name), (0, 0), false),
            Leaf::Dir(_, DirLeaf::Cache) => (ItemOut::new(EXCLUDED, name), (0, 0), false),
            Leaf::Dir(st, DirLeaf::Unread) => {
                let d = OpenDir::new(name, st, st.dev != parent_dev, true);
                (Self::dir_item(&d), (d.cum_asize, d.cum_dsize), true)
            }
        };
        self.entry(item, None, cum, 0, error)
    }

    fn open(
        &mut self,
        name: &[u8],
        st: &du_fs::Stat,
        parent_dev: u64,
        read_error: bool,
    ) -> io::Result<()> {
        self.stack
            .push(OpenDir::new(name, st, st.dev != parent_dev, read_error));
        Ok(())
    }

    fn close(&mut self) -> io::Result<()> {
        let mut d = self.stack.pop().ok_or_else(|| bad("close without open"))?;
        let item = Self::dir_item(&d);
        let Some(parent) = self.stack.last_mut() else {
            self.root = Some(self.put(item, None, d.last)?);
            return Ok(());
        };
        let dup = merge_links(&mut parent.links, std::mem::take(&mut d.links));
        self.entry(
            item,
            d.last,
            (
                d.cum_asize.saturating_sub(dup.0),
                d.cum_dsize.saturating_sub(dup.1),
            ),
            d.items,
            d.read_error || d.sub_error,
        )
    }

    fn finish(&mut self) -> io::Result<()> {
        let root = self.root.ok_or_else(|| bad("no root written"))?;
        self.flush_block()?;
        self.drain()?;
        let len = 8 + 8 * self.pointers.len() + 8;
        if len >= 1 << 28 {
            return Err(bad("too many blocks for the index"));
        }
        let head = ((1u32 << 28) | len as u32).to_be_bytes();
        let mut index = Vec::with_capacity(len);
        index.extend(head);
        for p in &self.pointers {
            index.extend(p.to_be_bytes());
        }
        index.extend(root.to_be_bytes());
        index.extend(head);
        self.write(&index)?;
        self.w.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::path::Path;

    const REAL: &[u8] = include_bytes!("../tests/data/ncdu-2.9.1-export.ncdu");
    const REAL_JSON: &str = include_str!("../tests/data/ncdu-2.9.1-export.json");

    fn read(bytes: &[u8]) -> io::Result<Snapshot> {
        parse(Cursor::new(bytes), 7)
    }

    /// A CBOR head: major type and argument.
    fn head(out: &mut Vec<u8>, major: u8, n: u64) {
        let m = major << 5;
        match n {
            0..=23 => out.push(m | n as u8),
            24..=0xff => out.extend([m | 24, n as u8]),
            0x100..=0xffff => {
                out.push(m | 25);
                out.extend((n as u16).to_be_bytes());
            }
            0x1_0000..=0xffff_ffff => {
                out.push(m | 26);
                out.extend((n as u32).to_be_bytes());
            }
            _ => {
                out.push(m | 27);
                out.extend(n.to_be_bytes());
            }
        }
    }

    enum V<'a> {
        U(u64),
        Neg(u64),
        Bytes(&'a str),
        Text(&'a str),
        Raw(&'a [u8]),
    }

    /// An item: a map of `fields`, of indefinite length when `open`.
    fn item(fields: &[(u64, V)], open: bool) -> Vec<u8> {
        let mut out = Vec::new();
        if open {
            out.push(0xbf);
        } else {
            head(&mut out, 5, fields.len() as u64);
        }
        for (k, v) in fields {
            head(&mut out, 0, *k);
            match v {
                V::U(n) => head(&mut out, 0, *n),
                V::Neg(n) => head(&mut out, 1, n - 1),
                V::Bytes(s) => {
                    head(&mut out, 2, s.len() as u64);
                    out.extend(s.as_bytes());
                }
                V::Text(s) => {
                    head(&mut out, 3, s.len() as u64);
                    out.extend(s.as_bytes());
                }
                V::Raw(r) => out.extend(*r),
            }
        }
        if open {
            out.push(0xff);
        }
        out
    }

    /// An export of `blocks` (number, decompressed items), written in the
    /// given order, with `root` as the root reference.
    fn export(blocks: &[(u32, Vec<u8>)], root: u64) -> Vec<u8> {
        let mut out = SIGNATURE.to_vec();
        let count = blocks.iter().map(|b| b.0 + 1).max().unwrap_or(0) as usize;
        let mut pointers = vec![0u64; count];
        for (n, data) in blocks {
            let z = ruzstd::encoding::compress_to_vec(
                &data[..],
                ruzstd::encoding::CompressionLevel::Fastest,
            );
            let len = (z.len() + 12) as u32;
            pointers[*n as usize] = ((out.len() as u64) << 24) | u64::from(len);
            out.extend(len.to_be_bytes());
            out.extend(n.to_be_bytes());
            out.extend(&z);
            out.extend(len.to_be_bytes());
        }
        let ilen = ((pointers.len() + 1) * 8 + 8) as u32 | (1 << 28);
        out.extend(ilen.to_be_bytes());
        for p in pointers {
            out.extend(p.to_be_bytes());
        }
        out.extend(root.to_be_bytes());
        out.extend(ilen.to_be_bytes());
        out
    }

    fn at(block: u64, off: usize) -> u64 {
        (block << 24) | off as u64
    }

    fn row<'a>(snap: &'a Snapshot, p: &str) -> &'a crate::du::Item {
        snap.dirs
            .iter()
            .find(|d| d.path == Path::new(p))
            .unwrap_or_else(|| panic!("no row {p}: {:?}", snap.dirs))
    }

    #[test]
    fn a_real_export_reads_as_its_json_conversion() {
        assert!(looks_like_binary(REAL));
        assert!(!looks_like_binary(b"[1,2,"));
        let bin = read(REAL).unwrap();
        let json = crate::du_ncdu::parse(REAL_JSON.as_bytes()).unwrap();
        let r = &bin.roots[0];
        assert_eq!(r.path, Path::new("/tmp/ncdu-fixture"));
        assert_eq!(format!("{:?}", bin.roots), format!("{:?}", json.roots));
        let key = |s: &Snapshot| {
            let mut v: Vec<String> = s.dirs.iter().map(|d| format!("{d:?}")).collect();
            v.sort();
            v
        };
        assert_eq!(key(&bin), key(&json));
        // One hard link counted once, one excluded log, and one
        // unreadable directory, with a row as a scan gives it one.
        assert_eq!((r.links, r.excluded), (1, 1));
        let locked = row(&bin, "/tmp/ncdu-fixture/locked");
        assert!(locked.path.ends_with("locked"));
        assert_eq!(locked.items, 0);
        assert_eq!(bin.unreadable, Some(1));
        assert_eq!((json.unreadable, &json.unread), (Some(1), &bin.unread));
        assert_eq!(bin.unread[0].0, Path::new("/tmp/ncdu-fixture/locked"));
        assert_eq!(bin.created, 7);
    }

    #[test]
    fn blocks_in_any_order_with_both_kinds_of_reference_read_whole() {
        // Block 0: two files of /r/d, the second pointing back at the
        // first; then a link candidate seen twice.
        let f1 = item(
            &[
                (0, V::U(1)),
                (1, V::Bytes("f1")),
                (3, V::U(10)),
                (4, V::U(4096)),
                (18, V::U(1_700_000_000)),
            ],
            false,
        );
        let f2_at = f1.len();
        let f2 = item(
            &[
                (1, V::Text("f2")),
                (0, V::U(3)),
                (4, V::U(8192)),
                (13, V::U(77)),
                (14, V::U(2)),
                (2, V::Neg(f2_at as u64)),
                // Unknown keys, nested values included, are skipped.
                (
                    99,
                    V::Raw(&[0x82, 0xa1, 0x01, 0x5f, 0x41, 0x00, 0xff, 0xf6]),
                ),
                (18, V::U(1_700_000_000)),
            ],
            true,
        );
        let x_at = f1.len() + f2.len();
        let x = item(
            &[
                (0, V::U(3)),
                (1, V::Bytes("again")),
                (4, V::U(8192)),
                (13, V::U(77)),
                (14, V::U(2)),
            ],
            false,
        );
        let skip_at = x_at + x.len();
        let skip = item(
            &[
                (0, V::Neg(2)),
                (1, V::Bytes("skip.log")),
                (2, V::U(at(0, x_at))),
            ],
            false,
        );
        let block0 = [f1, f2, x, skip].concat();
        // Block 1: the directory and the root.
        let d = item(
            &[
                (0, V::U(0)),
                (1, V::Bytes("d")),
                (4, V::U(4096)),
                (12, V::U(at(0, f2_at))),
                (2, V::U(at(0, skip_at))),
            ],
            false,
        );
        let root_at = d.len();
        let root = item(
            &[
                (0, V::U(0)),
                (1, V::Bytes("/r/")),
                (4, V::U(4096)),
                (5, V::U(9)),
                (12, V::Neg(root_at as u64)),
            ],
            false,
        );
        let file = export(&[(1, [d, root].concat()), (0, block0)], at(1, root_at));
        let snap = read(&file).unwrap();
        let r = &snap.roots[0];
        assert_eq!(r.path, Path::new("/r"));
        assert_eq!((r.files, r.links, r.dirs, r.excluded), (2, 1, 2, 1));
        assert_eq!(r.disk, 4096 + 4096 + 4096 + 8192);
        // Read in the order written, the link candidate in /r comes
        // before its second name in /r/d, so /r is charged for it.
        let d = row(&snap, "/r/d");
        assert_eq!((d.disk, d.apparent, d.items), (4096 + 4096, 10, 1));
        assert_eq!(d.mtime, 1_700_000_000);
    }

    #[test]
    fn broken_exports_are_errors_not_panics() {
        let a = item(
            &[(0, V::U(1)), (1, V::Bytes("a")), (2, V::U(at(0, 0)))],
            false,
        );
        let root_at = a.len();
        let root = item(
            &[(0, V::U(0)), (1, V::Bytes("/r")), (12, V::U(at(0, 0)))],
            false,
        );
        // An entry that points at itself.
        let looped = export(&[(0, [a, root].concat())], at(0, root_at));
        let e = read(&looped).unwrap_err().to_string();
        assert!(e.contains("loop"), "{e}");
        // A relative reference before the start of its block.
        let root = item(&[(0, V::U(0)), (1, V::Bytes("/r")), (12, V::Neg(5))], false);
        let e = read(&export(&[(0, root)], 0)).unwrap_err().to_string();
        assert!(e.contains("before its block"), "{e}");
        // A root in a block that is not there, and one past its block.
        let root = item(&[(0, V::U(0)), (1, V::Bytes("/r"))], false);
        let ok = export(&[(0, root.clone())], 0);
        assert!(read(&ok).is_ok());
        assert!(read(&export(&[(0, root.clone())], at(3, 0))).is_err());
        assert!(read(&export(&[(0, root.clone())], at(0, 900))).is_err());
        // A file as root, a name longer than its block, a bad signature,
        // and every truncation of a real export.
        let f = item(&[(0, V::U(1)), (1, V::Bytes("/r"))], false);
        assert!(read(&export(&[(0, f)], 0)).is_err());
        let mut long = item(&[(0, V::U(0))], false);
        long[0] = 0xa2;
        long.extend([0x01, 0x58, 0xff, b'x']);
        assert!(read(&export(&[(0, long)], 0)).is_err());
        let mut sig = ok.clone();
        sig[1] = b'N';
        assert!(read(&sig).is_err());
        for cut in 0..REAL.len() {
            assert!(read(&REAL[..cut]).is_err(), "cut at {cut}");
        }
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "rtrash-ncdubin-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// The directory item at `path` below the root of a written export,
    /// with its numeric fields by key.
    fn dir_at(buf: &[u8], path: &[&str]) -> (Item, std::collections::BTreeMap<u64, u64>) {
        let mut b = Blocks::open(io::Cursor::new(buf)).unwrap();
        let mut at = b.root;
        for name in path {
            let mut next = b.item(at).unwrap().sub;
            at = loop {
                let here = next.unwrap_or_else(|| panic!("no {name}"));
                let it = b.item(here).unwrap();
                if it.info.name == name.as_bytes() {
                    break here;
                }
                next = it.prev;
            };
        }
        let item = b.item(at).unwrap();
        let data = b.block((at >> 24) as usize).unwrap();
        let mut c = Cbor {
            data,
            pos: (at & 0xff_ffff) as usize,
        };
        let (_, len) = c.head().unwrap();
        let mut fields = std::collections::BTreeMap::new();
        for _ in 0..len.unwrap() {
            let key = c.uint().unwrap();
            if matches!(key, 3 | 4 | 7..=11) {
                fields.insert(key, c.uint().unwrap());
            } else {
                c.skip().unwrap();
            }
        }
        (item, fields)
    }

    #[test]
    fn encoder_frames_gain_the_content_size_the_format_needs() {
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let frame = ruzstd::encoding::compress_to_vec(
            &data[..],
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        let frame = with_content_size(frame, data.len()).unwrap();
        let mut d = ruzstd::decoding::FrameDecoder::new();
        d.init(&frame[..]).unwrap();
        assert_eq!(d.content_size(), data.len() as u64);
        let mut out = Vec::new();
        ruzstd::decoding::StreamingDecoder::new(&frame[..])
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, data);
        // A block that repeats for longer than 64 KiB, the match lengths
        // ruzstd 0.9.1 fixed the encoding of, reads back whole.
        let zeros = vec![0u8; BLOCK_TARGET];
        let z = ruzstd::encoding::compress_to_vec(
            &zeros[..],
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        let z = with_content_size(z, zeros.len()).unwrap();
        let mut back = Vec::new();
        ruzstd::decoding::StreamingDecoder::new(&z[..])
            .unwrap()
            .read_to_end(&mut back)
            .unwrap();
        assert_eq!(back, zeros);
        // A header with a size already, or not zstd at all, is refused.
        assert!(with_content_size(frame, 1).is_err());
        assert!(with_content_size(b"not zstd".to_vec(), 1).is_err());
    }

    #[test]
    fn written_exports_read_back_like_the_json_export() {
        use std::fs;
        let root = tmp("rt");
        // Enough entries for more than one data block.
        for i in 0..3000 {
            let d = root.join(format!("d{}", i % 7));
            fs::create_dir_all(&d).unwrap();
            fs::write(
                d.join(format!("a-file-with-a-long-name-{i}")),
                vec![7u8; i % 500],
            )
            .unwrap();
        }
        fs::create_dir_all(root.join("logs/old")).unwrap();
        fs::write(root.join("logs/x.log"), b"skipped").unwrap();
        fs::write(root.join("logs/old/y"), b"kept").unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        let spec = crate::du_filter::Spec {
            patterns: vec!["*.log".into()],
            ..crate::du_filter::Spec::default()
        };
        let mut bin = Vec::new();
        let done = crate::du_ncdu::export_binary(&root, false, Some(&spec), &mut bin).unwrap();
        assert_eq!(done.excluded, 1);
        let mut json = Vec::new();
        crate::du_ncdu::export(&root, false, Some(&spec), 5, &mut json).unwrap();
        assert!(looks_like_binary(&bin));
        assert!(
            Blocks::open(io::Cursor::new(&bin[..]))
                .unwrap()
                .pointers
                .len()
                > 1
        );
        let a = read(&bin).unwrap();
        let b = crate::du_ncdu::parse(&json[..]).unwrap();
        assert_eq!(format!("{:?}", a.roots), format!("{:?}", b.roots));
        let key = |s: &Snapshot| {
            let mut v: Vec<String> = s.dirs.iter().map(|d| format!("{d:?}")).collect();
            v.sort();
            v
        };
        assert_eq!(key(&a), key(&b));
        assert_eq!(a.roots[0].files, 3001);

        // The cumulative fields carry the totals, the root included.
        let (top, f) = dir_at(&bin, &[]);
        assert_eq!(top.kind, 0);
        assert!(top.info.dev.is_some());
        // A snapshot's apparent total is of files and its disk total
        // counts directories too; ncdu's fields count directories in both.
        let names: Vec<String> = (0..7).map(|i| format!("d{i}")).collect();
        let mut dirs: Vec<Vec<&str>> = names.iter().map(|n| vec![n.as_str()]).collect();
        dirs.extend([vec![], vec!["logs"], vec!["logs", "old"], vec!["empty"]]);
        let own: u64 = dirs.iter().map(|p| dir_at(&bin, p).1[&3]).sum();
        assert_eq!(f[&7], a.roots[0].apparent + own);
        assert_eq!(f[&8], a.roots[0].disk);
        // 7 + 3 directories, 3001 files and, as ncdu counts it, the
        // excluded log below the root.
        assert_eq!(f[&11], 3012);
        let (_, f) = dir_at(&bin, &["logs", "old"]);
        assert_eq!((f[&3] + 4, f[&11]), (f[&7], 1));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn references_in_one_block_are_relative_and_a_failed_write_stops() {
        use std::fs;
        let root = tmp("refs");
        fs::create_dir_all(root.join("d")).unwrap();
        fs::write(root.join("d/a"), b"a").unwrap();
        fs::write(root.join("d/b"), b"b").unwrap();
        let mut bin = Vec::new();
        crate::du_ncdu::export_binary(&root, false, None, &mut bin).unwrap();
        let mut b = Blocks::open(io::Cursor::new(&bin[..])).unwrap();
        let at = b.root;
        let data = b.block(0).unwrap();
        let top = Item::parse(data, (at & 0xff_ffff) as usize).unwrap();
        assert!(matches!(top.sub_ref, Some(Ref::Back(_))), "{top:?}");
        // The two files of d: the second points back at the first.
        let d_at = b.item(at).unwrap().sub.unwrap();
        let last_at = b.item(d_at).unwrap().sub.unwrap();
        let off = (last_at & 0xff_ffff) as usize;
        let raw = Item::parse(b.block(0).unwrap(), off).unwrap();
        assert!(matches!(raw.prev_ref, Some(Ref::Back(_))), "{raw:?}");
        assert!(b.item(last_at).unwrap().prev.is_some());

        // A writer that fails partway: an error, and no thread left waiting.
        struct Fails(usize);
        impl Write for Fails {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                if self.0 < b.len() {
                    return Err(io::Error::other("disk full"));
                }
                self.0 -= b.len();
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        for room in [0, 8, 20, bin.len() - 10] {
            let r = crate::du_ncdu::export_binary(&root, false, None, &mut Fails(room));
            assert!(r.is_err(), "room {room}");
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn hard_links_in_sibling_directories_read_alike_in_both_formats() {
        use std::fs;
        let root = tmp("siblings");
        for d in ["a", "b", "c"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(root.join("b/f"), vec![1u8; 50_000]).unwrap();
        fs::hard_link(root.join("b/f"), root.join("a/l")).unwrap();
        fs::hard_link(root.join("b/f"), root.join("c/l")).unwrap();
        let mut bin = Vec::new();
        crate::du_ncdu::export_binary(&root, false, None, &mut bin).unwrap();
        let mut json = Vec::new();
        crate::du_ncdu::export(&root, false, None, 5, &mut json).unwrap();
        let rows = |s: &Snapshot| {
            let mut v: Vec<String> = s.dirs.iter().map(|d| format!("{d:?}")).collect();
            v.sort();
            v
        };
        let (a, b) = (
            read(&bin).unwrap(),
            crate::du_ncdu::parse(&json[..]).unwrap(),
        );
        // Each format charges the file to the directory walked first.
        assert_eq!(rows(&a), rows(&b));
        let charged = a.dirs.iter().filter(|d| d.disk > 40_000).count();
        assert_eq!(charged, 1, "{:?}", a.dirs);
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn hard_links_count_once_per_directory_and_share_like_ncdu() {
        use std::fs;
        let root = tmp("links");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("x")).unwrap();
        fs::write(root.join("a/f"), vec![1u8; 10_000]).unwrap();
        fs::hard_link(root.join("a/f"), root.join("a/b/l1")).unwrap();
        fs::hard_link(root.join("a/f"), root.join("x/l2")).unwrap();
        let mut bin = Vec::new();
        crate::du_ncdu::export_binary(&root, false, None, &mut bin).unwrap();
        let own = |f: &std::collections::BTreeMap<u64, u64>| f[&3];
        // Each directory counts the inode once, and all but the root
        // share it with links outside them.
        for (path, shared) in [
            (&["a"][..], true),
            (&["a", "b"][..], true),
            (&["x"][..], true),
            (&[][..], false),
        ] {
            let (_, f) = dir_at(&bin, path);
            let subdirs: u64 = match path {
                ["a"] => dir_at(&bin, &["a", "b"]).1[&3],
                [] => {
                    dir_at(&bin, &["a"]).1[&3]
                        + dir_at(&bin, &["a", "b"]).1[&3]
                        + dir_at(&bin, &["x"]).1[&3]
                }
                _ => 0,
            };
            assert_eq!(f[&7], own(&f) + subdirs + 10_000, "{path:?}");
            assert_eq!(f.get(&9).copied(), shared.then_some(10_000), "{path:?}");
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_directories_are_marked_and_their_parents_flag_it() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let root = tmp("err");
        fs::create_dir_all(root.join("ok/locked/inner")).unwrap();
        fs::set_permissions(root.join("ok/locked"), fs::Permissions::from_mode(0o000)).unwrap();
        let mut bin = Vec::new();
        let done = crate::du_ncdu::export_binary(&root, false, None, &mut bin).unwrap();
        fs::set_permissions(root.join("ok/locked"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(done.unreadable, 1);
        let (locked, f) = dir_at(&bin, &["ok", "locked"]);
        assert!(locked.info.read_error);
        assert_eq!(f[&7], f[&3]);
        // The parent says an entry below failed, without failing itself.
        let (ok, _) = dir_at(&bin, &["ok"]);
        assert!(!ok.info.read_error);
        let snap = read(&bin).unwrap();
        assert!(snap.dirs.iter().all(|d| !d.path.ends_with("inner")));
        let _ = fs::remove_dir_all(&root);
    }
}
