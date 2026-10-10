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

use std::collections::HashMap;

/// One mapped extent of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub physical: u64,
    pub length: u64,
    pub shared: bool,
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
    ranges: HashMap<u64, Vec<(u64, u64)>>,
    /// Bytes of shared extents as the files reference them, with repeats.
    pub referenced: u64,
}

impl SharedSet {
    pub fn add(&mut self, dev: u64, extents: &[Extent]) {
        if extents.is_empty() {
            return;
        }
        let v = self.ranges.entry(dev).or_default();
        for e in extents {
            self.referenced = self.referenced.saturating_add(e.length);
            v.push((e.physical, e.physical.saturating_add(e.length)));
        }
    }

    pub fn merge(&mut self, other: SharedSet) {
        self.referenced = self.referenced.saturating_add(other.referenced);
        for (dev, v) in other.ranges {
            self.ranges.entry(dev).or_default().extend(v);
        }
    }

    /// Bytes the shared extents take on disk, each range counted once.
    pub fn distinct(&self) -> u64 {
        self.ranges.values().map(|v| union_len(v)).sum()
    }
}

/// Total length of the union of half-open ranges `[start, end)`.
pub fn union_len(ranges: &[(u64, u64)]) -> u64 {
    let mut v: Vec<(u64, u64)> = ranges.iter().copied().filter(|(s, e)| e > s).collect();
    v.sort_unstable();
    let mut total = 0u64;
    let mut cur: Option<(u64, u64)> = None;
    for (s, e) in v {
        cur = match cur {
            Some((cs, ce)) if s <= ce => Some((cs, ce.max(e))),
            Some((cs, ce)) => {
                total += ce - cs;
                Some((s, e))
            }
            None => Some((s, e)),
        };
    }
    if let Some((cs, ce)) = cur {
        total += ce - cs;
    }
    total
}

/// What one file adds to a `--unique` scan.
#[derive(Debug, Default)]
pub struct FileShare {
    pub unique: u64,
    pub shared: Vec<Extent>,
}

/// Unique size of an open regular file whose allocated size is `disk`.
/// Without an extent map (unsupported file system, platform or error),
/// the whole file counts as unique.
#[cfg(unix)]
pub fn file_share(file: &std::fs::File, disk: u64) -> FileShare {
    match file_extents(file, disk) {
        Ok(Mapped::Extents(ext)) => {
            let (unique, shared) = split(&ext, disk);
            FileShare { unique, shared }
        }
        Ok(Mapped::Private(p)) => FileShare {
            unique: p.min(disk),
            shared: Vec::new(),
        },
        Err(_) => FileShare {
            unique: disk,
            shared: Vec::new(),
        },
    }
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
        }
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
    fn union_counts_overlaps_once() {
        assert_eq!(union_len(&[]), 0);
        assert_eq!(union_len(&[(0, 10), (5, 15), (20, 25)]), 20);
        assert_eq!(union_len(&[(20, 25), (0, 10), (10, 20)]), 25);
        assert_eq!(union_len(&[(7, 7), (3, 1)]), 0);
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
}
