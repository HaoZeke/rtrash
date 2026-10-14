//! Windows directory access. One `GetFileInformationByHandleEx` call with
//! `FileIdBothDirectoryInfo` lists many entries with their sizes,
//! attributes, write times and file IDs, so no entry needs a handle of its
//! own. The listing carries no link count, so every file goes through the
//! hard-link set by `(volume serial, file ID)`.
//!
//! Names are kept in the `OsStr` encoding of std, NUL-terminated like the
//! Unix names, so the walk handles both the same way.

use super::fileid::{mode_of, mtime_of, parse};
use super::{Entries, Stat};
use std::cell::RefCell;
use std::ffi::{c_void, CStr, OsStr};
use std::io;
use std::mem::{size_of, MaybeUninit};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{GetLastError, ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FileAttributeTagInfo, FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo,
    FileStandardInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_BOTH_DIR_INFO, FILE_INFO_BY_HANDLE_CLASS,
    FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_STANDARD_INFO, OPEN_EXISTING,
};

/// Bytes of the per-thread listing buffer.
const LIST_BUF: usize = 64 * 1024;

/// `path` as a NUL-terminated wide string. Long paths get the verbatim
/// prefix, which lifts the 260-character limit.
fn wide(path: &Path) -> Vec<u16> {
    let s: Vec<u16> = path.as_os_str().encode_wide().collect();
    let bs = u16::from(b'\\');
    let mut out = Vec::with_capacity(s.len() + 9);
    let verbatim = s.starts_with(&[bs, bs, u16::from(b'?'), bs]);
    if s.len() >= 248 && !verbatim && path.is_absolute() {
        if s.starts_with(&[bs, bs]) {
            out.extend(r"\\?\UNC".encode_utf16());
            out.extend_from_slice(&s[1..]);
        } else {
            out.extend(r"\\?\".encode_utf16());
            out.extend_from_slice(&s);
        }
    } else {
        out.extend_from_slice(&s);
    }
    out.push(0);
    out
}

/// Open `path` itself, never what a reparse point names.
fn open_handle(path: &Path, access: u32) -> io::Result<OwnedHandle> {
    let w = wide(path);
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

fn handle_stat(h: &OwnedHandle) -> io::Result<Stat> {
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
    Ok(Stat {
        mode: mode_of(tag.FileAttributes, tag.ReparseTag),
        size: std_info.EndOfFile.max(0) as u64,
        blocks: (std_info.AllocationSize.max(0) as u64).div_ceil(512),
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
    /// Open the directory at `path`; a symlink or junction is refused.
    pub fn open(path: &Path) -> io::Result<Dir> {
        let handle = open_handle(path, FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)?;
        let stat = handle_stat(&handle)?;
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
    handle_stat(&open_handle(path, FILE_READ_ATTRIBUTES)?)
}

/// Windows has no descriptor limit to raise.
pub fn raise_nofile_limit() {}

/// Name bytes of an entry as an `OsStr`.
pub fn os(name: &CStr) -> &OsStr {
    // SAFETY: every name came from `OsStr::as_encoded_bytes` in `parse`,
    // and the NUL that ends it is not part of `to_bytes`.
    unsafe { OsStr::from_encoded_bytes_unchecked(name.to_bytes()) }
}
