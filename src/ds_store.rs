//! A reader (and a small writer) for Finder's `.DS_Store` files, enough to
//! find the Put Back records Finder leaves in `~/.Trash/.DS_Store`.
//!
//! The format: a 4-byte `00 00 00 01` prefix, then a buddy allocator whose
//! header names a block table and a table of contents. The `DSDB` entry
//! points at a B-tree of records, each `(file name, 4-byte code, typed
//! value)`. Finder stores the original folder of a trashed item under code
//! `ptbL` (a volume-relative path) and its original name under `ptbN`.
//! All integers are big-endian; names and strings are UTF-16BE. Every read
//! here checks its bounds, so a damaged file yields an error, not a panic.

use std::collections::HashMap;

/// One typed value of a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Long(u32),
    Short(u32),
    Bool(bool),
    Blob(Vec<u8>),
    Type([u8; 4]),
    Ustr(String),
    Comp(u64),
    Dutc(u64),
}

/// One record: the file it describes, a 4-byte code and its value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub name: String,
    pub code: [u8; 4],
    pub value: Value,
}

/// Where Finder's Put Back would return an item: the folder relative to
/// the volume root (`ptbL`, such as `Users/ann/Desktop/`) and the original
/// name (`ptbN`), when the record has one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PutBack {
    pub folder: Option<String>,
    pub name: Option<String>,
}

/// The Put Back records in `records`, keyed by the name inside the trash.
pub fn put_back(records: &[Record]) -> HashMap<String, PutBack> {
    let mut out: HashMap<String, PutBack> = HashMap::new();
    for r in records {
        let Value::Ustr(s) = &r.value else { continue };
        match &r.code {
            b"ptbL" => out.entry(r.name.clone()).or_default().folder = Some(s.clone()),
            b"ptbN" => out.entry(r.name.clone()).or_default().name = Some(s.clone()),
            _ => {}
        }
    }
    out
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.data.len())
            .ok_or_else(|| format!("truncated at byte {}", self.pos))?;
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, String> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let hi = u64::from(self.u32()?);
        Ok((hi << 32) | u64::from(self.u32()?))
    }

    fn code(&mut self) -> Result<[u8; 4], String> {
        let b = self.take(4)?;
        Ok([b[0], b[1], b[2], b[3]])
    }

    fn utf16(&mut self) -> Result<String, String> {
        let units = self.u32()? as usize;
        let bytes = self.take(units.checked_mul(2).ok_or("string length overflows")?)?;
        let v: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_be_bytes(*c))
            .collect();
        Ok(String::from_utf16_lossy(&v))
    }

    fn record(&mut self) -> Result<Record, String> {
        let name = self.utf16()?;
        let code = self.code()?;
        let kind = self.code()?;
        let value = match &kind {
            b"long" => Value::Long(self.u32()?),
            b"shor" => Value::Short(self.u32()?),
            b"bool" => Value::Bool(self.u8()? != 0),
            b"blob" => {
                let n = self.u32()? as usize;
                Value::Blob(self.take(n)?.to_vec())
            }
            b"type" => Value::Type(self.code()?),
            b"ustr" => Value::Ustr(self.utf16()?),
            b"comp" => Value::Comp(self.u64()?),
            b"dutc" => Value::Dutc(self.u64()?),
            other => {
                return Err(format!(
                    "unknown value type '{}' in record for '{name}'",
                    String::from_utf8_lossy(other)
                ))
            }
        };
        Ok(Record { name, code, value })
    }
}

/// Deepest B-tree accepted; Finder's trees stay a few levels deep.
const MAX_DEPTH: usize = 32;

/// Every record in a `.DS_Store` file, in B-tree order.
pub fn parse(data: &[u8]) -> Result<Vec<Record>, String> {
    if data.len() < 36 || data[..4] != [0, 0, 0, 1] || &data[4..8] != b"Bud1" {
        return Err("not a .DS_Store file (bad magic)".into());
    }
    // Offsets count from the end of the 4-byte prefix.
    let base = &data[4..];
    let mut hdr = Reader::new(&base[4..]);
    let info_off = hdr.u32()? as usize;
    let info_size = hdr.u32()? as usize;
    if hdr.u32()? as usize != info_off {
        return Err("header offsets disagree".into());
    }
    let info = info_off
        .checked_add(info_size)
        .and_then(|end| base.get(info_off..end))
        .ok_or("allocator block lies outside the file")?;
    let mut r = Reader::new(info);
    let count = r.u32()? as usize;
    r.u32()?;
    let padded = count.div_ceil(256) * 256;
    let mut addrs = Vec::with_capacity(count.min(4096));
    for i in 0..padded {
        let a = r.u32()?;
        if i < count {
            addrs.push(a);
        }
    }
    let ntoc = r.u32()?;
    let mut dsdb = None;
    for _ in 0..ntoc {
        let n = r.u8()? as usize;
        let key = r.take(n)?;
        let id = r.u32()?;
        if key == b"DSDB" {
            dsdb = Some(id);
        }
    }
    let dsdb = dsdb.ok_or("no DSDB entry")?;
    let block = |id: u32| -> Result<&[u8], String> {
        let addr = *addrs
            .get(id as usize)
            .ok_or_else(|| format!("block {id} missing from the table"))?;
        let off = (addr & !0x1f) as usize;
        let size = 1usize
            .checked_shl(addr & 0x1f)
            .ok_or("block size overflows")?;
        off.checked_add(size)
            .and_then(|end| base.get(off..end))
            .ok_or_else(|| format!("block {id} lies outside the file"))
    };
    let mut head = Reader::new(block(dsdb)?);
    let root = head.u32()?;
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    walk(&block, root, 0, &mut seen, &mut out)?;
    Ok(out)
}

fn walk<'a>(
    block: &dyn Fn(u32) -> Result<&'a [u8], String>,
    id: u32,
    depth: usize,
    seen: &mut std::collections::HashSet<u32>,
    out: &mut Vec<Record>,
) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err("B-tree too deep".into());
    }
    if !seen.insert(id) {
        return Err(format!("B-tree loops back to block {id}"));
    }
    let mut r = Reader::new(block(id)?);
    let right = r.u32()?;
    let count = r.u32()?;
    for _ in 0..count {
        if right != 0 {
            let child = r.u32()?;
            walk(block, child, depth + 1, seen, out)?;
        }
        out.push(r.record()?);
    }
    if right != 0 {
        walk(block, right, depth + 1, seen, out)?;
    }
    Ok(())
}

fn put_utf16(out: &mut Vec<u8>, s: &str) {
    let units: Vec<u16> = s.encode_utf16().collect();
    out.extend_from_slice(&(units.len() as u32).to_be_bytes());
    for u in units {
        out.extend_from_slice(&u.to_be_bytes());
    }
}

/// Serialise one record as it appears inside a B-tree node.
pub fn encode_record(r: &Record) -> Vec<u8> {
    let mut out = Vec::new();
    put_utf16(&mut out, &r.name);
    out.extend_from_slice(&r.code);
    match &r.value {
        Value::Long(v) => {
            out.extend_from_slice(b"long");
            out.extend_from_slice(&v.to_be_bytes());
        }
        Value::Short(v) => {
            out.extend_from_slice(b"shor");
            out.extend_from_slice(&v.to_be_bytes());
        }
        Value::Bool(v) => {
            out.extend_from_slice(b"bool");
            out.push(u8::from(*v));
        }
        Value::Blob(b) => {
            out.extend_from_slice(b"blob");
            out.extend_from_slice(&(b.len() as u32).to_be_bytes());
            out.extend_from_slice(b);
        }
        Value::Type(t) => {
            out.extend_from_slice(b"type");
            out.extend_from_slice(t);
        }
        Value::Ustr(s) => {
            out.extend_from_slice(b"ustr");
            put_utf16(&mut out, s);
        }
        Value::Comp(v) => {
            out.extend_from_slice(b"comp");
            out.extend_from_slice(&v.to_be_bytes());
        }
        Value::Dutc(v) => {
            out.extend_from_slice(b"dutc");
            out.extend_from_slice(&v.to_be_bytes());
        }
    }
    out
}

/// Assemble a `.DS_Store` image from raw blocks: `blocks[i]` becomes block
/// `i + 1` (block 0 holds the allocator itself), and `dsdb` names the block
/// with the B-tree header. Each block gets a power-of-two slot aligned to
/// its size, as the buddy allocator expects.
pub fn assemble(blocks: &[Vec<u8>], dsdb: u32) -> Vec<u8> {
    let slot = |len: usize| len.max(32).next_power_of_two();
    // Allocator info: count, zero, block table padded to 256 entries, the
    // table of contents, and 32 empty free lists.
    let count = blocks.len() + 1;
    let padded = count.div_ceil(256) * 256;
    let info_len = 8 + 4 * padded + 4 + 1 + 4 + 4 + 32 * 4;
    let info_slot = slot(info_len);
    // The first 32 bytes after the prefix hold the header.
    let mut offsets = Vec::with_capacity(blocks.len());
    let mut next = 32usize;
    for b in blocks {
        let s = slot(b.len());
        next = next.div_ceil(s) * s;
        offsets.push(next);
        next += s;
    }
    let info_off = next.div_ceil(info_slot) * info_slot;
    let total = info_off + info_slot;
    let mut base = vec![0u8; total];
    let addr = |off: usize, len: usize| (off as u32) | slot(len).trailing_zeros();
    let mut info = Vec::with_capacity(info_len);
    info.extend_from_slice(&(count as u32).to_be_bytes());
    info.extend_from_slice(&0u32.to_be_bytes());
    for i in 0..padded {
        let a = match i {
            0 => addr(info_off, info_len),
            i if i < count => addr(offsets[i - 1], blocks[i - 1].len()),
            _ => 0,
        };
        info.extend_from_slice(&a.to_be_bytes());
    }
    info.extend_from_slice(&1u32.to_be_bytes());
    info.push(4);
    info.extend_from_slice(b"DSDB");
    info.extend_from_slice(&dsdb.to_be_bytes());
    info.extend_from_slice(&[0u8; 32 * 4]);
    base[info_off..info_off + info.len()].copy_from_slice(&info);
    for (b, &off) in blocks.iter().zip(&offsets) {
        base[off..off + b.len()].copy_from_slice(b);
    }
    base[..4].copy_from_slice(b"Bud1");
    base[4..8].copy_from_slice(&(info_off as u32).to_be_bytes());
    base[8..12].copy_from_slice(&(info_slot as u32).to_be_bytes());
    base[12..16].copy_from_slice(&(info_off as u32).to_be_bytes());
    let mut out = vec![0, 0, 0, 1];
    out.extend_from_slice(&base);
    out
}

/// The B-tree header block: root node, levels, records, nodes, page size.
pub fn dsdb_header(root: u32, levels: u32, records: u32, nodes: u32) -> Vec<u8> {
    [root, levels, records, nodes, 0x1000]
        .iter()
        .flat_map(|v| v.to_be_bytes())
        .collect()
}

/// A `.DS_Store` image holding `records` in one leaf, sorted the way
/// Finder sorts them (name without case, then code).
pub fn write(records: &[Record]) -> Vec<u8> {
    let mut sorted: Vec<&Record> = records.iter().collect();
    sorted.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.code.cmp(&b.code))
    });
    let mut leaf = Vec::new();
    leaf.extend_from_slice(&0u32.to_be_bytes());
    leaf.extend_from_slice(&(sorted.len() as u32).to_be_bytes());
    for r in &sorted {
        leaf.extend_from_slice(&encode_record(r));
    }
    let head = dsdb_header(2, 0, sorted.len() as u32, 1);
    assemble(&[head, leaf], 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ustr(name: &str, code: &[u8; 4], s: &str) -> Record {
        Record {
            name: name.into(),
            code: *code,
            value: Value::Ustr(s.into()),
        }
    }

    #[test]
    fn write_then_parse_keeps_every_value_type() {
        let recs = vec![
            ustr("b.txt", b"ptbN", "b.txt"),
            ustr("a 10-11-12", b"ptbL", "Users/ann/Desktop/"),
            ustr("a 10-11-12", b"ptbN", "a"),
            Record {
                name: "c".into(),
                code: *b"Iloc",
                value: Value::Blob(vec![0, 1, 2, 3, 255]),
            },
            Record {
                name: "c".into(),
                code: *b"dilc",
                value: Value::Long(7),
            },
            Record {
                name: "c".into(),
                code: *b"ICVO",
                value: Value::Bool(true),
            },
            Record {
                name: "c".into(),
                code: *b"vstl",
                value: Value::Type(*b"icnv"),
            },
            Record {
                name: "c".into(),
                code: *b"logS",
                value: Value::Comp(1 << 40),
            },
            Record {
                name: "c".into(),
                code: *b"modD",
                value: Value::Dutc(u64::MAX),
            },
            Record {
                name: "ünï 😀".into(),
                code: *b"fwsw",
                value: Value::Short(3),
            },
        ];
        let img = write(&recs);
        let back = parse(&img).unwrap();
        assert_eq!(back.len(), recs.len());
        for r in &recs {
            assert!(back.contains(r), "{r:?} lost");
        }
        // Sorted without case, then by code.
        assert_eq!(back[0].name, "a 10-11-12");
        assert_eq!(back[0].code, *b"ptbL");
        let pb = put_back(&back);
        assert_eq!(
            pb["a 10-11-12"],
            PutBack {
                folder: Some("Users/ann/Desktop/".into()),
                name: Some("a".into()),
            }
        );
        assert_eq!(pb["b.txt"].folder, None);
        assert!(!pb.contains_key("c"));
    }

    #[test]
    fn parse_walks_an_internal_node_in_order() {
        // Root block 2 holds one separator record with child block 3 on
        // its left and block 4 on its right.
        let leaf = |recs: &[Record]| {
            let mut b = Vec::new();
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&(recs.len() as u32).to_be_bytes());
            for r in recs {
                b.extend_from_slice(&encode_record(r));
            }
            b
        };
        let a = ustr("a", b"ptbN", "a");
        let m = ustr("m", b"ptbN", "m");
        let z = ustr("z", b"ptbN", "z");
        let mut root = Vec::new();
        root.extend_from_slice(&4u32.to_be_bytes());
        root.extend_from_slice(&1u32.to_be_bytes());
        root.extend_from_slice(&3u32.to_be_bytes());
        root.extend_from_slice(&encode_record(&m));
        let img = assemble(
            &[
                dsdb_header(2, 1, 3, 3),
                root,
                leaf(std::slice::from_ref(&a)),
                leaf(std::slice::from_ref(&z)),
            ],
            1,
        );
        assert_eq!(parse(&img).unwrap(), vec![a, m, z]);
    }

    /// A file from another implementation, the Python `ds_store` package
    /// (one leaf; that package corrupts the tree when it splits a node):
    ///
    /// ```text
    /// for i in range(25):
    ///     n = f'file{i:03d}.txt'
    ///     d[n]['ptbL'] = ('ustr', f'Users/ann/Projects/p{i%7}/')
    ///     d[n]['ptbN'] = ('ustr', n if i%5 else f'orig {i}.txt')
    /// d['ünï 😀']['ptbL'] = ('ustr', 'Users/ann/')
    /// d['ünï 😀']['ptbN'] = ('ustr', 'ünï 😀')
    /// ```
    #[test]
    fn parse_reads_a_file_written_by_python_ds_store() {
        let img = include_bytes!("../tests/data/python-ds_store-trash.bin");
        let recs = parse(img).unwrap();
        assert_eq!(recs.len(), 52);
        let pb = put_back(&recs);
        assert_eq!(pb.len(), 26);
        for i in 0..25 {
            let n = format!("file{i:03}.txt");
            let want = PutBack {
                folder: Some(format!("Users/ann/Projects/p{}/", i % 7)),
                name: Some(if i % 5 == 0 {
                    format!("orig {i}.txt")
                } else {
                    n.clone()
                }),
            };
            assert_eq!(pb[&n], want, "{n}");
        }
        assert_eq!(pb["ünï 😀"].folder.as_deref(), Some("Users/ann/"));
        // B-tree order: names ascend.
        let names: Vec<&str> = recs.iter().map(|r| r.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_by_key(|n| n.to_lowercase());
        assert_eq!(names, sorted);
    }

    #[test]
    fn damaged_files_give_errors_not_panics() {
        let img = write(&[ustr("x", b"ptbL", "Users/x/"), ustr("x", b"ptbN", "x")]);
        for len in 0..img.len() {
            let _ = parse(&img[..len]);
        }
        let mut flipped = img.clone();
        for i in 0..flipped.len() {
            flipped[i] ^= 0xff;
            let _ = parse(&flipped);
            flipped[i] ^= 0xff;
        }
        assert!(parse(b"not a ds store at all, not at all....").is_err());

        // A node that names itself as its own child.
        let mut node = Vec::new();
        node.extend_from_slice(&2u32.to_be_bytes());
        node.extend_from_slice(&0u32.to_be_bytes());
        let img = assemble(&[dsdb_header(2, 1, 0, 1), node], 1);
        assert!(parse(&img).unwrap_err().contains("loops"));
    }
}
