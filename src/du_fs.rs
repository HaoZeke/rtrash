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

    pub fn is_symlink(&self) -> bool {
        self.mode & S_IFMT == S_IFLNK
    }

    /// Apparent size and allocated bytes (`st_blocks * 512`).
    pub fn sizes(&self) -> (u64, u64) {
        (self.size, self.blocks.saturating_mul(512))
    }
}

/// 35(a): the bytes on disk of the file at `path`, whose metadata is
/// `meta`, as the du walk counts them. On Unix that is its 512-byte
/// blocks. On Windows it is the disk use [`lstat`] gives, which knows
/// compressed, Compact, sparse and online-only files; when that stat
/// fails it is the length. Every size count outside the walk reads this
/// (precheck check_disk_reads), so reap, empty and the trash totals agree
/// with du.
#[cfg(unix)]
pub fn disk_bytes(_path: &std::path::Path, meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.blocks().saturating_mul(512)
}

/// See the Unix [`disk_bytes`].
#[cfg(windows)]
pub fn disk_bytes(path: &std::path::Path, meta: &std::fs::Metadata) -> u64 {
    lstat(path).map_or(meta.len(), |st| st.sizes().1)
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
    /// The inode number the listing gave each regular file (Linux
    /// `getdents64`), 0 for other entries; empty when it gave none.
    inos: Vec<u64>,
    /// 34(j): the entries whose listed stat may miss data that Compact
    /// packed (`fileid::hides_its_clusters`); [`Entries::restat`] asks
    /// them again.
    #[cfg(any(windows, test))]
    restat: Vec<usize>,
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

    /// Add an entry along with the inode number its listing gave, 0 for
    /// none.
    #[cfg(target_os = "linux")]
    pub(crate) fn push_with_ino(&mut self, name: &[u8], kind: Kind, ino: u64) {
        let before = self.index.len();
        self.push(name, kind);
        if self.index.len() > before {
            self.inos.resize(before, 0);
            self.inos.push(ino);
        }
    }

    /// 34(j): ask `stat` again about each entry the listing marked, and
    /// keep what it gives. When it gives None, the listed stat stays.
    #[cfg(any(windows, test))]
    fn restat(&mut self, mut stat: impl FnMut(&CStr) -> Option<Stat>) {
        for i in std::mem::take(&mut self.restat) {
            let got = stat(self.get(i).0);
            if let (Some(s), Some(slot)) = (got, self.stats.get_mut(i)) {
                *slot = Some(s);
            }
        }
    }

    /// Stat of entry `i` if the listing returned one.
    pub fn stat(&self, i: usize) -> Option<Stat> {
        self.stats.get(i).copied().flatten()
    }

    /// The inode number the listing gave entry `i`, a regular file. Only
    /// [`Dir::lists_true_inodes`] makes it the number its stat would give.
    pub fn file_ino(&self, i: usize) -> Option<u64> {
        self.inos.get(i).copied().filter(|&n| n != 0)
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
    pub const ATTR_CMN_ACCESSMASK: u32 = 0x0002_0000;
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
        | ATTR_CMN_ACCESSMASK
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

    // With FSOPT_PACK_INVAL_ATTRS every requested attribute has its slot
    // after the entry length and the returned set. The kernel packs
    // ATTR_CMN_ERROR first, then the other common attributes in bit order
    // (xnu bsd/vfs/vfs_attrlist.c, attr_pack_common). File attributes
    // follow only for entries that are not directories, so a directory's
    // record ends at COMMON_END. The fixed slots are packed back to back
    // with no alignment beyond 4 bytes (ATTR_PACK4 and ATTR_PACK8), so
    // 8-byte fields are read unaligned. The name's bytes live after the
    // fixed slots and their padding never moves a fixed field.
    const RETURNED: usize = 4;
    const ERROR: usize = RETURNED + 20;
    pub const NAME: usize = ERROR + 4;
    const DEVID: usize = NAME + 8;
    const OBJTYPE: usize = DEVID + 4;
    const MODTIME: usize = OBJTYPE + 4;
    const ACCESSMASK: usize = MODTIME + 16;
    const FILEID: usize = ACCESSMASK + 4;
    pub const COMMON_END: usize = FILEID + 8;
    const LINKCOUNT: usize = COMMON_END;
    const ALLOCSIZE: usize = LINKCOUNT + 4;
    const DATALENGTH: usize = ALLOCSIZE + 8;
    pub const FILE_END: usize = DATALENGTH + 8;

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
            if off + COMMON_END > buf.len() {
                return false;
            }
            let len = u32_at(buf, off) as usize;
            if len < COMMON_END || off + len > buf.len() {
                return false;
            }
            let rec = &buf[off..off + len];
            off += len;
            let common = u32_at(rec, RETURNED);
            let file = u32_at(rec, RETURNED + 12);
            // The name offset counts from its own attrreference slot, and
            // its length includes the NUL. The name sits right after the
            // fixed slots, so its offset also tells whether the record
            // carries the file attributes.
            let name_at = NAME as i64 + i64::from(u32_at(rec, NAME) as i32);
            let name_len = u32_at(rec, NAME + 4) as usize;
            let has_file = match usize::try_from(name_at) {
                Ok(FILE_END) => true,
                Ok(COMMON_END) => false,
                _ => return false,
            };
            if common & ATTR_CMN_NAME == 0 || name_at as usize + name_len > rec.len() {
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
            const STAT_COMMON: u32 =
                ATTR_CMN_DEVID | ATTR_CMN_MODTIME | ATTR_CMN_ACCESSMASK | ATTR_CMN_FILEID;
            let complete = common & STAT_COMMON == STAT_COMMON && has_file && file & FILE == FILE;
            // Directories get their stat from their own descriptor, so
            // only other entries keep the one from the listing.
            let stat = (kind == Kind::Other && complete).then(|| Stat {
                // The access mask is va_mode, which already carries the
                // type bits when the kernel has the vnode; take the type
                // from the object type either way.
                mode: mode_of(objtype) | (u32_at(rec, ACCESSMASK) & 0o7777),
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
    pub const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;
    pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    pub const FILE_ATTRIBUTE_COMPRESSED: u32 = 0x800;
    // winnt.h: a file that Compact packed (the Windows Overlay Filter).
    pub const IO_REPARSE_TAG_WOF: u32 = 0x8000_0017;
    // winnt.h: a junction or volume mount point, and a symlink.
    #[cfg(test)]
    pub const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
    #[cfg(test)]
    pub const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
    // winnt.h IsReparseTagNameSurrogate: the tag names another file or
    // folder, as a junction, a volume mount point, a symlink, a WSL symlink
    // or a container link does. Other tags (cloud placeholders,
    // deduplicated files) hold data and count as what they are.
    const NAME_SURROGATE: u32 = 0x2000_0000;

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

    /// A reparse point that names another file or folder: the walk counts
    /// it as a link and never enters it, so a junction or a volume mount
    /// point is not crossed (diskus #42, gdu #441). This is the test std's
    /// `FileType::is_symlink` makes, so the `--dupes` walk, reap and the
    /// Recycle Bin code agree with the scan.
    pub fn is_link(attrs: u32, tag: u32) -> bool {
        attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 && tag & NAME_SURROGATE != 0
    }

    /// gdu #407: a file whose data file compression or Compact packed.
    /// Its allocation size is not its disk use, so a scan asks the file
    /// itself: the listing gives no stat for it, and a stat by path reads
    /// GetCompressedFileSizeW. Every read of an allocation size checks
    /// this first (precheck check_compressed_sizes).
    pub fn needs_real_size(attrs: u32, tag: u32) -> bool {
        attrs & FILE_ATTRIBUTE_COMPRESSED != 0
            || (attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 && tag == IO_REPARSE_TAG_WOF)
    }

    /// NTFS can keep a file this short inside its file record, with no
    /// clusters of its own.
    pub const NO_CLUSTERS_MIN: i64 = 4096;

    /// 34(j): the Compact filter can hide the reparse point of a packed
    /// file from a listing. The listing then shows a plain file of `len`
    /// bytes with no clusters (`alloc` 0), while a stat by path gives the
    /// packed size. So a file past [`NO_CLUSTERS_MIN`] bytes with no
    /// clusters gets a stat as well (`Entries::restat`), and the listed
    /// value stays when that stat fails. A sparse file keeps its listing,
    /// since its clusters are its disk use. So does a reparse point, which
    /// [`needs_real_size`] or the stat by path already covers.
    pub fn hides_its_clusters(attrs: u32, alloc: i64, len: i64) -> bool {
        alloc == 0
            && len > NO_CLUSTERS_MIN
            && attrs & (FILE_ATTRIBUTE_SPARSE_FILE | FILE_ATTRIBUTE_REPARSE_POINT) == 0
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

    /// What a listing says of one entry ([`listed`]): attributes, EA size
    /// or reparse tag, allocation size and length.
    pub type Listed = (u32, u32, i64, i64);

    /// What the listing in `buf` says of the entry `name`, for the size
    /// report of 34(j). None when no record has that name or a record runs
    /// past `buf`.
    pub fn listed(buf: &[u8], name: &str) -> Option<Listed> {
        type I = Record;
        let name_at = offset_of!(I, file_name);
        let mut off = 0usize;
        loop {
            let rec = buf.get(off..)?;
            if rec.len() < name_at {
                return None;
            }
            let next = u32_at(rec, offset_of!(I, next_entry_offset)) as usize;
            let name_len = u32_at(rec, offset_of!(I, file_name_length)) as usize;
            let raw = rec.get(name_at..name_at.checked_add(name_len)?)?;
            if utf16_text(raw) == name {
                return Some((
                    u32_at(rec, offset_of!(I, file_attributes)),
                    u32_at(rec, offset_of!(I, ea_size)),
                    i64_at(rec, offset_of!(I, allocation_size)),
                    i64_at(rec, offset_of!(I, end_of_file)),
                ));
            }
            if next == 0 {
                return None;
            }
            off = off.checked_add(next)?;
        }
    }

    // FILE_STREAM_INFO: NextEntryOffset, StreamNameLength (bytes),
    // StreamSize, StreamAllocationSize, then the name. du_fs windows
    // checks these offsets at compile time.
    pub const STREAM_NEXT: usize = 0;
    pub const STREAM_NAME_LEN: usize = 4;
    pub const STREAM_ALLOC: usize = 16;
    pub const STREAM_NAME: usize = 24;

    /// The name and allocation size of each FILE_STREAM_INFO record in
    /// `buf`, for the size report of 34(j). None when a record runs past
    /// `buf`.
    pub fn stream_records(buf: &[u8]) -> Option<Vec<(String, i64)>> {
        let mut off = 0usize;
        let mut out = Vec::new();
        loop {
            let rec = buf.get(off..)?;
            if rec.len() < STREAM_NAME {
                return None;
            }
            let next = u32_at(rec, STREAM_NEXT) as usize;
            let name_len = u32_at(rec, STREAM_NAME_LEN) as usize;
            let name = rec.get(STREAM_NAME..STREAM_NAME.checked_add(name_len)?)?;
            out.push((utf16_text(name), i64_at(rec, STREAM_ALLOC)));
            if next == 0 {
                return Some(out);
            }
            off = off.checked_add(next)?;
        }
    }

    /// The UTF-16 bytes `raw` as text; a lone surrogate reads U+FFFD.
    fn utf16_text(raw: &[u8]) -> String {
        let units: Vec<u16> = raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_ne_bytes(*c))
            .collect();
        String::from_utf16_lossy(&units)
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
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_ne_bytes(*c))
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
            let mut ask = false;
            let (kind, stat) = if mode == S_IFDIR {
                (Kind::Dir, None)
            } else if needs_real_size(attrs, tag) {
                (Kind::Other, None)
            } else {
                let alloc = i64_at(rec, offset_of!(I, allocation_size));
                let len = i64_at(rec, offset_of!(I, end_of_file));
                ask = mode == S_IFREG && hides_its_clusters(attrs, alloc, len);
                let stat = Stat {
                    mode,
                    size: len.max(0) as u64,
                    blocks: (alloc.max(0) as u64).div_ceil(512),
                    // Unknown: the listing has no link count.
                    nlink: 0,
                    ino: i64_at(rec, offset_of!(I, file_id)) as u64,
                    dev,
                    mtime: mtime_of(i64_at(rec, offset_of!(I, last_write_time))),
                };
                (Kind::Other, Some(stat))
            };
            let before = out.len();
            out.push_with_stat(name.as_encoded_bytes(), kind, stat);
            if ask && out.len() > before {
                out.restat.push(before);
            }
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

    #[test]
    fn kernel_file_systems_are_told_apart() {
        let tmp = Dir::open(&std::env::temp_dir()).unwrap();
        assert!(!tmp.is_kernfs());
        // proc and sysfs are mounted on any Linux host CI runs on.
        #[cfg(target_os = "linux")]
        for p in ["/proc", "/sys"] {
            let d = Dir::open(std::path::Path::new(p)).unwrap();
            assert!(d.is_kernfs(), "{p}");
        }
    }

    /// The values one crafted bulk record carries.
    #[derive(Clone, Copy)]
    struct Rec<'a> {
        name: &'a str,
        objtype: u32,
        error: u32,
        file_valid: bool,
        access: u32,
        ino: u64,
        nlink: u32,
        alloc: u64,
        size: u64,
    }

    impl<'a> Rec<'a> {
        fn new(name: &'a str, objtype: u32) -> Self {
            Rec {
                name,
                objtype,
                error: 0,
                file_valid: true,
                // va_mode with the type bits, as the kernel fills it in.
                access: 0o100_644,
                ino: 4242,
                nlink: 2,
                alloc: 4097,
                size: 1234,
            }
        }
    }

    /// One packed `getattrlistbulk` record as xnu lays it out: the error
    /// slot right after the returned set, the common attributes in bit
    /// order, and file attributes only for entries that are not
    /// directories. The name's bytes follow the fixed slots, and the
    /// record is padded to 8 bytes.
    fn bulk_record(f: Rec<'_>) -> Vec<u8> {
        use bulk::*;
        let mut r = Vec::new();
        let common = COMMON & !ATTR_CMN_RETURNED_ATTRS;
        let push32 = |r: &mut Vec<u8>, v: u32| r.extend_from_slice(&v.to_ne_bytes());
        let push64 = |r: &mut Vec<u8>, v: u64| r.extend_from_slice(&v.to_ne_bytes());
        let dir = f.objtype == 2;
        push32(&mut r, 0); // length, patched below
        push32(&mut r, common);
        push32(&mut r, 0);
        push32(&mut r, 0);
        push32(&mut r, if f.file_valid && !dir { FILE } else { 0 });
        push32(&mut r, 0);
        push32(&mut r, f.error);
        let name_slot = r.len();
        push32(&mut r, 0); // offset, patched below
        push32(&mut r, f.name.len() as u32 + 1);
        push32(&mut r, (-3i32) as u32); // devid
        push32(&mut r, f.objtype);
        push64(&mut r, 1_700_000_000); // mtime seconds
        push64(&mut r, 5); // nanoseconds
        push32(&mut r, f.access);
        push64(&mut r, f.ino);
        assert_eq!(r.len(), COMMON_END);
        if !dir {
            push32(&mut r, f.nlink);
            push64(&mut r, f.alloc);
            push64(&mut r, f.size);
            assert_eq!(r.len(), FILE_END);
        }
        let at = r.len();
        r[name_slot..name_slot + 4].copy_from_slice(&((at - name_slot) as u32).to_ne_bytes());
        r.extend_from_slice(f.name.as_bytes());
        r.push(0);
        while !r.len().is_multiple_of(8) {
            r.push(0);
        }
        let len = r.len() as u32;
        r[..4].copy_from_slice(&len.to_ne_bytes());
        r
    }

    #[test]
    fn bulk_records_parse_into_entries_and_stats() {
        let mut buf = Vec::new();
        buf.extend(bulk_record(Rec::new("file.bin", 1)));
        buf.extend(bulk_record(Rec::new("sub", 2)));
        buf.extend(bulk_record(Rec {
            error: libc::EACCES as u32,
            ..Rec::new("gone", 1)
        }));
        buf.extend(bulk_record(Rec {
            access: 0o120_755,
            ..Rec::new("link", 5)
        }));
        let mut e = Entries::default();
        assert!(bulk::parse(&buf, 4, &mut e));
        assert_eq!(e.len(), 4);
        let (n, k) = e.get(0);
        assert_eq!((n.to_bytes(), k), (&b"file.bin"[..], Kind::Other));
        let s = e.stat(0).unwrap();
        assert!(s.is_file());
        assert_eq!(s.mode, 0o100_644, "type and permission bits");
        assert_eq!((s.size, s.blocks, s.nlink, s.ino), (1234, 9, 2, 4242));
        assert_eq!((s.dev, s.mtime), (u64::MAX - 2, 1_700_000_000));
        assert_eq!(e.get(1).1, Kind::Dir);
        assert_eq!(e.stat(1), None, "directories stat their own descriptor");
        assert_eq!(e.get(2).1, Kind::Unknown);
        assert_eq!(e.stat(2), None, "a failed entry gets a plain stat later");
        let l = e.stat(3).unwrap();
        assert!(!l.is_file() && !l.is_dir());
        assert_eq!(l.mode, 0o120_755);
        // A record cut short is refused rather than read past its end.
        let mut short = Entries::default();
        assert!(!bulk::parse(&buf[..buf.len() - 8], 4, &mut short));
        // So is a name that does not start where the fixed slots end.
        let mut odd = bulk_record(Rec::new("odd", 1));
        let slot = bulk::NAME;
        let off = u32::from_ne_bytes(odd[slot..slot + 4].try_into().unwrap()) + 4;
        odd[slot..slot + 4].copy_from_slice(&off.to_ne_bytes());
        assert!(!bulk::parse(&odd, 1, &mut Entries::default()));
    }

    /// Names of every length across the 4- and 8-byte padding steps,
    /// packed back to back in one fill, each with distinct values, so a
    /// record that shifted any field would show it.
    #[test]
    fn bulk_records_keep_every_field_across_name_lengths() {
        let names: Vec<String> = (1..=24)
            .chain([47, 48, 49, 255])
            .map(|n| "n".repeat(n))
            .collect();
        let recs: Vec<Rec<'_>> = names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let i = i as u64;
                Rec {
                    access: 0o100_000 | (0o600 + i as u32 % 0o100),
                    ino: 10_000 + i,
                    nlink: 1 + i as u32,
                    alloc: 4096 * (i + 1),
                    size: 1000 + i,
                    ..Rec::new(n, if i % 5 == 4 { 2 } else { 1 })
                }
            })
            .collect();
        let mut buf = Vec::new();
        for r in &recs {
            buf.extend(bulk_record(*r));
        }
        let mut e = Entries::default();
        assert!(bulk::parse(&buf, recs.len(), &mut e));
        assert_eq!(e.len(), recs.len());
        for (i, r) in recs.iter().enumerate() {
            let (name, kind) = e.get(i);
            assert_eq!(name.to_bytes(), r.name.as_bytes(), "record {i}");
            if r.objtype == 2 {
                assert_eq!((kind, e.stat(i)), (Kind::Dir, None), "record {i}");
                continue;
            }
            let s = e.stat(i).unwrap();
            assert_eq!(
                (s.mode, s.ino, s.nlink, s.blocks, s.size, s.mtime),
                (
                    r.access,
                    r.ino,
                    u64::from(r.nlink),
                    r.alloc / 512,
                    r.size,
                    1_700_000_000
                ),
                "record {i}, name length {}",
                r.name.len()
            );
        }
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
        while !r.len().is_multiple_of(8) {
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
        assert!(is_link(junction, IO_REPARSE_TAG_SYMLINK));
        let mut short = Entries::default();
        assert!(!parse(&buf[..buf.len() - 9], 9, &mut short));
    }

    /// gdu #407: a compressed file and a Compact one get no stat from the
    /// listing, so the walk stats them by path for their real disk use.
    #[test]
    fn compressed_files_leave_their_size_to_a_stat() {
        use fileid::*;
        let wof = 0x20 | FILE_ATTRIBUTE_REPARSE_POINT;
        let mut buf = Vec::new();
        buf.extend(fileid_record("plain.bin", 0x20, 0, false));
        buf.extend(fileid_record(
            "ntfs.bin",
            0x20 | FILE_ATTRIBUTE_COMPRESSED,
            0,
            false,
        ));
        buf.extend(fileid_record("wof.bin", wof, IO_REPARSE_TAG_WOF, false));
        // The tag counts only on a reparse point: EA size is a size else.
        buf.extend(fileid_record("ea.bin", 0x20, IO_REPARSE_TAG_WOF, true));
        let mut e = Entries::default();
        assert!(parse(&buf, 9, &mut e));
        assert_eq!(e.len(), 4);
        assert!(e.stat(0).is_some());
        assert_eq!(e.get(1).1, Kind::Other);
        assert_eq!(e.stat(1), None);
        assert_eq!(e.get(2).1, Kind::Other);
        assert_eq!(e.stat(2), None);
        assert!(e.stat(3).is_some());
        assert!(
            !is_link(wof, IO_REPARSE_TAG_WOF),
            "a Compact file holds data"
        );
    }

    /// 35(a): a size count outside the walk gives a file the disk use that
    /// du's own stat gives it.
    #[test]
    fn disk_bytes_match_the_stat_du_takes() {
        let dir = std::env::temp_dir().join(format!("rtrash-dufs-{}-disk", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("data.bin");
        std::fs::write(&f, [7u8; 64 * 1024]).unwrap();
        let meta = std::fs::symlink_metadata(&f).unwrap();
        let st = lstat(&f).unwrap();
        assert_eq!(disk_bytes(&f, &meta), st.sizes().1);
        assert!(disk_bytes(&f, &meta) > 0, "{st:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 34(j): the Compact filter may hide the reparse point of a packed
    /// file, so a listing shows a long file with no clusters. That file is
    /// asked again and takes the stat it gets; when the stat gives None it
    /// keeps the listed value. A sparse file, a reparse point and a file
    /// short enough for its file record are not asked.
    #[test]
    fn a_long_file_without_clusters_asks_a_stat_and_keeps_the_listing_on_none() {
        use fileid::*;
        use std::mem::offset_of;
        let sized = |name: &str, attrs: u32, len: i64, last: bool| {
            let mut r = fileid_record(name, attrs, 0, last);
            let a = offset_of!(Record, allocation_size);
            r[a..a + 8].copy_from_slice(&0i64.to_ne_bytes());
            let l = offset_of!(Record, end_of_file);
            r[l..l + 8].copy_from_slice(&len.to_ne_bytes());
            r
        };
        let sparse = 0x20 | FILE_ATTRIBUTE_SPARSE_FILE;
        let cloud = 0x20 | FILE_ATTRIBUTE_REPARSE_POINT;
        let mut buf = sized("wof.bin", 0x20, 1 << 20, false);
        buf.extend(sized("hollow.bin", sparse, 8 << 20, false));
        buf.extend(sized("cloud.bin", cloud, 1 << 20, false));
        buf.extend(sized("tiny.txt", 0x20, 600, true));
        let mut e = Entries::default();
        assert!(parse(&buf, 9, &mut e));
        let mut asked = Vec::new();
        e.restat(|name| {
            asked.push(name.to_string_lossy().into_owned());
            None
        });
        assert_eq!(asked, ["wof.bin"]);
        let kept = e.stat(0).unwrap();
        assert_eq!(kept.blocks, 0, "a stat that gives None keeps the listing");
        assert_eq!(kept.size, 1 << 20);
        let mut e = Entries::default();
        assert!(parse(&buf, 9, &mut e));
        let packed = Stat {
            blocks: 248,
            ..kept
        };
        e.restat(|_| Some(packed));
        assert_eq!(e.stat(0).unwrap().blocks, 248);
        assert_eq!(
            e.stat(1).unwrap().blocks,
            0,
            "a sparse file keeps its listing"
        );
        assert_eq!(
            e.stat(2).unwrap().blocks,
            0,
            "a reparse point keeps its listing"
        );
        assert_eq!(e.stat(3).unwrap().size, 600);
        assert!(!hides_its_clusters(0x20, 0, NO_CLUSTERS_MIN));
    }

    /// 34(j) size report: `listed` reads one entry back from a listing,
    /// and gives None for a missing name or a cut listing.
    #[test]
    fn a_listing_entry_reads_back_by_name() {
        use fileid::*;
        let mut buf = fileid_record("plain.bin", 0x20, 0, false);
        buf.extend(fileid_record("wof.bin", 0x420, IO_REPARSE_TAG_WOF, true));
        let got = listed(&buf, "wof.bin");
        assert_eq!(got, Some((0x420, IO_REPARSE_TAG_WOF, 4096, 1234)));
        assert_eq!(listed(&buf, "none.bin"), None);
        assert_eq!(listed(&buf[..buf.len() - 4], "wof.bin"), None);
    }

    /// One FILE_STREAM_INFO record, padded to 8 bytes.
    fn stream_record(name: &str, alloc: i64, last: bool) -> Vec<u8> {
        use fileid::*;
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut r = vec![0u8; STREAM_NAME + units.len() * 2];
        let len = (units.len() * 2) as u32;
        r[STREAM_NAME_LEN..STREAM_NAME_LEN + 4].copy_from_slice(&len.to_ne_bytes());
        r[STREAM_ALLOC..STREAM_ALLOC + 8].copy_from_slice(&alloc.to_ne_bytes());
        for (i, u) in units.iter().enumerate() {
            let at = STREAM_NAME + 2 * i;
            r[at..at + 2].copy_from_slice(&u.to_ne_bytes());
        }
        while !r.len().is_multiple_of(8) {
            r.push(0);
        }
        if !last {
            let n = r.len() as u32;
            r[STREAM_NEXT..STREAM_NEXT + 4].copy_from_slice(&n.to_ne_bytes());
        }
        r
    }

    /// 34(j) size report: `stream_records` gives each stream name with its
    /// allocation, and None for a cut list.
    #[test]
    fn stream_records_give_each_name_and_allocation() {
        use fileid::*;
        let mut buf = stream_record("::$DATA", 0, false);
        buf.extend(stream_record(":WofCompressedData:$DATA", 8192, true));
        let got = stream_records(&buf);
        let want = vec![
            ("::$DATA".to_string(), 0),
            (":WofCompressedData:$DATA".to_string(), 8192),
        ];
        assert_eq!(got, Some(want));
        assert_eq!(stream_records(&buf[..buf.len() - 10]), None);
        assert_eq!(stream_records(&[]), None);
    }

    /// Every reparse tag that names another file or folder is a link, the
    /// ones winnt.h lists and any other; a tag that holds data is not.
    #[test]
    fn reparse_points_that_name_another_folder_are_links() {
        use fileid::*;
        let dir = FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT;
        // Junction, symlink, WSL symlink, container link, an unknown one.
        for tag in [
            0xA000_0003,
            0xA000_000C,
            0xA000_001D,
            0xA000_0027,
            0x2000_00FF,
        ] {
            assert!(is_link(dir, tag), "{tag:#x}");
        }
        // Cloud placeholder, deduplicated file, NFS special file.
        for tag in [0x9000_001A, 0x8000_0013, 0x8000_0014] {
            assert!(!is_link(dir, tag), "{tag:#x}");
        }
        assert!(!is_link(FILE_ATTRIBUTE_DIRECTORY, 0xA000_0003));
    }

    /// gdu #688: a file whose clusters are not on disk (an online-only
    /// OneDrive file, or a sparse one as here) keeps its length as the
    /// apparent size and its allocated size, near 0, as disk use, both in
    /// the directory listing and in a stat of the file. Windows only.
    #[cfg(windows)]
    #[test]
    fn a_hollow_file_counts_its_clusters_not_its_length() {
        use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
        let dir = std::env::temp_dir().join(format!("rtrash-dufs-{}-hollow", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("hollow.bin");
        let file = std::fs::File::create(&f).unwrap();
        crate::du_dupes::fsctl(&file, FSCTL_SET_SPARSE, &[], &mut []).unwrap();
        file.set_len(8 << 20).unwrap();
        drop(file);
        let st = lstat(&f).unwrap();
        assert_eq!(st.size, 8 << 20);
        assert!(st.sizes().1 < 1 << 20, "{st:?}");
        let (e, failed) = Dir::open(&dir).unwrap().entries();
        assert!(!failed);
        let i = (0..e.len()).find(|&i| e.get(i).0.to_bytes() == b"hollow.bin");
        let listed = e.stat(i.expect("hollow.bin listed")).unwrap();
        assert_eq!(listed.size, st.size);
        assert_eq!(listed.sizes().1, st.sizes().1, "{listed:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 35(c): the stat the walk takes for each entry, the listing's or else
    /// one by name, matches a stat by path on file type, length and disk
    /// use. Each system's listing parser is in precheck LISTING_PARSERS,
    /// and this test covers them all: getdents on Linux, getattrlistbulk on
    /// macOS, FileIdBothDirectoryInfo on Windows. The tree holds a plain
    /// file, an empty one, a sparse one, two names of one file, a symlink
    /// and a folder, plus packed files: compact /c and /exe on Windows and
    /// an APFS clone on macOS. Every row is printed before the assert.
    #[test]
    fn the_walk_stat_of_each_entry_matches_a_stat_by_path() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("rtrash-dufs-{}-parity", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        // Text that packs well, so a packed file shows a smaller disk use.
        let words = ["alpha ", "beta ", "gamma ", "delta ", "kappa ", "omega "];
        let mut text = Vec::with_capacity(1 << 20);
        let mut x = 0x2545_f491_u32;
        while text.len() < 1 << 20 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            text.extend_from_slice(words[(x >> 16) as usize % words.len()].as_bytes());
        }
        fs::write(dir.join("plain.txt"), &text).unwrap();
        fs::write(dir.join("empty"), b"").unwrap();
        fs::hard_link(dir.join("plain.txt"), dir.join("again.txt")).unwrap();
        let sparse = fs::File::create(dir.join("sparse.bin")).unwrap();
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
            crate::du_dupes::fsctl(&sparse, FSCTL_SET_SPARSE, &[], &mut []).unwrap();
        }
        sparse.set_len(8 << 20).unwrap();
        drop(sparse);
        #[cfg(unix)]
        std::os::unix::fs::symlink("plain.txt", dir.join("link")).unwrap();
        // A symlink needs a privilege on Windows; the test goes on without.
        #[cfg(windows)]
        let _ = std::os::windows::fs::symlink_file("plain.txt", dir.join("link"));
        #[cfg(windows)]
        for (name, args) in [
            ("ntfs.txt", &["/c"][..]),
            ("wof.txt", &["/c", "/exe:lzx"][..]),
        ] {
            let f = dir.join(name);
            fs::write(&f, &text).unwrap();
            let out = std::process::Command::new("compact")
                .args(args)
                .arg(&f)
                .output();
            eprintln!(
                "parity: compact {args:?} {name}: {:?}",
                out.map(|o| o.status)
            );
        }
        #[cfg(target_os = "macos")]
        {
            let clone = std::process::Command::new("cp")
                .arg("-c")
                .arg(dir.join("plain.txt"))
                .arg(dir.join("clone.txt"))
                .status();
            eprintln!("parity: cp -c clone.txt: {clone:?}");
        }
        let d = Dir::open(&dir).unwrap();
        // On macOS read with getattrlistbulk itself, not whichever reader
        // the timing heuristic picks.
        #[cfg(target_os = "macos")]
        let (e, failed) = {
            use std::os::fd::AsRawFd;
            let f = fs::File::open(&dir).unwrap();
            bulk::read(f.as_raw_fd()).expect("getattrlistbulk read nothing from a temp folder")
        };
        #[cfg(not(target_os = "macos"))]
        let (e, failed) = d.entries();
        let mut listed = Vec::new();
        let mut differ = Vec::new();
        let mut from_listing = 0;
        for i in 0..e.len() {
            let (name, kind) = e.get(i);
            let what = os(name).to_string_lossy().into_owned();
            let by_path = lstat(&dir.join(os(name))).unwrap();
            listed.push(what.clone());
            if kind == Kind::Dir {
                eprintln!("parity: {what}: listed as a folder, stat {by_path:?}");
                if !by_path.is_dir() {
                    differ.push(format!("{what}: listed as a folder"));
                }
                continue;
            }
            let walk = match e.stat(i) {
                Some(s) => {
                    from_listing += 1;
                    s
                }
                None => d.stat_at(name).unwrap(),
            };
            eprintln!("parity: {what}: walk {walk:?}, by path {by_path:?}");
            let fields = [
                (
                    "type",
                    u64::from(walk.mode & S_IFMT),
                    u64::from(by_path.mode & S_IFMT),
                ),
                ("size", walk.size, by_path.size),
                ("disk", walk.sizes().1, by_path.sizes().1),
            ];
            for (field, w, p) in fields {
                if w != p {
                    differ.push(format!("{what}: {field} walk={w} by path={p}"));
                }
            }
        }
        listed.sort();
        eprintln!("parity: listed {listed:?}, {from_listing} stats from the listing");
        let _ = fs::remove_dir_all(&dir);
        assert!(!failed);
        assert_eq!(differ, Vec::<String>::new());
        for want in ["again.txt", "empty", "plain.txt", "sparse.bin", "sub"] {
            assert!(listed.iter().any(|n| n == want), "{want}");
        }
        // The listings of macOS and Windows carry stats; Linux has none.
        if cfg!(any(target_os = "macos", windows)) {
            assert!(from_listing >= 4, "{from_listing}");
        }
    }
}
