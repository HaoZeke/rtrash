//! Directory access for `rtrash du`: read a directory's entries in bulk
//! and stat each entry relative to its open directory, without building a
//! path per entry.
//!
//! On Linux the entries come from raw `getdents64` into a reused
//! per-thread buffer, which skips the `fstat` that `opendir` adds and the
//! small buffer of musl's `readdir`. On macOS `getattrlistbulk` returns
//! the names and the stat fields of many entries per call, and the walk
//! falls back to `readdir` when a file system lacks it or the calls turn
//! out to wait on I/O. Other Unix systems use `fdopendir` and `readdir`.
//! Entries without a stat from the listing get one from
//! `fstatat(dirfd, name, AT_SYMLINK_NOFOLLOW)`. On Windows one
//! `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)` call returns
//! sizes, attributes and file IDs for many entries.

use std::ffi::CStr;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

// `st_mode` file type bits, the same on every system that has them.
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
#[cfg_attr(not(windows), allow(dead_code))]
const S_IFLNK: u32 = 0o120000;

/// The fields of `stat` that du uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub mode: u32,
    pub size: u64,
    pub blocks: u64,
    pub nlink: u64,
    pub ino: u64,
    pub dev: u64,
    pub mtime: i64,
}

impl Stat {
    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }

    pub fn is_file(&self) -> bool {
        self.mode & S_IFMT == S_IFREG
    }

    /// Apparent size and allocated bytes (`st_blocks * 512`).
    pub fn sizes(&self) -> (u64, u64) {
        (self.size, self.blocks.saturating_mul(512))
    }
}

/// What the directory entry says about its type before any stat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Dir,
    /// Anything that is known not to be a directory.
    Other,
    Unknown,
}

/// The entries of one directory: names packed into one buffer, each
/// followed by a NUL so it can go straight to `fstatat`.
#[derive(Debug, Default)]
pub struct Entries {
    names: Vec<u8>,
    /// (start in `names`, length without the NUL, kind)
    index: Vec<(usize, usize, Kind)>,
    /// Stat of each entry when the listing returned one (macOS bulk
    /// reads); empty otherwise.
    stats: Vec<Option<Stat>>,
}

impl Entries {
    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    fn push(&mut self, name: &[u8], kind: Kind) {
        if name == b"." || name == b".." {
            return;
        }
        let start = self.names.len();
        self.names.extend_from_slice(name);
        self.names.push(0);
        self.index.push((start, name.len(), kind));
    }

    /// Add an entry along with the stat its listing returned.
    #[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
    fn push_with_stat(&mut self, name: &[u8], kind: Kind, stat: Option<Stat>) {
        let before = self.index.len();
        self.push(name, kind);
        if self.index.len() > before {
            self.stats.resize(before, None);
            self.stats.push(stat);
        }
    }

    /// Stat of entry `i` if the listing returned one.
    pub fn stat(&self, i: usize) -> Option<Stat> {
        self.stats.get(i).copied().flatten()
    }

    /// Name and kind of entry `i`.
    pub fn get(&self, i: usize) -> (&CStr, Kind) {
        let (start, len, kind) = self.index[i];
        // SAFETY: push stored the name followed by exactly one NUL, and a
        // directory entry name never contains a NUL byte.
        let c = unsafe { CStr::from_bytes_with_nul_unchecked(&self.names[start..=start + len]) };
        (c, kind)
    }
}

/// `getattrlistbulk(2)` on macOS. The parser is plain byte handling, so it
/// also builds on other systems for its tests.
#[cfg(any(target_os = "macos", test))]
mod bulk {
    use super::{Entries, Kind, Stat};

    // sys/attr.h
    pub const ATTR_CMN_NAME: u32 = 0x0000_0001;
    pub const ATTR_CMN_DEVID: u32 = 0x0000_0002;
    pub const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
    pub const ATTR_CMN_MODTIME: u32 = 0x0000_0400;
    pub const ATTR_CMN_FILEID: u32 = 0x0200_0000;
    pub const ATTR_CMN_ERROR: u32 = 0x2000_0000;
    pub const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;
    pub const ATTR_FILE_LINKCOUNT: u32 = 0x0000_0001;
    pub const ATTR_FILE_ALLOCSIZE: u32 = 0x0000_0004;
    pub const ATTR_FILE_DATALENGTH: u32 = 0x0000_0200;

    pub const COMMON: u32 = ATTR_CMN_RETURNED_ATTRS
        | ATTR_CMN_NAME
        | ATTR_CMN_DEVID
        | ATTR_CMN_OBJTYPE
        | ATTR_CMN_MODTIME
        | ATTR_CMN_FILEID
        | ATTR_CMN_ERROR;
    pub const FILE: u32 = ATTR_FILE_LINKCOUNT | ATTR_FILE_ALLOCSIZE | ATTR_FILE_DATALENGTH;

    // sys/vnode.h `enum vtype`, and the matching `st_mode` file types.
    const VREG: u32 = 1;
    const VDIR: u32 = 2;
    const VBLK: u32 = 3;
    const VCHR: u32 = 4;
    const VLNK: u32 = 5;
    const VSOCK: u32 = 6;
    const VFIFO: u32 = 7;

    fn mode_of(objtype: u32) -> u32 {
        match objtype {
            VREG => 0o100000,
            VDIR => 0o040000,
            VBLK => 0o060000,
            VCHR => 0o020000,
            VLNK => 0o120000,
            VSOCK => 0o140000,
            VFIFO => 0o010000,
            _ => 0,
        }
    }

    // With FSOPT_PACK_INVAL_ATTRS every requested attribute has its slot,
    // in bit order within each group, after the entry length and the
    // returned set. Slots are 4-byte aligned, so 8-byte fields are read
    // unaligned.
    const RETURNED: usize = 4;
    const NAME: usize = RETURNED + 20;
    const DEVID: usize = NAME + 8;
    const OBJTYPE: usize = DEVID + 4;
    const MODTIME: usize = OBJTYPE + 4;
    const FILEID: usize = MODTIME + 16;
    const ERROR: usize = FILEID + 8;
    const LINKCOUNT: usize = ERROR + 4;
    const ALLOCSIZE: usize = LINKCOUNT + 4;
    const DATALENGTH: usize = ALLOCSIZE + 8;
    const FIXED: usize = DATALENGTH + 8;

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
    }

    fn u64_at(b: &[u8], at: usize) -> u64 {
        u64::from_ne_bytes(b[at..at + 8].try_into().unwrap())
    }

    /// Parse `count` records from one `getattrlistbulk` fill. Returns
    /// false if the buffer does not hold what the call promised.
    pub fn parse(buf: &[u8], count: usize, out: &mut Entries) -> bool {
        let mut off = 0usize;
        for _ in 0..count {
            if off + FIXED > buf.len() {
                return false;
            }
            let len = u32_at(buf, off) as usize;
            if len < FIXED || off + len > buf.len() {
                return false;
            }
            let rec = &buf[off..off + len];
            off += len;
            let common = u32_at(rec, RETURNED);
            let file = u32_at(rec, RETURNED + 12);
            // The name offset counts from its own attrreference slot, and
            // its length includes the NUL.
            let name_at = NAME as i64 + i64::from(u32_at(rec, NAME) as i32);
            let name_len = u32_at(rec, NAME + 4) as usize;
            if common & ATTR_CMN_NAME == 0 || name_at < 0 || name_at as usize + name_len > rec.len()
            {
                return false;
            }
            let raw = &rec[name_at as usize..name_at as usize + name_len];
            let name = raw.split(|b| *b == 0).next().unwrap_or(raw);
            let objtype = u32_at(rec, OBJTYPE);
            let failed = common & ATTR_CMN_ERROR != 0 && u32_at(rec, ERROR) != 0;
            let typed = common & ATTR_CMN_OBJTYPE != 0;
            let kind = match objtype {
                _ if failed || !typed => Kind::Unknown,
                VDIR => Kind::Dir,
                _ => Kind::Other,
            };
            let complete = common & (ATTR_CMN_DEVID | ATTR_CMN_MODTIME | ATTR_CMN_FILEID)
                == ATTR_CMN_DEVID | ATTR_CMN_MODTIME | ATTR_CMN_FILEID
                && file & FILE == FILE;
            // Directories get their stat from their own descriptor, so
            // only other entries keep the one from the listing.
            let stat = (kind == Kind::Other && complete).then(|| Stat {
                mode: mode_of(objtype),
                size: u64_at(rec, DATALENGTH),
                // stat(2) reports the total allocation in 512-byte blocks.
                blocks: u64_at(rec, ALLOCSIZE).div_ceil(512),
                nlink: u64::from(u32_at(rec, LINKCOUNT)),
                ino: u64_at(rec, FILEID),
                // dev_t is an i32 there; widen it the way `st_dev as u64`
                // does so it compares equal to lstat's.
                dev: u32_at(rec, DEVID) as i32 as u64,
                mtime: u64_at(rec, MODTIME) as i64,
            });
            out.push_with_stat(name, kind, stat);
        }
        true
    }

    #[cfg(target_os = "macos")]
    pub use imp::read;

    #[cfg(target_os = "macos")]
    mod imp {
        use super::*;
        use std::cell::RefCell;
        use std::os::fd::RawFd;
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use std::sync::OnceLock;
        use std::time::Instant;

        const ATTR_BIT_MAP_COUNT: u16 = 5;
        const FSOPT_PACK_INVAL_ATTRS: u64 = 0x0000_0008;
        const BUF: usize = 64 * 1024;
        /// Entries timed before deciding whether the bulk reads wait on
        /// I/O.
        const SAMPLE: u64 = 4096;
        /// Above this many nanoseconds per entry the reads are waiting on
        /// the disk. A warm cache takes about one microsecond, and
        /// `readdir` with parallel stats keeps more requests in flight.
        const SLOW_NS_PER_ENTRY: u64 = 20_000;

        static OFF: AtomicBool = AtomicBool::new(false);
        static TIMED_ENTRIES: AtomicU64 = AtomicU64::new(0);
        static TIMED_NS: AtomicU64 = AtomicU64::new(0);

        /// `RTRASH_DU_BULK=0` turns the bulk reads off and `1` keeps them
        /// on regardless of timing; anything else lets the timing decide.
        fn setting() -> Option<bool> {
            static S: OnceLock<Option<bool>> = OnceLock::new();
            *S.get_or_init(|| match std::env::var("RTRASH_DU_BULK").as_deref() {
                Ok("0") => Some(false),
                Ok("1") => Some(true),
                _ => None,
            })
        }

        fn record(entries: u64, ns: u64) {
            if setting().is_some() {
                return;
            }
            let n = TIMED_ENTRIES.fetch_add(entries, Ordering::Relaxed) + entries;
            let t = TIMED_NS.fetch_add(ns, Ordering::Relaxed) + ns;
            if n >= SAMPLE && t / n > SLOW_NS_PER_ENTRY {
                OFF.store(true, Ordering::Relaxed);
            }
        }

        /// Read a directory with `getattrlistbulk`. `None` means the
        /// caller should use `readdir`: the bulk reads are off, the file
        /// system does not support them, or they failed before returning
        /// anything.
        pub fn read(fd: RawFd) -> Option<(Entries, bool)> {
            if setting() == Some(false) || (setting().is_none() && OFF.load(Ordering::Relaxed)) {
                return None;
            }
            thread_local! {
                static BUFFER: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
            }
            BUFFER.with(|b| {
                let mut buf = b.borrow_mut();
                if buf.len() < BUF {
                    buf.resize(BUF, 0);
                }
                let mut list = libc::attrlist {
                    bitmapcount: ATTR_BIT_MAP_COUNT,
                    reserved: 0,
                    commonattr: COMMON,
                    volattr: 0,
                    dirattr: 0,
                    fileattr: FILE,
                    forkattr: 0,
                };
                let mut out = Entries::default();
                loop {
                    let start = Instant::now();
                    // SAFETY: `list` and the buffer outlive the call, and
                    // the buffer is writable for its full length.
                    let n = unsafe {
                        libc::getattrlistbulk(
                            fd,
                            &mut list as *mut libc::attrlist as *mut libc::c_void,
                            buf.as_mut_ptr() as *mut libc::c_void,
                            buf.len(),
                            FSOPT_PACK_INVAL_ATTRS,
                        )
                    };
                    if n < 0 {
                        let e = std::io::Error::last_os_error();
                        if e.kind() == std::io::ErrorKind::Interrupted {
                            continue;
                        }
                        // Nothing consumed yet: let readdir try instead.
                        return if out.is_empty() {
                            None
                        } else {
                            Some((out, true))
                        };
                    }
                    if n == 0 {
                        return Some((out, false));
                    }
                    record(n as u64, start.elapsed().as_nanos() as u64);
                    if !parse(&buf, n as usize, &mut out) {
                        return Some((out, true));
                    }
                }
            })
        }
    }
}

/// `FILE_ID_BOTH_DIR_INFO` records from a Windows directory listing. The
/// parser is plain byte handling, so it also builds elsewhere for tests.
#[cfg(any(windows, test))]
mod fileid {
    use super::{Entries, Kind, Stat, S_IFDIR, S_IFLNK, S_IFREG};
    use std::mem::offset_of;

    pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    // winnt.h: the reparse points that work like symlinks. Other tags (cloud
    // placeholders, deduplicated files) hold data and count as what they are.
    pub const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
    pub const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

    /// Same layout as `FILE_ID_BOTH_DIR_INFO` in winbase.h.
    #[repr(C)]
    #[allow(dead_code)]
    pub struct Record {
        pub next_entry_offset: u32,
        pub file_index: u32,
        pub creation_time: i64,
        pub last_access_time: i64,
        pub last_write_time: i64,
        pub change_time: i64,
        pub end_of_file: i64,
        pub allocation_size: i64,
        pub file_attributes: u32,
        pub file_name_length: u32,
        pub ea_size: u32,
        pub short_name_length: i8,
        pub short_name: [u16; 12],
        pub file_id: i64,
        pub file_name: [u16; 1],
    }

    pub fn is_link(attrs: u32, tag: u32) -> bool {
        attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0
            && (tag == IO_REPARSE_TAG_SYMLINK || tag == IO_REPARSE_TAG_MOUNT_POINT)
    }

    pub fn mode_of(attrs: u32, tag: u32) -> u32 {
        if is_link(attrs, tag) {
            S_IFLNK
        } else if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
            S_IFDIR
        } else {
            S_IFREG
        }
    }

    pub fn mtime_of(filetime: i64) -> i64 {
        crate::win_recycle::filetime_to_unix_epoch(filetime as u64)
    }

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
    }

    fn i64_at(b: &[u8], at: usize) -> i64 {
        i64::from_ne_bytes(b[at..at + 8].try_into().unwrap())
    }

    /// Parse the `FILE_ID_BOTH_DIR_INFO` records of one fill. Returns false
    /// when a record runs past the buffer.
    pub fn parse(buf: &[u8], dev: u64, out: &mut Entries) -> bool {
        type I = Record;
        let name_at = offset_of!(I, file_name);
        let mut off = 0usize;
        loop {
            if off + name_at > buf.len() {
                return false;
            }
            let rec = &buf[off..];
            let next = u32_at(rec, offset_of!(I, next_entry_offset)) as usize;
            let name_len = u32_at(rec, offset_of!(I, file_name_length)) as usize;
            if name_at + name_len > rec.len() {
                return false;
            }
            let units: Vec<u16> = rec[name_at..name_at + name_len]
                .chunks_exact(2)
                .map(|c| u16::from_ne_bytes([c[0], c[1]]))
                .collect();
            #[cfg(windows)]
            let name = {
                use std::os::windows::ffi::OsStringExt;
                std::ffi::OsString::from_wide(&units)
            };
            #[cfg(not(windows))]
            let name = std::ffi::OsString::from(String::from_utf16_lossy(&units));
            let attrs = u32_at(rec, offset_of!(I, file_attributes));
            // For a reparse point the EA size field holds the reparse tag.
            let tag = u32_at(rec, offset_of!(I, ea_size));
            let mode = mode_of(attrs, tag);
            let (kind, stat) = if mode == S_IFDIR {
                (Kind::Dir, None)
            } else {
                let stat = Stat {
                    mode,
                    size: i64_at(rec, offset_of!(I, end_of_file)).max(0) as u64,
                    blocks: (i64_at(rec, offset_of!(I, allocation_size)).max(0) as u64)
                        .div_ceil(512),
                    // Unknown: the listing has no link count.
                    nlink: 0,
                    ino: i64_at(rec, offset_of!(I, file_id)) as u64,
                    dev,
                    mtime: mtime_of(i64_at(rec, offset_of!(I, last_write_time))),
                };
                (Kind::Other, Some(stat))
            };
            out.push_with_stat(name.as_encoded_bytes(), kind, stat);
            if next == 0 {
                return true;
            }
            off += next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One packed `getattrlistbulk` record as macOS lays it out.
    fn bulk_record(name: &str, objtype: u32, error: u32, file_valid: bool) -> Vec<u8> {
        use bulk::*;
        let mut r = Vec::new();
        let common = COMMON & !ATTR_CMN_RETURNED_ATTRS;
        let push32 = |r: &mut Vec<u8>, v: u32| r.extend_from_slice(&v.to_ne_bytes());
        let push64 = |r: &mut Vec<u8>, v: u64| r.extend_from_slice(&v.to_ne_bytes());
        push32(&mut r, 0); // length, patched below
        push32(&mut r, common);
        push32(&mut r, 0);
        push32(&mut r, 0);
        push32(&mut r, if file_valid { FILE } else { 0 });
        push32(&mut r, 0);
        let name_slot = r.len();
        push32(&mut r, 0); // offset, patched below
        push32(&mut r, name.len() as u32 + 1);
        push32(&mut r, (-3i32) as u32); // devid
        push32(&mut r, objtype);
        push64(&mut r, 1_700_000_000); // mtime seconds
        push64(&mut r, 5); // nanoseconds
        push64(&mut r, 4242); // fileid
        push32(&mut r, error);
        push32(&mut r, 2); // link count
        push64(&mut r, 4097); // allocation
        push64(&mut r, 1234); // data length
        let at = r.len();
        r[name_slot..name_slot + 4].copy_from_slice(&((at - name_slot) as u32).to_ne_bytes());
        r.extend_from_slice(name.as_bytes());
        r.push(0);
        while r.len() % 4 != 0 {
            r.push(0);
        }
        let len = r.len() as u32;
        r[..4].copy_from_slice(&len.to_ne_bytes());
        r
    }

    #[test]
    fn bulk_records_parse_into_entries_and_stats() {
        let mut buf = Vec::new();
        buf.extend(bulk_record("file.bin", 1, 0, true));
        buf.extend(bulk_record("sub", 2, 0, false));
        buf.extend(bulk_record("gone", 1, libc::EACCES as u32, true));
        buf.extend(bulk_record("link", 5, 0, true));
        let mut e = Entries::default();
        assert!(bulk::parse(&buf, 4, &mut e));
        assert_eq!(e.len(), 4);
        let (n, k) = e.get(0);
        assert_eq!((n.to_bytes(), k), (&b"file.bin"[..], Kind::Other));
        let s = e.stat(0).unwrap();
        assert!(s.is_file());
        assert_eq!((s.size, s.blocks, s.nlink, s.ino), (1234, 9, 2, 4242));
        assert_eq!((s.dev, s.mtime), (u64::MAX - 2, 1_700_000_000));
        assert_eq!(e.get(1).1, Kind::Dir);
        assert_eq!(e.stat(1), None, "directories stat their own descriptor");
        assert_eq!(e.get(2).1, Kind::Unknown);
        assert_eq!(e.stat(2), None, "a failed entry gets a plain stat later");
        let l = e.stat(3).unwrap();
        assert!(!l.is_file() && !l.is_dir());
        // A record cut short is refused rather than read past its end.
        let mut short = Entries::default();
        assert!(!bulk::parse(&buf[..buf.len() - 8], 4, &mut short));
    }

    /// One `FILE_ID_BOTH_DIR_INFO` record, padded to 8 bytes.
    fn fileid_record(name: &str, attrs: u32, ea: u32, last: bool) -> Vec<u8> {
        use fileid::Record;
        use std::mem::offset_of;
        let units: Vec<u16> = name.encode_utf16().collect();
        let at = offset_of!(Record, file_name);
        let mut r = vec![0u8; at + units.len() * 2];
        let put = |r: &mut Vec<u8>, off: usize, v: &[u8]| r[off..off + v.len()].copy_from_slice(v);
        put(
            &mut r,
            offset_of!(Record, last_write_time),
            &(116_444_736_000_000_000i64 + 10_000_000 * 60).to_ne_bytes(),
        );
        put(
            &mut r,
            offset_of!(Record, end_of_file),
            &1234i64.to_ne_bytes(),
        );
        put(
            &mut r,
            offset_of!(Record, allocation_size),
            &4096i64.to_ne_bytes(),
        );
        put(
            &mut r,
            offset_of!(Record, file_attributes),
            &attrs.to_ne_bytes(),
        );
        put(
            &mut r,
            offset_of!(Record, file_name_length),
            &((units.len() * 2) as u32).to_ne_bytes(),
        );
        put(&mut r, offset_of!(Record, ea_size), &ea.to_ne_bytes());
        put(&mut r, offset_of!(Record, file_id), &77i64.to_ne_bytes());
        for (i, u) in units.iter().enumerate() {
            put(&mut r, at + 2 * i, &u.to_ne_bytes());
        }
        while r.len() % 8 != 0 {
            r.push(0);
        }
        if !last {
            let n = r.len() as u32;
            put(
                &mut r,
                offset_of!(Record, next_entry_offset),
                &n.to_ne_bytes(),
            );
        }
        r
    }

    #[test]
    fn fileid_records_parse_into_entries_and_stats() {
        use fileid::*;
        let mut buf = Vec::new();
        buf.extend(fileid_record(".", FILE_ATTRIBUTE_DIRECTORY, 0, false));
        buf.extend(fileid_record("data.bin", 0x20, 0, false));
        buf.extend(fileid_record("sub", FILE_ATTRIBUTE_DIRECTORY, 0, false));
        let junction = FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT;
        buf.extend(fileid_record(
            "junction",
            junction,
            IO_REPARSE_TAG_MOUNT_POINT,
            false,
        ));
        // A cloud placeholder is a reparse point that holds data.
        let cloud = 0x20 | FILE_ATTRIBUTE_REPARSE_POINT;
        buf.extend(fileid_record("cloud.txt", cloud, 0x9000_001A, false));
        buf.extend(fileid_record("\u{e9}t\u{e9}", 0x20, 0, true));
        let mut e = Entries::default();
        assert!(parse(&buf, 9, &mut e));
        let names: Vec<String> = (0..e.len())
            .map(|i| e.get(i).0.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            ["data.bin", "sub", "junction", "cloud.txt", "\u{e9}t\u{e9}"]
        );
        let f = e.stat(0).unwrap();
        assert!(f.is_file());
        assert_eq!(
            (f.size, f.blocks, f.nlink, f.ino, f.dev, f.mtime),
            (1234, 8, 0, 77, 9, 60)
        );
        assert_eq!((e.get(1).1, e.stat(1)), (Kind::Dir, None));
        let j = e.stat(2).unwrap();
        assert_eq!(e.get(2).1, Kind::Other, "junctions are not followed");
        assert!(!j.is_dir() && !j.is_file());
        assert!(e.stat(3).unwrap().is_file());
        let mut short = Entries::default();
        assert!(!parse(&buf[..buf.len() - 9], 9, &mut short));
    }
}
