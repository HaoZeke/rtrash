//! Windows directory access. One `GetFileInformationByHandleEx` call with
//! `FileIdBothDirectoryInfo` lists many entries with their sizes,
//! attributes, write times and file IDs, so no entry needs a handle of its
//! own. The listing carries no link count, so every file goes through the
//! hard-link set by `(volume serial, file ID)`.
//!
//! Names are kept in the `OsStr` encoding of std, NUL-terminated like the
//! Unix names, so the walk handles both the same way.

use super::fileid::{
    listed, mode_of, mtime_of, needs_real_size, parse, stream_records, Listed, STREAM_ALLOC,
    STREAM_NAME, STREAM_NAME_LEN, STREAM_NEXT,
};
use super::{Entries, Stat};
use std::cell::RefCell;
use std::ffi::{c_void, CStr, OsStr};
use std::io;
use std::mem::{size_of, MaybeUninit};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    GetLastError, SetLastError, ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FileAttributeTagInfo, FileCompressionInfo, FileIdBothDirectoryInfo,
    FileIdBothDirectoryRestartInfo, FileStandardInfo, FileStreamInfo, GetCompressedFileSizeW,
    GetFileInformationByHandle, GetFileInformationByHandleEx, BY_HANDLE_FILE_INFORMATION,
    FILE_ATTRIBUTE_TAG_INFO, FILE_COMPRESSION_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_BOTH_DIR_INFO, FILE_INFO_BY_HANDLE_CLASS,
    FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_STANDARD_INFO, FILE_STREAM_INFO, OPEN_EXISTING,
};

/// Bytes of the per-thread listing buffer.
const LIST_BUF: usize = 64 * 1024;

/// Open `path` itself, never what a reparse point names.
fn open_handle(path: &Path, access: u32) -> io::Result<OwnedHandle> {
    open_with(
        path,
        access,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
    )
}

/// Open `path` with the CreateFileW flags `flags`, sharing it fully.
fn open_with(path: &Path, access: u32, flags: u32) -> io::Result<OwnedHandle> {
    let w = crate::util::win_path(path);
    // SAFETY: `w` is NUL-terminated; a valid returned handle is ours.
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            flags,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(h as _) })
}

fn info_ex<T>(h: &OwnedHandle, class: FILE_INFO_BY_HANDLE_CLASS) -> io::Result<T> {
    let mut v = MaybeUninit::<T>::uninit();
    // SAFETY: the buffer holds exactly one `T`, the struct of `class`.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            h.as_raw_handle() as _,
            class,
            v.as_mut_ptr() as *mut c_void,
            size_of::<T>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { v.assume_init() })
}

/// gdu #407: the bytes on disk of a file that NTFS compression or Compact
/// packed, as Explorer's "Size on disk" shows them. GetCompressedFileSizeW
/// opens the file through the Compact filter, which a handle opened with
/// FILE_FLAG_OPEN_REPARSE_POINT skips.
fn compressed_size(path: &Path) -> Option<u64> {
    let w = crate::util::win_path(path);
    let mut high = 0u32;
    // SAFETY: `w` is NUL-terminated and `high` is a u32 to write.
    let low = unsafe {
        SetLastError(0);
        GetCompressedFileSizeW(w.as_ptr(), &mut high)
    };
    // A size whose low half is all ones is also u32::MAX; the error says.
    if low == u32::MAX && unsafe { GetLastError() } != 0 {
        return None;
    }
    Some(u64::from(high) << 32 | u64::from(low))
}

/// The stat of the open handle `h`. With `path`, a compressed or Compact
/// file gets its real disk use ([`compressed_size`]).
fn handle_stat(h: &OwnedHandle, path: Option<&Path>) -> io::Result<Stat> {
    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: GetFileInformationByHandle fills `info` on success.
    if unsafe { GetFileInformationByHandle(h.as_raw_handle() as _, info.as_mut_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let info = unsafe { info.assume_init() };
    let tag: FILE_ATTRIBUTE_TAG_INFO = info_ex(h, FileAttributeTagInfo)?;
    let std_info: FILE_STANDARD_INFO = info_ex(h, FileStandardInfo)?;
    let wt = info.ftLastWriteTime;
    let write = (u64::from(wt.dwHighDateTime) << 32 | u64::from(wt.dwLowDateTime)) as i64;
    let packed = path
        .filter(|_| needs_real_size(tag.FileAttributes, tag.ReparseTag))
        .and_then(compressed_size);
    let allocated = packed.unwrap_or(std_info.AllocationSize.max(0) as u64);
    Ok(Stat {
        mode: mode_of(tag.FileAttributes, tag.ReparseTag),
        size: std_info.EndOfFile.max(0) as u64,
        blocks: allocated.div_ceil(512),
        nlink: u64::from(std_info.NumberOfLinks),
        ino: u64::from(info.nFileIndexHigh) << 32 | u64::from(info.nFileIndexLow),
        dev: u64::from(info.dwVolumeSerialNumber),
        mtime: mtime_of(write),
    })
}

/// GetCompressedFileSizeW on the NUL-terminated wide path `w`, with the
/// error when it fails. For the size report.
fn compressed_size_wide(w: &[u16]) -> io::Result<u64> {
    let mut high = 0u32;
    // SAFETY: `w` is NUL-terminated and `high` is a u32 to write.
    let low = unsafe {
        SetLastError(0);
        GetCompressedFileSizeW(w.as_ptr(), &mut high)
    };
    if low == u32::MAX && unsafe { GetLastError() } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(u64::from(high) << 32 | u64::from(low))
}

/// The FILE_STREAM_INFO records of the open file `h`, as bytes.
fn stream_list(h: &OwnedHandle) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u64; 2048];
    // SAFETY: the buffer is 8-byte aligned and writable for its full
    // length.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            h.as_raw_handle() as _,
            FileStreamInfo,
            buf.as_mut_ptr() as *mut c_void,
            (buf.len() * 8) as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(buf.iter().flat_map(|w| w.to_ne_bytes()).collect())
}

/// What the listing of the folder `dir` says of its entry `name`
/// ([`listed`]), or None when no fill names it.
fn listing_of(dir: &Path, name: &str) -> io::Result<Option<Listed>> {
    let h = open_handle(dir, FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)?;
    let mut buf = vec![0u64; LIST_BUF / 8];
    let mut class = FileIdBothDirectoryRestartInfo;
    loop {
        // SAFETY: the buffer is 8-byte aligned and writable for its full
        // length.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                h.as_raw_handle() as _,
                class,
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * 8) as u32,
            )
        };
        class = FileIdBothDirectoryInfo;
        if ok == 0 {
            if unsafe { GetLastError() } == ERROR_NO_MORE_FILES {
                return Ok(None);
            }
            return Err(io::Error::last_os_error());
        }
        let bytes: Vec<u8> = buf.iter().flat_map(|w| w.to_ne_bytes()).collect();
        if let Some(found) = listed(&bytes, name) {
            return Ok(Some(found));
        }
    }
}

/// An error as its Windows code, for the size report.
fn code(e: io::Error) -> String {
    format!("error {}", e.raw_os_error().unwrap_or(-1))
}

/// 34(j) size report for the Windows du test, which prints it before its
/// asserts: what each way of asking Windows gives for the file `path`,
/// one line each, then the stat du takes. It only reads, never panics,
/// and gives each failure as its error code. Two opens are asked: one
/// with FILE_FLAG_OPEN_REPARSE_POINT, as du opens, and one without, so
/// the Compact filter is in the path.
pub fn size_probe(path: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    out.push(match listing_of(dir, &name) {
        Ok(Some((a, ea, alloc, len))) => {
            format!("listing: attrs {a:#x} ea/tag {ea:#x} alloc {alloc} len {len}")
        }
        Ok(None) => "listing: no entry".to_string(),
        Err(e) => format!("listing: {}", code(e)),
    });
    let mut stream = path.as_os_str().to_os_string();
    stream.push(":WofCompressedData");
    let stream = PathBuf::from(stream);
    let reparse = FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT;
    let opens = [
        ("reparse-point open", reparse),
        ("filter open", FILE_FLAG_BACKUP_SEMANTICS),
    ];
    for (what, flags) in opens {
        let h = match open_with(path, FILE_READ_ATTRIBUTES, flags) {
            Ok(h) => h,
            Err(e) => {
                out.push(format!("{what}: {}", code(e)));
                continue;
            }
        };
        out.push(
            match info_ex::<FILE_ATTRIBUTE_TAG_INFO>(&h, FileAttributeTagInfo) {
                Ok(t) => format!(
                    "{what}: attrs {:#x} tag {:#x}",
                    t.FileAttributes, t.ReparseTag
                ),
                Err(e) => format!("{what}: tag info {}", code(e)),
            },
        );
        out.push(match info_ex::<FILE_STANDARD_INFO>(&h, FileStandardInfo) {
            Ok(i) => format!(
                "{what}: standard alloc {} len {}",
                i.AllocationSize, i.EndOfFile
            ),
            Err(e) => format!("{what}: standard {}", code(e)),
        });
        out.push(
            match info_ex::<FILE_COMPRESSION_INFO>(&h, FileCompressionInfo) {
                Ok(c) => format!(
                    "{what}: compression size {} format {}",
                    c.CompressedFileSize, c.CompressionFormat
                ),
                Err(e) => format!("{what}: compression {}", code(e)),
            },
        );
        out.push(match stream_list(&h).map(|b| stream_records(&b)) {
            Ok(Some(streams)) => format!("{what}: streams {streams:?}"),
            Ok(None) => format!("{what}: streams cut short"),
            Err(e) => format!("{what}: streams {}", code(e)),
        });
        let by_name = open_with(&stream, FILE_READ_ATTRIBUTES, flags)
            .and_then(|s| info_ex::<FILE_STANDARD_INFO>(&s, FileStandardInfo));
        out.push(match by_name {
            Ok(i) => format!(
                "{what}: by-name stream alloc {} len {}",
                i.AllocationSize, i.EndOfFile
            ),
            Err(e) => format!("{what}: by-name stream {}", code(e)),
        });
    }
    out.push(match compressed_size_wide(&crate::util::win_path(path)) {
        Ok(n) => format!("GetCompressedFileSizeW by path: {n}"),
        Err(e) => format!("GetCompressedFileSizeW by path: {}", code(e)),
    });
    match std::path::absolute(path) {
        Ok(abs) => {
            let mut verbatim = std::ffi::OsString::from(r"\\?\");
            verbatim.push(crate::util::plain_path(&abs).as_os_str());
            let w = crate::util::win_path(Path::new(&verbatim));
            out.push(match compressed_size_wide(&w) {
                Ok(n) => format!("GetCompressedFileSizeW verbatim: {n}"),
                Err(e) => format!("GetCompressedFileSizeW verbatim: {}", code(e)),
            });
        }
        Err(e) => out.push(format!("GetCompressedFileSizeW verbatim: {}", code(e))),
    }
    out.push(match lstat(path) {
        Ok(st) => format!(
            "du stat: blocks {} = {} bytes",
            st.blocks,
            st.blocks.saturating_mul(512)
        ),
        Err(e) => format!("du stat: {}", code(e)),
    });
    out
}

// The stream report reads FILE_STREAM_INFO at its own offsets.
const _: () = {
    use std::mem::offset_of;
    assert!(offset_of!(FILE_STREAM_INFO, NextEntryOffset) == STREAM_NEXT);
    assert!(offset_of!(FILE_STREAM_INFO, StreamNameLength) == STREAM_NAME_LEN);
    assert!(offset_of!(FILE_STREAM_INFO, StreamAllocationSize) == STREAM_ALLOC);
    assert!(offset_of!(FILE_STREAM_INFO, StreamName) == STREAM_NAME);
};

/// An open directory.
#[derive(Debug)]
pub struct Dir {
    handle: OwnedHandle,
    path: PathBuf,
    stat: Stat,
}

impl Dir {
    /// Windows has no kernel pseudo file systems to skip.
    pub fn is_kernfs(&self) -> bool {
        false
    }

    /// The Windows listing gives each entry its stat, so no inode number
    /// is ever looked up.
    pub fn lists_true_inodes(&self) -> bool {
        false
    }

    /// Windows has no btrfs snapshots.
    pub fn is_readonly_snapshot(&self) -> bool {
        false
    }

    /// Open the directory at `path`. A symlink, a junction, a volume mount
    /// point or any other reparse point that names another folder is
    /// refused, so no walk crosses one (see `fileid::is_link`).
    pub fn open(path: &Path) -> io::Result<Dir> {
        let handle = open_handle(path, FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)?;
        let stat = handle_stat(&handle, None)?;
        if !stat.is_dir() {
            return Err(io::Error::from_raw_os_error(267)); // ERROR_DIRECTORY
        }
        Ok(Dir {
            handle,
            path: path.to_path_buf(),
            stat,
        })
    }

    /// Open the subdirectory `name` of this directory.
    /// `du -L` is refused on Windows; this keeps the walker portable.
    pub fn open_follow(path: &Path) -> io::Result<Dir> {
        Self::open(path)
    }

    pub fn open_child_follow(&self, name: &CStr) -> io::Result<Dir> {
        self.open_child(name)
    }

    pub fn stat_follow(&self, name: &CStr) -> io::Result<Stat> {
        self.stat_at(name)
    }

    pub fn open_child(&self, name: &CStr) -> io::Result<Dir> {
        Dir::open(&self.path.join(os(name)))
    }

    /// Stat of the directory itself.
    pub fn stat(&self) -> io::Result<Stat> {
        Ok(self.stat)
    }

    /// Stat of entry `name`, not following reparse points.
    pub fn stat_at(&self, name: &CStr) -> io::Result<Stat> {
        lstat(&self.path.join(os(name)))
    }

    /// Read every entry. Returns the entries read and whether reading
    /// stopped on an error.
    pub fn entries(&self) -> (Entries, bool) {
        self.entries_upto(usize::MAX)
    }

    /// [`Self::entries`]: the listing gives every stat already, so there
    /// are no inode numbers to keep.
    pub fn entries_with_inos(&self) -> (Entries, bool) {
        self.entries()
    }

    /// [`Self::entries`], stopping once `max` or more are in: the rest of
    /// the buffer that crossed the mark comes along.
    pub fn entries_upto(&self, max: usize) -> (Entries, bool) {
        thread_local! {
            static BUF: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
        }
        let (mut out, failed) = BUF.with(|b| {
            let mut buf = b.borrow_mut();
            if buf.len() < LIST_BUF / 8 {
                buf.resize(LIST_BUF / 8, 0);
            }
            let mut out = Entries::default();
            let mut class = FileIdBothDirectoryRestartInfo;
            loop {
                // SAFETY: the buffer is 8-byte aligned and writable for its
                // full length.
                let ok = unsafe {
                    GetFileInformationByHandleEx(
                        self.handle.as_raw_handle() as _,
                        class,
                        buf.as_mut_ptr() as *mut c_void,
                        (buf.len() * 8) as u32,
                    )
                };
                class = FileIdBothDirectoryInfo;
                if ok == 0 {
                    let failed = unsafe { GetLastError() } != ERROR_NO_MORE_FILES;
                    return (out, failed);
                }
                // SAFETY: a u64 slice viewed as its bytes.
                let bytes =
                    unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * 8) };
                if !parse(bytes, self.stat.dev, &mut out) {
                    return (out, true);
                }
                if out.len() >= max {
                    return (out, false);
                }
            }
        });
        // 34(j): a file the listing shows with no clusters may be one that
        // Compact packed; the stat by path gives its packed size. A stat
        // that fails leaves the listed one.
        out.restat(|name| lstat(&self.path.join(os(name))).ok());
        (out, failed)
    }
}

// The parser reads records through its own mirror of the struct.
const _: () = {
    use super::fileid::Record;
    use std::mem::offset_of;
    assert!(size_of::<Record>() == size_of::<FILE_ID_BOTH_DIR_INFO>());
    assert!(offset_of!(Record, file_id) == offset_of!(FILE_ID_BOTH_DIR_INFO, FileId));
    assert!(offset_of!(Record, file_name) == offset_of!(FILE_ID_BOTH_DIR_INFO, FileName));
    assert!(offset_of!(Record, ea_size) == offset_of!(FILE_ID_BOTH_DIR_INFO, EaSize));
};

/// Stat `path` without following a final reparse point.
/// `du -L` is refused on Windows; see [`Dir::open_follow`].
pub fn stat_follow(path: &Path) -> io::Result<Stat> {
    lstat(path)
}

pub fn lstat(path: &Path) -> io::Result<Stat> {
    handle_stat(&open_handle(path, FILE_READ_ATTRIBUTES)?, Some(path))
}

/// Windows has no descriptor limit to raise.
pub fn raise_nofile_limit() {}

/// Name bytes of an entry as an `OsStr`.
pub fn os(name: &CStr) -> &OsStr {
    // SAFETY: every name came from `OsStr::as_encoded_bytes` in `parse`,
    // and the NUL that ends it is not part of `to_bytes`.
    unsafe { OsStr::from_encoded_bytes_unchecked(name.to_bytes()) }
}
