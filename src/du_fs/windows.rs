//! Windows directory access. One `GetFileInformationByHandleEx` call with
//! `FileIdBothDirectoryInfo` lists many entries with their sizes,
//! attributes, write times and file IDs, so no entry needs a handle of its
//! own. The listing carries no link count, so every file goes through the
//! hard-link set by `(volume serial, file ID)`.
//!
//! Names are kept in the `OsStr` encoding of std, NUL-terminated like the
//! Unix names, so the walk handles both the same way.

use super::fileid::{is_wof, mode_of, mtime_of, needs_real_size, parse, wof_streams_size};
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
    CreateFileW, FileAttributeTagInfo, FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo,
    FileStandardInfo, FileStreamInfo, GetCompressedFileSizeW, GetFileInformationByHandle,
    GetFileInformationByHandleEx, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_TAG_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_BOTH_DIR_INFO,
    FILE_INFO_BY_HANDLE_CLASS, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, OPEN_EXISTING,
};

/// Bytes of the per-thread listing buffer.
const LIST_BUF: usize = 64 * 1024;

/// Open `path` itself, never what a reparse point names.
fn open_handle(path: &Path, access: u32) -> io::Result<OwnedHandle> {
    let w = crate::util::win_path(path);
    // SAFETY: `w` is NUL-terminated; a valid returned handle is ours.
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
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

/// 34(j): the disk use of a Compact file, whose main stream is sparse
/// with no clusters: the allocation of its streams, the packed
/// `WofCompressedData` among them. The handle of a stat skips the Compact
/// filter, so the stream list shows that stream. Failing that, the
/// packed stream opened by name gives its own allocation size.
fn wof_size(h: &OwnedHandle, path: Option<&Path>) -> Option<u64> {
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
    if ok != 0 {
        // SAFETY: a u64 slice viewed as its bytes.
        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * 8) };
        if let Some(n) = wof_streams_size(bytes) {
            return Some(n);
        }
    }
    let mut named = path?.as_os_str().to_os_string();
    named.push(":WofCompressedData");
    let stream = open_handle(Path::new(&named), FILE_READ_ATTRIBUTES).ok()?;
    let info: FILE_STANDARD_INFO = info_ex(&stream, FileStandardInfo).ok()?;
    Some(info.AllocationSize.max(0) as u64)
}

/// The disk use of a file that [`needs_real_size`] names: the packed
/// streams of a Compact file ([`wof_size`]), else GetCompressedFileSizeW
/// by `path` ([`compressed_size`]).
fn packed_size(h: &OwnedHandle, attrs: u32, tag: u32, path: Option<&Path>) -> Option<u64> {
    if is_wof(attrs, tag) {
        if let Some(n) = wof_size(h, path) {
            return Some(n);
        }
    }
    path.and_then(compressed_size)
}

/// The stat of the open handle `h`. A compressed or Compact file gets
/// its real disk use ([`packed_size`]); `path` is the name of `h`.
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
    let (attrs, rtag) = (tag.FileAttributes, tag.ReparseTag);
    let packed = if needs_real_size(attrs, rtag) {
        packed_size(h, attrs, rtag, path)
    } else {
        None
    };
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

    /// [`Self::entries`], stopping once `max` or more are in: the rest of
    /// the buffer that crossed the mark comes along.
    pub fn entries_upto(&self, max: usize) -> (Entries, bool) {
        thread_local! {
            static BUF: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
        }
        BUF.with(|b| {
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
        })
    }
}

// The stream parser reads FILE_STREAM_INFO at its own offsets.
const _: () = {
    use super::fileid::{STREAM_ALLOC, STREAM_NAME, STREAM_NAME_LEN, STREAM_NEXT};
    use std::mem::offset_of;
    use windows_sys::Win32::Storage::FileSystem::FILE_STREAM_INFO;
    assert!(offset_of!(FILE_STREAM_INFO, NextEntryOffset) == STREAM_NEXT);
    assert!(offset_of!(FILE_STREAM_INFO, StreamNameLength) == STREAM_NAME_LEN);
    assert!(offset_of!(FILE_STREAM_INFO, StreamAllocationSize) == STREAM_ALLOC);
    assert!(offset_of!(FILE_STREAM_INFO, StreamName) == STREAM_NAME);
};

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
