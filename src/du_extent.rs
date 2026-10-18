//! Shared-extent-aware sizes for `du --unique`.
//!
//! On Linux, `FS_IOC_FIEMAP` maps each file's extents, and btrfs and
//! the XFS family flag the extents that reflinks or snapshots share
//! (`FIEMAP_EXTENT_SHARED`). A file's unique size is the part in extents
//! no other file uses: roughly what deleting it would free. On macOS, APFS
//! reports the same figure for clones as the private size of a file
//! (`ATTR_CMNEXT_PRIVATESIZE`). Elsewhere, and on file systems that share
//! nothing, the unique size equals the disk size.
//!
//! The shared extents met during a scan are kept by physical range, so
//! the report can say how much space they take when each counts once.
//!
//! Compressed extents on btrfs (`FIEMAP_EXTENT_ENCODED`) map at their
//! uncompressed length, from where their compressed data starts, so their
//! ranges overlap the extents after them. The kernel gives no compressed
//! length without root, so such an extent counts once by where it
//! starts, at the length it maps: the size `st_blocks` counts too.
//!
//! Mapping costs a syscall or more per file, so a [`Probe`] maps only
//! files of at least [`PROBE_MIN`] bytes on disk (`RTRASH_EXTENT_MIN`
//! overrides it) on file systems that can share extents: btrfs, XFS,
//! bcachefs and OCFS2 on Linux, APFS on macOS. Every other file counts
//! as unique, which smaller files on those file systems seldom are not.

use std::collections::HashMap;

/// One mapped extent of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub physical: u64,
    pub length: u64,
    pub shared: bool,
    /// Compressed or otherwise encoded: `length` is not its size on disk.
    pub encoded: bool,
}

impl Extent {
    /// Where the extent sits on device `dev`.
    pub fn span(&self, dev: u64) -> Span {
        Span {
            dev,
            encoded: self.encoded,
            start: self.physical,
            end: self.physical.saturating_add(self.length),
        }
    }
}

/// Disk bytes one or more files point at, half-open `[start, end)` on
/// device `dev`. Spans of encoded extents join only when they start at
/// the same place: their ends are not where their data ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Span {
    pub dev: u64,
    pub encoded: bool,
    pub start: u64,
    pub end: u64,
}

impl Span {
    /// Whether `next`, sorted after `self`, covers bytes of it.
    fn joins(&self, next: &Span) -> bool {
        self.dev == next.dev
            && self.encoded == next.encoded
            && if next.encoded {
                next.start == self.start
            } else {
                next.start <= self.end
            }
    }
}

/// Bytes in extents only this file uses, and the shared extents, from a
/// file's extent map. `disk` (allocated bytes) caps the unique part: an
/// extent map can describe compressed data at its uncompressed length.
pub fn split(extents: &[Extent], disk: u64) -> (u64, Vec<Extent>) {
    let mut unique = 0u64;
    let mut shared = Vec::new();
    for e in extents {
        if e.shared {
            shared.push(*e);
        } else {
            unique = unique.saturating_add(e.length);
        }
    }
    (unique.min(disk), shared)
}

/// Physical ranges of shared extents, per device, for counting each byte
/// once however many files point at it.
#[derive(Debug, Default)]
pub struct SharedSet {
    spans: Vec<Span>,
    /// Bytes of shared extents as the files reference them, with repeats.
    pub referenced: u64,
    /// Bytes the mapped files have in encoded extents, at the length they
    /// map: the uncompressed size.
    pub encoded: u64,
}

impl SharedSet {
    pub fn add(&mut self, dev: u64, extents: &[Extent]) {
        for e in extents {
            self.referenced = self.referenced.saturating_add(e.length);
            self.spans.push(e.span(dev));
        }
    }

    pub fn merge(&mut self, other: SharedSet) {
        self.referenced = self.referenced.saturating_add(other.referenced);
        self.encoded = self.encoded.saturating_add(other.encoded);
        self.spans.extend(other.spans);
    }

    /// Bytes the shared extents take on disk, each extent counted once
    /// (compressed ones at their mapped length).
    pub fn distinct(&self) -> u64 {
        coalesce(&mut self.spans.clone())
    }
}

/// Merge `spans` in place: sorted, with the ones that overlap or touch
/// joined (encoded ones only when they start at the same place). Returns
/// the bytes they cover, each counted once.
pub fn coalesce(spans: &mut Vec<Span>) -> u64 {
    spans.retain(|s| s.end > s.start);
    spans.sort_unstable();
    let mut out: Vec<Span> = Vec::with_capacity(spans.len());
    for s in spans.iter() {
        match out.last_mut() {
            Some(last) if last.joins(s) => last.end = last.end.max(s.end),
            _ => out.push(*s),
        }
    }
    *spans = out;
    crate::du::size_sum(spans.iter().map(|s| s.end - s.start))
}

/// What one file adds to a `--unique` scan.
#[derive(Debug, Default)]
pub struct FileShare {
    pub unique: u64,
    pub shared: Vec<Extent>,
    /// Bytes in encoded extents, at their mapped (uncompressed) length.
    pub encoded: u64,
}

/// Unique size of an open regular file whose allocated size is `disk`.
/// Without an extent map (unsupported file system, platform or error),
/// the whole file counts as unique.
#[cfg(unix)]
pub fn file_share(file: &std::fs::File, disk: u64) -> FileShare {
    match file_extents(file, disk) {
        Ok(Mapped::Extents(ext)) => {
            let (unique, shared) = split(&ext, disk);
            let encoded = crate::du::size_sum(ext.iter().filter(|e| e.encoded).map(|e| e.length));
            FileShare {
                unique,
                shared,
                encoded,
            }
        }
        Ok(Mapped::Private(p)) => FileShare {
            unique: p.min(disk),
            shared: Vec::new(),
            encoded: 0,
        },
        Err(_) => FileShare {
            unique: disk,
            shared: Vec::new(),
            encoded: 0,
        },
    }
}

/// Files with fewer bytes on disk than this are not mapped.
pub const PROBE_MIN: u64 = 64 * 1024;

/// [`PROBE_MIN`], or `RTRASH_EXTENT_MIN` when it holds a size.
pub fn probe_min() -> u64 {
    static MIN: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MIN.get_or_init(|| {
        std::env::var("RTRASH_EXTENT_MIN")
            .ok()
            .and_then(|v| crate::filters::parse_size(&v))
            .unwrap_or(PROBE_MIN)
    })
}

/// Maps files only where it can pay: above [`probe_min`] and on file
/// systems that share extents, asked once per device.
#[derive(Debug, Default)]
pub struct Probe {
    shares: std::sync::Mutex<HashMap<u64, bool>>,
}

impl Probe {
    /// The open file, when one of `disk` bytes on `dev` is worth
    /// mapping; `open` runs only then.
    pub fn wants(
        &self,
        dev: u64,
        disk: u64,
        open: impl FnOnce() -> Option<std::fs::File>,
    ) -> Option<std::fs::File> {
        if disk < probe_min() {
            return None;
        }
        let known = self.shares.lock().ok().and_then(|m| m.get(&dev).copied());
        if known == Some(false) {
            return None;
        }
        let f = open()?;
        if known.is_none() {
            let may = fs_may_share(&f);
            if let Ok(mut m) = self.shares.lock() {
                m.insert(dev, may);
            }
            if !may {
                return None;
            }
        }
        Some(f)
    }
}

/// Whether the file system holding `file` can share extents between
/// files. An error says yes, leaving the answer to the extent map.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[allow(clippy::unnecessary_cast)]
pub fn fs_may_share(file: &std::fs::File) -> bool {
    use std::os::fd::AsRawFd;
    const BTRFS: i64 = 0x9123_683E;
    const XFS: i64 = 0x5846_5342;
    const BCACHEFS: i64 = 0xCA45_1A4E;
    const OCFS2: i64 = 0x7461_636F;
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: the fd is open and `buf` is a plain C struct.
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut buf) } != 0 {
        return true;
    }
    // f_type is signed on some targets; compare its low 32 bits.
    matches!(
        buf.f_type as i64 & 0xFFFF_FFFF,
        BTRFS | XFS | BCACHEFS | OCFS2
    )
}

/// Whether the file system holding `file` is APFS, which clones files.
#[cfg(target_os = "macos")]
pub fn fs_may_share(file: &std::fs::File) -> bool {
    use std::os::fd::AsRawFd;
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: the fd is open and `buf` is a plain C struct.
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut buf) } != 0 {
        return true;
    }
    // SAFETY: f_fstypename is a NUL-terminated name.
    let name = unsafe { std::ffi::CStr::from_ptr(buf.f_fstypename.as_ptr()) };
    name.to_bytes() == b"apfs"
}

/// No other platform maps extents.
#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
pub fn fs_may_share(_file: &std::fs::File) -> bool {
    false
}

#[cfg(unix)]
// Each platform builds only the variant its mapper returns.
#[allow(dead_code)]
enum Mapped {
    Extents(Vec<Extent>),
    /// Bytes no clone shares, as APFS reports them.
    Private(u64),
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod fiemap {
    //! The `FS_IOC_FIEMAP` ioctl, from `linux/fiemap.h`.

    pub const FS_IOC_FIEMAP: libc::c_ulong = 0xC020_660B;
    pub const FIEMAP_EXTENT_LAST: u32 = 0x1;
    pub const FIEMAP_EXTENT_ENCODED: u32 = 0x8;
    pub const FIEMAP_EXTENT_SHARED: u32 = 0x2000;
    /// Extents asked for per call.
    pub const BATCH: usize = 256;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct Extent {
        pub logical: u64,
        pub physical: u64,
        pub length: u64,
        pub reserved64: [u64; 2],
        pub flags: u32,
        pub reserved: [u32; 3],
    }

    #[repr(C)]
    pub struct Map {
        pub start: u64,
        pub length: u64,
        pub flags: u32,
        pub mapped_extents: u32,
        pub extent_count: u32,
        pub reserved: u32,
        pub extents: [Extent; BATCH],
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn file_extents(file: &std::fs::File, _disk: u64) -> std::io::Result<Mapped> {
    use std::os::fd::AsRawFd;
    let mut out = Vec::new();
    let mut start = 0u64;
    let mut map = Box::new(fiemap::Map {
        start: 0,
        length: 0,
        flags: 0,
        mapped_extents: 0,
        extent_count: 0,
        reserved: 0,
        extents: [fiemap::Extent::default(); fiemap::BATCH],
    });
    loop {
        map.start = start;
        map.length = u64::MAX - start;
        // No FIEMAP_FLAG_SYNC: a read-only scan should not force writeback.
        // Delayed allocations map without a physical place and count as
        // unique, which new data is.
        map.flags = 0;
        map.mapped_extents = 0;
        map.extent_count = fiemap::BATCH as u32;
        // SAFETY: `map` is a valid `struct fiemap` with room for
        // `extent_count` extents, and the fd is open.
        let r = unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                fiemap::FS_IOC_FIEMAP as _,
                &mut *map as *mut fiemap::Map,
            )
        };
        if r < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let n = (map.mapped_extents as usize).min(fiemap::BATCH);
        if n == 0 {
            break;
        }
        let mut last = false;
        for e in &map.extents[..n] {
            out.push(Extent {
                physical: e.physical,
                length: e.length,
                shared: e.flags & fiemap::FIEMAP_EXTENT_SHARED != 0,
                encoded: e.flags & fiemap::FIEMAP_EXTENT_ENCODED != 0,
            });
            last |= e.flags & fiemap::FIEMAP_EXTENT_LAST != 0;
        }
        let e = &map.extents[n - 1];
        start = e.logical.saturating_add(e.length);
        if last || start == u64::MAX {
            break;
        }
    }
    Ok(Mapped::Extents(out))
}

#[cfg(target_os = "macos")]
fn file_extents(file: &std::fs::File, _disk: u64) -> std::io::Result<Mapped> {
    use std::os::fd::AsRawFd;
    // From sys/attr.h: the private size of a file is an extended common
    // attribute, asked for in the fork group with FSOPT_ATTR_CMN_EXTENDED.
    const ATTR_BIT_MAP_COUNT: u16 = 5;
    const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;
    const ATTR_CMNEXT_PRIVATESIZE: u32 = 0x0000_0008;
    const FSOPT_ATTR_CMN_EXTENDED: u32 = 0x0000_0020;
    #[repr(C)]
    struct AttrList {
        bitmapcount: u16,
        reserved: u16,
        commonattr: u32,
        volattr: u32,
        dirattr: u32,
        fileattr: u32,
        forkattr: u32,
    }
    #[repr(C, packed(4))]
    struct Reply {
        length: u32,
        returned: [u32; 5],
        private_size: i64,
    }
    extern "C" {
        fn fgetattrlist(
            fd: libc::c_int,
            attr_list: *mut AttrList,
            attr_buf: *mut libc::c_void,
            attr_buf_size: libc::size_t,
            options: u32,
        ) -> libc::c_int;
    }
    let mut req = AttrList {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: ATTR_CMN_RETURNED_ATTRS,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: ATTR_CMNEXT_PRIVATESIZE,
    };
    let mut reply = Reply {
        length: 0,
        returned: [0; 5],
        private_size: 0,
    };
    // SAFETY: both structs match sys/attr.h and the buffer size is right.
    let r = unsafe {
        fgetattrlist(
            file.as_raw_fd(),
            &mut req,
            (&mut reply as *mut Reply).cast(),
            std::mem::size_of::<Reply>(),
            FSOPT_ATTR_CMN_EXTENDED,
        )
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // The fork group of the returned set says whether the size came back.
    let returned_fork = reply.returned[4];
    if returned_fork & ATTR_CMNEXT_PRIVATESIZE == 0 {
        return Err(std::io::ErrorKind::Unsupported.into());
    }
    let size = reply.private_size;
    Ok(Mapped::Private(u64::try_from(size).unwrap_or(0)))
}

#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android", target_os = "macos"))
))]
fn file_extents(_file: &std::fs::File, _disk: u64) -> std::io::Result<Mapped> {
    Err(std::io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext(physical: u64, length: u64, shared: bool) -> Extent {
        Extent {
            physical,
            length,
            shared,
            encoded: false,
        }
    }

    /// A shared compressed extent, as btrfs maps one.
    fn zext(physical: u64, length: u64) -> Extent {
        Extent {
            encoded: true,
            ..ext(physical, length, true)
        }
    }

    fn span(dev: u64, start: u64, end: u64) -> Span {
        Span {
            dev,
            encoded: false,
            start,
            end,
        }
    }

    #[test]
    fn coalesce_joins_ranges_per_device() {
        let mut v = vec![
            span(2, 0, 10),
            span(1, 50, 60),
            span(1, 0, 20),
            span(1, 10, 30),
            span(1, 30, 40),
            span(1, 5, 5),
        ];
        assert_eq!(coalesce(&mut v), 10 + 40 + 10);
        assert_eq!(v, vec![span(1, 0, 40), span(1, 50, 60), span(2, 0, 10)]);
        let mut none = Vec::new();
        assert_eq!(coalesce(&mut none), 0);
    }

    #[test]
    fn compressed_extents_count_once_by_where_they_start() {
        // What btrfs mapped in CI for 8 MiB of zstd text: 128 KiB extents,
        // each starting one 4 KiB block after the last.
        let file: Vec<Extent> = (0..64).map(|i| zext(4096 * i, 131072)).collect();
        let mut set = SharedSet::default();
        set.add(1, &file);
        // A reflink copy points at the same extents.
        set.add(1, &file);
        // Ranges would overlap into 63 blocks plus one extent.
        assert_eq!(set.distinct(), 64 * 131072);
    }

    #[test]
    fn a_compressed_extent_counts_at_its_longest_reference() {
        let mut v = vec![zext(0, 4096).span(1), zext(0, 65536).span(1)];
        assert_eq!(coalesce(&mut v), 65536);
    }

    #[test]
    fn a_plain_extent_inside_a_compressed_one_counts_on_its_own() {
        let mut v = vec![zext(0, 131072).span(1), ext(4096, 4096, true).span(1)];
        assert_eq!(coalesce(&mut v), 131072 + 4096);
    }

    #[test]
    fn split_counts_unshared_extents_and_caps_at_disk() {
        let e = [
            ext(0, 4096, false),
            ext(8192, 8192, true),
            ext(65536, 4096, false),
        ];
        let (u, s) = split(&e, 1 << 20);
        assert_eq!(u, 8192);
        assert_eq!(s, vec![ext(8192, 8192, true)]);
        // Compressed data maps at its logical length; disk caps it.
        assert_eq!(split(&e, 4096).0, 4096);
        assert_eq!(split(&[], 4096), (0, vec![]));
    }

    #[test]
    fn shared_set_counts_overlaps_once() {
        let mut a = SharedSet::default();
        a.add(1, &[ext(0, 10, true), ext(5, 10, true), ext(20, 5, true)]);
        assert_eq!(a.distinct(), 20);
    }

    #[test]
    fn shared_set_dedups_per_device() {
        let mut a = SharedSet::default();
        // Two reflinked copies point at one extent.
        a.add(1, &[ext(4096, 8192, true)]);
        a.add(1, &[ext(4096, 8192, true)]);
        let mut b = SharedSet::default();
        // The same physical range on another device is other space.
        b.add(2, &[ext(4096, 8192, true)]);
        b.add(1, &[ext(8192, 8192, true)]);
        a.merge(b);
        assert_eq!(a.referenced, 4 * 8192);
        assert_eq!(a.distinct(), 12288 + 8192);
    }

    #[cfg(unix)]
    #[test]
    fn plain_file_is_all_unique() {
        let p = std::env::temp_dir().join(format!("rtrash-extent-{}", std::process::id()));
        std::fs::write(&p, vec![9u8; 64 * 1024]).unwrap();
        let f = std::fs::File::open(&p).unwrap();
        use std::os::unix::fs::MetadataExt;
        let disk = f.metadata().unwrap().blocks() * 512;
        let s = file_share(&f, disk);
        // A fresh file shares nothing; file systems without extent maps
        // report the disk size.
        assert!(
            s.unique <= disk && s.unique >= disk.min(64 * 1024),
            "{} of {disk}",
            s.unique
        );
        assert!(s.shared.is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn probe_skips_small_files_and_asks_each_device_once() {
        let p = std::env::temp_dir().join(format!("rtrash-probe-{}", std::process::id()));
        std::fs::write(&p, b"x").unwrap();
        let probe = Probe::default();
        let opens = std::cell::Cell::new(0);
        let open = || {
            opens.set(opens.get() + 1);
            std::fs::File::open(&p).ok()
        };
        assert!(probe.wants(1, probe_min() - 1, open).is_none());
        assert_eq!(opens.get(), 0, "a small file was opened");
        let first = probe.wants(1, probe_min(), open).is_some();
        assert_eq!(opens.get(), 1);
        assert_eq!(first, fs_may_share(&std::fs::File::open(&p).unwrap()));
        let again = probe.wants(1, probe_min(), open).is_some();
        assert_eq!(again, first);
        // A device that shares nothing is not opened again.
        assert_eq!(opens.get(), if first { 2 } else { 1 });
        let _ = std::fs::remove_file(&p);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn apfs_clone_has_no_private_bytes() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::MetadataExt;
        let dir = std::env::temp_dir().join(format!("rtrash-clone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a");
        let b = dir.join("b");
        std::fs::write(&a, vec![5u8; 1 << 20]).unwrap();
        let ca = std::ffi::CString::new(a.as_os_str().as_bytes()).unwrap();
        let cb = std::ffi::CString::new(b.as_os_str().as_bytes()).unwrap();
        // SAFETY: two valid C strings.
        let cloned = unsafe { libc::clonefile(ca.as_ptr(), cb.as_ptr(), 0) } == 0;
        if cloned {
            let f = std::fs::File::open(&b).unwrap();
            let disk = f.metadata().unwrap().blocks() * 512;
            assert!(file_share(&f, disk).unique < disk / 2);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shared_bytes_stop_at_the_top_across_devices() {
        let half = u64::MAX / 2 + 1;
        let mut a = SharedSet::default();
        // Two devices, each with an extent of more than half of u64.
        a.add(1, &[ext(0, half, true)]);
        a.add(2, &[ext(0, half, true)]);
        assert_eq!(a.distinct(), u64::MAX);
        assert_eq!(a.referenced, u64::MAX);
    }
}
