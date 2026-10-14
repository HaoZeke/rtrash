//! Windows system Recycle Bin backend.
//!
//! This is **not** FreeDesktop. Put/list/restore/empty use the OS Recycle Bin
//! (shell APIs via the `trash` crate on Windows). Host-free pure helpers for
//! `$I` metadata parsing are always compiled so Linux CI can unit-test them.

use std::path::{Path, PathBuf};

#[cfg(windows)]
use std::io;

#[cfg(windows)]
use crate::platform;
#[cfg(windows)]
use crate::util;

/// Metadata from a Vista+ `$I…` Recycle Bin index file (on-disk companion to `$R…`).
///
/// Format (version 2, Windows 10+):
/// - `0x00` u64 version (=2)
/// - `0x08` u64 file size
/// - `0x10` u64 FILETIME (100ns since 1601-01-01 UTC)
/// - `0x18` u32 path byte length (UTF-16LE bytes including NUL)
/// - `0x1C` UTF-16LE path
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecycleIMeta {
    pub version: u64,
    pub size: u64,
    pub delete_filetime: u64,
    pub original: PathBuf,
}

/// FILETIME units between 1601-01-01 and 1970-01-01 UTC.
pub const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;
/// 100-nanosecond intervals per second.
pub const HUNDRED_NS_PER_SEC: u64 = 10_000_000;

/// Convert Windows FILETIME to unix epoch seconds (saturating).
pub fn filetime_to_unix_epoch(ft: u64) -> i64 {
    if ft < FILETIME_UNIX_EPOCH {
        return 0;
    }
    ((ft - FILETIME_UNIX_EPOCH) / HUNDRED_NS_PER_SEC) as i64
}

/// Parse a Vista+ `$I` index buffer. Returns `None` if the header is too short
/// or the UTF-16 path is malformed.
pub fn parse_recycle_i(bytes: &[u8]) -> Option<RecycleIMeta> {
    if bytes.len() < 0x18 {
        return None;
    }
    let version = u64::from_le_bytes(bytes[0x00..0x08].try_into().ok()?);
    let size = u64::from_le_bytes(bytes[0x08..0x10].try_into().ok()?);
    let delete_filetime = u64::from_le_bytes(bytes[0x10..0x18].try_into().ok()?);

    let original = if version >= 2 {
        // Version 2: a u32 count of UTF-16 units (the trailing NUL
        // included) at 0x18, then the path at 0x1C. The count is in
        // characters, not bytes.
        if bytes.len() < 0x1C + 2 {
            return None;
        }
        let units = u32::from_le_bytes(bytes[0x18..0x1C].try_into().ok()?) as usize;
        let end = 0x1C_usize
            .saturating_add(units.saturating_mul(2))
            .min(bytes.len());
        decode_utf16le_path(&bytes[0x1C..end])?
    } else {
        // Version 1: fixed 260 wchar (520 byte) buffer at 0x18.
        if bytes.len() < 0x18 + 2 {
            return None;
        }
        let end = (0x18 + 520).min(bytes.len());
        decode_utf16le_path(&bytes[0x18..end])?
    };

    Some(RecycleIMeta {
        version,
        size,
        delete_filetime,
        original,
    })
}

fn decode_utf16le_path(bytes: &[u8]) -> Option<PathBuf> {
    if bytes.len() < 2 {
        return None;
    }
    let mut units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    // The path ends at the first NUL; what follows is padding.
    if let Some(n) = units.iter().position(|&u| u == 0) {
        units.truncate(n);
    }
    if units.is_empty() {
        return None;
    }
    let s = String::from_utf16(&units).ok()?;
    is_plausible_original(&s).then(|| PathBuf::from(s))
}

/// An original path from a `$I` record looks right: absolute (`C:\\` or
/// `\\\\server\\share`) and free of control characters.
fn is_plausible_original(s: &str) -> bool {
    let b = s.as_bytes();
    let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\';
    (drive || s.starts_with(r"\\")) && !s.chars().any(char::is_control)
}

/// Build a minimal version-2 `$I` buffer for tests (round-trip with [`parse_recycle_i`]).
pub fn encode_recycle_i_v2(original: &Path, size: u64, delete_filetime: u64) -> Vec<u8> {
    let path_utf16: Vec<u16> = original
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let units = path_utf16.len() as u32;
    let mut out = Vec::with_capacity(0x1C + path_utf16.len() * 2);
    out.extend_from_slice(&2u64.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&delete_filetime.to_le_bytes());
    out.extend_from_slice(&units.to_le_bytes());
    for u in path_utf16 {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out
}

/// The `$I` index file beside the `$R` payload at `payload`: same
/// folder, same name after the first two characters. `None` when the
/// name does not start with `$R`.
pub fn index_file_for(payload: &Path) -> Option<PathBuf> {
    let name = payload.file_name()?.to_str()?;
    let rest = name.strip_prefix("$R")?;
    Some(payload.with_file_name(format!("$I{rest}")))
}

/// The drive root (`C:\`) a Recycle Bin path such as
/// `C:\$Recycle.Bin\S-1-5-21-…\$R1A2B3C.txt` lies on.
pub fn drive_root_of(path: &Path) -> Option<PathBuf> {
    let s = path.to_str()?;
    let b = s.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
        .then(|| PathBuf::from(format!("{}:\\", char::from(b[0].to_ascii_uppercase()))))
}

/// A key for the volume that holds `path`, for telling drives apart where
/// there are no device numbers: the drive letter for `C:\\...`, a hash of
/// `\\\\server\\share` for a network path, else 0. A relative path counts
/// from the current directory.
pub fn drive_key(path: &Path) -> u64 {
    match key_of(&path.to_string_lossy()) {
        0 => std::path::absolute(path).map_or(0, |a| key_of(&a.to_string_lossy())),
        k => k,
    }
}

/// Windows: a key for the volume that holds `path`, by its serial number,
/// so a volume mounted on a folder (`C:\\mnt\\data`) counts apart from the
/// drive that holds the folder. Falls back to [`drive_key`] when the
/// volume cannot be asked.
#[cfg(windows)]
pub fn volume_key(path: &Path) -> u64 {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;
    let Some(root) = volume_root(path) else {
        return drive_key(path);
    };
    let root: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut serial = 0u32;
    // SAFETY: `root` is NUL-terminated; the other outputs are null or a u32.
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 || serial == 0 {
        return drive_key(path);
    }
    serial_key(serial)
}

/// Windows: the root of the volume that holds `path`: `C:\\` for a
/// plain drive, or the folder a volume is mounted on (`C:\\mnt\\data\\`).
#[cfg(windows)]
pub fn volume_root(path: &Path) -> Option<PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW;
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let w: Vec<u16> = abs.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut root = [0u16; 1024];
    // SAFETY: `w` is NUL-terminated and `root` is as long as the length passed.
    if unsafe { GetVolumePathNameW(w.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
        return None;
    }
    let n = root.iter().position(|&c| c == 0).unwrap_or(root.len());
    Some(PathBuf::from(std::ffi::OsString::from_wide(&root[..n])))
}

/// Windows: the root of the volume whose Recycle Bin holds the item at
/// `id`: its drive, or the folder its volume is mounted on.
#[cfg(windows)]
pub fn bin_root_of(id: &Path) -> Option<PathBuf> {
    volume_root(id)
        .map(|r| {
            let s = r.to_string_lossy();
            s.strip_prefix(r"\\?\")
                .map_or_else(|| r.clone(), PathBuf::from)
        })
        .or_else(|| drive_root_of(id))
}

/// The `put --verbose` line for `name`: it names the Recycle Bin the item
/// went to, by its volume root, so an item on a volume mounted on a folder
/// shows the bin of that volume, not the bin of the drive above it.
pub fn removed_line(name: &str, bin: Option<&Path>) -> String {
    match bin {
        Some(root) => format!("removed '{name}' (Recycle Bin of {})", root.display()),
        None => format!("removed '{name}'"),
    }
}

/// Windows: the folders that volumes are mounted on (not drive roots),
/// each with a trailing backslash, for the history of their Recycle Bins.
#[cfg(windows)]
pub fn mounted_volumes() -> Vec<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetVolumePathNamesForVolumeNameW,
    };
    let mut out = Vec::new();
    let mut name = [0u16; 64];
    // SAFETY: `name` is as long as the length passed.
    let h = unsafe { FindFirstVolumeW(name.as_mut_ptr(), name.len() as u32) };
    if h == INVALID_HANDLE_VALUE {
        return out;
    }
    loop {
        let mut paths = vec![0u16; 4096];
        let mut len = 0u32;
        // SAFETY: `name` is NUL-terminated; `paths` is as long as passed.
        let ok = unsafe {
            GetVolumePathNamesForVolumeNameW(
                name.as_ptr(),
                paths.as_mut_ptr(),
                paths.len() as u32,
                &mut len,
            )
        };
        if ok != 0 {
            // A list of NUL-terminated strings, ending in an empty one.
            for p in paths.split(|&c| c == 0).take_while(|p| !p.is_empty()) {
                let p = PathBuf::from(std::ffi::OsString::from_wide(p));
                if mounted_folder(&p.to_string_lossy()) {
                    out.push(p);
                }
            }
        }
        // SAFETY: `h` came from FindFirstVolumeW; `name` as above.
        if unsafe { FindNextVolumeW(h, name.as_mut_ptr(), name.len() as u32) } == 0 {
            break;
        }
    }
    // SAFETY: `h` came from FindFirstVolumeW and is closed once.
    unsafe { FindVolumeClose(h) };
    out.sort();
    out
}

/// A volume path that is a folder rather than a drive root (`C:\\`).
pub fn mounted_folder(path: &str) -> bool {
    let b = path.as_bytes();
    let root = b.len() <= 3 && b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    !root && !path.is_empty()
}

/// The name of the history file for the Recycle Bin of volume `root`:
/// `C.txt` for a drive root, else `vol-SERIAL.txt` from its volume key.
pub fn history_name(root: &str, key: u64) -> String {
    match root.as_bytes() {
        [l, b':', ..] if !mounted_folder(root) && l.is_ascii_alphabetic() => {
            format!("{}.txt", char::from(l.to_ascii_uppercase()))
        }
        _ => format!("vol-{:08x}.txt", key & 0xffff_ffff),
    }
}

/// The indices of the Recycle Bin items in `ids` whose volume, by `key`,
/// is `want`: the items a purge on that volume may take. An unknown
/// volume (key 0) matches nothing.
pub fn on_volume(ids: &[PathBuf], key: impl Fn(&Path) -> u64, want: u64) -> Vec<usize> {
    if want == 0 {
        return Vec::new();
    }
    (0..ids.len()).filter(|&i| key(&ids[i]) == want).collect()
}

/// The key [`volume_key`] gives a volume serial number: the high bit
/// below the share flag set, so it never equals a drive letter's key or a
/// share's.
pub fn serial_key(serial: u32) -> u64 {
    u64::from(serial) | 1 << 62
}

fn key_of(s: &str) -> u64 {
    let s = s.replace('/', "\\");
    let s = s
        .strip_prefix(r"\\?\UNC\")
        .map(|r| format!(r"\\{r}"))
        .unwrap_or_else(|| s.strip_prefix(r"\\?\").unwrap_or(&s).to_string());
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return u64::from(b[0].to_ascii_uppercase());
    }
    if let Some(rest) = s.strip_prefix(r"\\") {
        let share: Vec<&str> = rest.splitn(3, '\\').take(2).collect();
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for c in share.join("\\").to_lowercase().bytes() {
            h = (h ^ u64::from(c)).wrapping_mul(0x100_0000_01b3);
        }
        return h | 1 << 63;
    }
    0
}

/// Bytes each destination drive must take for a restore: items whose bin
/// (`id`) sits on another drive than their original path are copied, so
/// their size counts against that drive. Items on the same drive move.
pub fn cross_drive_needs(items: &[(PathBuf, PathBuf, u64)]) -> Vec<(PathBuf, u64)> {
    cross_volume_needs(items, drive_root_of)
}

/// [`cross_drive_needs`] with the volume of each path from `root_of`: a
/// volume mounted on a folder counts apart from the drive above it, so a
/// restore onto it checks its own free space.
pub fn cross_volume_needs(
    items: &[(PathBuf, PathBuf, u64)],
    root_of: impl Fn(&Path) -> Option<PathBuf>,
) -> Vec<(PathBuf, u64)> {
    let mut out: std::collections::BTreeMap<PathBuf, u64> = std::collections::BTreeMap::new();
    for (bin, dest, size) in items {
        let (Some(from), Some(to)) = (root_of(bin), root_of(dest)) else {
            continue;
        };
        if from != to {
            let e = out.entry(to).or_default();
            *e = e.saturating_add(*size);
        }
    }
    out.into_iter().collect()
}

/// The age and size filters the Recycle Bin `list`, `status` and `rm`
/// take, with the FreeDesktop commands' flags and meanings.
#[derive(Debug, Default)]
pub struct BinFilters {
    pub age: crate::filters::AgeFilter,
    pub size: crate::filters::SizeFilter,
}

impl BinFilters {
    pub fn is_set(&self) -> bool {
        self.age.is_set() || self.size.is_set()
    }

    /// The items that pass: `when` gives an item's deletion time (Unix
    /// seconds), `size` its bytes, asked only when a size bound is set.
    pub fn apply<T>(
        &self,
        items: Vec<T>,
        when: impl Fn(&T) -> i64,
        size: impl Fn(&T) -> u64,
    ) -> Vec<T> {
        let now = crate::util::now_epoch();
        items
            .into_iter()
            .filter(|it| self.age.keeps_at(Some(when(it)), now))
            .filter(|it| !self.size.is_set() || self.size.keeps(size(it)))
            .collect()
    }
}

/// Parse the filter flags out of `args`, handing every other argument to
/// `other`, which returns an exit status to stop with. A bad filter
/// value prints its message and stops with 2.
pub fn parse_with_filters(
    prog: &str,
    args: &[String],
    mut other: impl FnMut(&str) -> Option<i32>,
) -> Result<BinFilters, i32> {
    let mut f = BinFilters::default();
    let mut i = 0usize;
    while i < args.len() {
        let taken = f
            .age
            .take(args, &mut i)
            .or_else(|| f.size.take(args, &mut i));
        match taken {
            Some(Ok(())) => {}
            Some(Err(e)) => {
                eprintln!("{prog}: {e}");
                return Err(2);
            }
            None => {
                if let Some(code) = other(&args[i]) {
                    return Err(code);
                }
            }
        }
        i += 1;
    }
    if let Err(e) = f.size.check() {
        eprintln!("{prog}: {e}");
        return Err(2);
    }
    Ok(f)
}

/// One Recycle Bin item, as trash-stats shows it.
#[cfg(windows)]
pub struct BinStat {
    pub name: String,
    pub original: PathBuf,
    pub epoch: i64,
    pub bytes: u64,
    /// Files and directories in its `$R` payload.
    pub files: u64,
}

/// Indices of `items` (deletion time, bytes, files), oldest first, to
/// purge until at least `need` (bytes, files) goes; all of them when that
/// is more than they hold.
pub fn pick_oldest(items: &[(i64, u64, u64)], need: (u64, u64)) -> Vec<usize> {
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by_key(|&i| items[i].0);
    let (mut bytes, mut files) = (0u64, 0u64);
    order
        .into_iter()
        .take_while(|&i| {
            let more = bytes < need.0 || files < need.1;
            bytes = bytes.saturating_add(items[i].1);
            files = files.saturating_add(items[i].2);
            more
        })
        .collect()
}

/// The `empty --json` summary, in the Unix fields: whole days as an
/// integer, any other age as a fraction, and the exact seconds too.
pub fn empty_summary_json(
    dry_run: bool,
    removed: u64,
    bytes: u64,
    older_secs: Option<i64>,
    max_size: Option<u64>,
) -> String {
    let days = match older_secs {
        Some(s) if s % 86_400 == 0 => (s / 86_400).to_string(),
        Some(s) => format!("{}", s as f64 / 86_400.0),
        None => "null".into(),
    };
    let opt = |v: Option<String>| v.unwrap_or_else(|| "null".into());
    format!(
        r#"{{"dry_run":{dry_run},"removed":{removed},"bytes":{bytes},"days":{days},"older_than_secs":{},"max_size":{}}}"#,
        opt(older_secs.map(|s| s.to_string())),
        opt(max_size.map(|m| m.to_string()))
    )
}

pub use crate::budget::pick_budget;

/// What a Recycle Bin item takes, from its `$R` payload path: the size
/// its `$I` record holds (Windows writes a folder's total there), else
/// the bytes of the files under the payload.
pub fn recycled_size(payload: &Path) -> u64 {
    index_file_for(payload)
        .and_then(|i| std::fs::read(i).ok())
        .and_then(|b| parse_recycle_i(&b))
        .map(|m| m.size)
        .unwrap_or_else(|| tree_bytes(payload))
}

/// Files and directories at or below `root`, without following links.
#[cfg(windows)]
fn tree_files(root: &Path) -> u64 {
    let mut n = 0u64;
    let mut todo = vec![root.to_path_buf()];
    while let Some(p) = todo.pop() {
        let Ok(md) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        n += 1;
        if md.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&p) {
                todo.extend(rd.flatten().map(|e| e.path()));
            }
        }
    }
    n
}

/// Bytes of the files under `root` (or of `root` itself), without
/// following links, walked with a stack.
fn tree_bytes(root: &Path) -> u64 {
    let mut total = 0u64;
    let mut todo = vec![root.to_path_buf()];
    while let Some(p) = todo.pop() {
        let Ok(md) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if md.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&p) {
                todo.extend(rd.flatten().map(|e| e.path()));
            }
        } else {
            total = total.saturating_add(md.len());
        }
    }
    total
}

/// Remove `path`, clearing the read-only attribute once and retrying when
/// the delete is refused. A symlink is removed as a link. A directory on
/// another volume is left in place and reported.
#[cfg(windows)]
pub(crate) fn remove_forcing(path: &Path) -> io::Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let ft = meta.file_type();
    if ft.is_symlink() || !ft.is_dir() {
        return remove_retry(path, false);
    }
    let root_vol = volume_serial(path);
    let mut stack = vec![WinFrame {
        path: path.to_path_buf(),
        kids: Vec::new(),
        listed: false,
    }];
    loop {
        let top = stack.len() - 1;
        if !stack[top].listed {
            match std::fs::read_dir(&stack[top].path) {
                Ok(rd) => {
                    stack[top].kids = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
                    stack[top].listed = true;
                }
                Err(e) if access_denied(&e) && clear_readonly(&stack[top].path) => {
                    let rd = std::fs::read_dir(&stack[top].path)?;
                    stack[top].kids = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
                    stack[top].listed = true;
                }
                Err(e) => return Err(e),
            }
            continue;
        }
        if let Some(child) = stack[top].kids.pop() {
            match std::fs::symlink_metadata(&child) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
                Ok(m) => {
                    let ft = m.file_type();
                    if ft.is_symlink() || !ft.is_dir() {
                        remove_retry(&child, false)?;
                        continue;
                    }
                    if root_vol.is_some()
                        && volume_serial(&child).is_some_and(|v| Some(v) != root_vol)
                    {
                        return Err(io::Error::other(
                            "refusing to enter a directory on another device",
                        ));
                    }
                    stack.push(WinFrame {
                        path: child,
                        kids: Vec::new(),
                        listed: false,
                    });
                }
            }
            continue;
        }
        let done = stack.pop().expect("frame");
        remove_retry(&done.path, true)?;
        if stack.is_empty() {
            return Ok(());
        }
    }
}

#[cfg(windows)]
struct WinFrame {
    path: PathBuf,
    kids: Vec<PathBuf>,
    listed: bool,
}

#[cfg(windows)]
fn access_denied(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::PermissionDenied
}

#[cfg(windows)]
fn clear_readonly(path: &Path) -> bool {
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileAttributesW, SetFileAttributesW, FILE_ATTRIBUTE_READONLY, INVALID_FILE_ATTRIBUTES,
    };
    let wide = wide(path.as_os_str());
    let attrs = unsafe { GetFileAttributesW(wide.as_ptr()) };
    if attrs == INVALID_FILE_ATTRIBUTES || attrs & FILE_ATTRIBUTE_READONLY == 0 {
        return false;
    }
    unsafe { SetFileAttributesW(wide.as_ptr(), attrs & !FILE_ATTRIBUTE_READONLY) != 0 }
}

#[cfg(windows)]
fn remove_retry(path: &Path, dir: bool) -> io::Result<()> {
    let once = || {
        if dir {
            std::fs::remove_dir(path)
        } else {
            std::fs::remove_file(path).or_else(|_| std::fs::remove_dir(path))
        }
    };
    match once() {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) if access_denied(&e) => {
            let _ = clear_readonly(path);
            if let Some(parent) = path.parent() {
                let _ = clear_readonly(parent);
            }
            match once() {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

#[cfg(windows)]
fn volume_serial(path: &Path) -> Option<u32> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .ok()?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) };
    (ok != 0).then_some(info.dwVolumeSerialNumber)
}

// ---- Windows CLI surface (system Recycle Bin via `trash` crate) ----

#[cfg(windows)]
mod os {
    use super::*;
    use crate::filters::AgeFilter;
    use trash::os_limited;
    use trash::TrashItem;

    fn err_msg(e: impl std::fmt::Display) -> String {
        e.to_string()
    }

    /// Where `it` came from, as its `$I` record holds it. The shell's
    /// display name, which `TrashItem::original_path` builds on, hides
    /// the extension of known file types, so `a.txt` would show as `a`.
    fn original_of(it: &TrashItem) -> PathBuf {
        index_file_for(Path::new(&it.id))
            .and_then(|i| std::fs::read(i).ok())
            .and_then(|b| parse_recycle_i(&b))
            .map(|m| m.original)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| it.original_path())
    }

    fn item_size(it: &TrashItem) -> u64 {
        recycled_size(Path::new(&it.id))
    }

    pub fn put_run(prog: &str, args: &[String]) -> i32 {
        let mut force = false;
        let mut verbose = false;
        let mut files: Vec<PathBuf> = Vec::new();
        let mut plain = false;
        let mut json = false;
        let mut pinned = false;
        let mut no_more_opts = false;
        let mut in_use: Option<String> = None;
        for arg in args {
            if no_more_opts || arg == "-" || !arg.starts_with('-') {
                files.push(PathBuf::from(arg));
                continue;
            }
            match arg.as_str() {
                "--" => no_more_opts = true,
                "-f" | "--force" => force = true,
                "-v" | "--verbose" => verbose = true,
                "--plain" => plain = true,
                "--json" => json = true,
                // FreeDesktop options: the Recycle Bin has one place per
                // drive, so there is nothing to pin or narrow down.
                "--home-only" => {}
                a if a.starts_with("--trash-dir=") => pinned = true,
                a if a.starts_with("--in-use=") => {
                    let v = &a["--in-use=".len()..];
                    if !matches!(v, "warn" | "refuse" | "ignore") {
                        eprintln!("{prog}: invalid argument '{v}' for '--in-use' (want warn, refuse or ignore)");
                        return 2;
                    }
                    in_use = Some(v.to_string());
                }
                "-h" | "--help" => {
                    print!(
                        "\
Usage: {prog} [OPTION]... [FILE]...
Move FILE(s) to the Windows Recycle Bin (system trash; not FreeDesktop).

  -f, --force    ignore nonexistent files
  -v, --verbose  print each path and the Recycle Bin it went to
      --plain    accepted for CLI parity (no TUI put browser on Windows)
      --json     print a JSON summary on stdout ({{trashed, failed}});
                   replaces -v output
      --in-use=WHEN  when another process has a file open (as the
                   Restart Manager reports it): warn (the default),
                   refuse, or ignore
      --trash-dir=PATH, --home-only
                 accepted for script parity; items always go to the
                   Recycle Bin of their drive
      --help     display this help and exit
      --version  output version information and exit
"
                    );
                    return 0;
                }
                "--version" => {
                    println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                    return 0;
                }
                a if a.starts_with('-') && a.len() > 1 => {
                    // Accept common rm flags as no-ops / best-effort for multi-call.
                    if a.chars().all(|c| "rRfidv".contains(c) || c == '-') {
                        if a.contains('f') {
                            force = true;
                        }
                        if a.contains('v') {
                            verbose = true;
                        }
                        continue;
                    }
                    eprintln!("{prog}: unrecognized option '{a}'");
                    return 2;
                }
                a => files.push(PathBuf::from(a)),
            }
        }
        let _ = plain;
        if pinned {
            eprintln!("{prog}: --trash-dir has no effect on Windows; using the Recycle Bin");
        }
        if files.is_empty() {
            if force {
                if json {
                    println!("{}", util::put_summary_json(&[], 0));
                }
                return 0;
            }
            eprintln!("{prog}: missing operand");
            eprintln!("Try '{prog} --help' for more information.");
            return 2;
        }
        let in_use = in_use.unwrap_or_else(|| in_use_default(prog));
        let snap =
            (in_use != "ignore").then(|| crate::holders::Snapshot::scan_under(&files, false));
        let mut status = 0;
        let mut failed = 0usize;
        let mut moved: Vec<PathBuf> = Vec::new();
        let mut roots: Vec<PathBuf> = Vec::new();
        for f in &files {
            if std::fs::symlink_metadata(f).is_err() {
                if force {
                    continue;
                }
                eprintln!(
                    "{prog}: cannot remove '{}': No such file or directory",
                    f.display()
                );
                status = status.max(1);
                failed += 1;
                continue;
            }
            let abs = std::fs::canonicalize(f)
                .map(|p| strip_verbatim(&p))
                .unwrap_or_else(|_| f.clone());
            if let Some(snap) = &snap {
                let holds = snap.under(f);
                if !holds.is_empty() {
                    let who = crate::holders::describe(&holds);
                    if in_use == "refuse" {
                        eprintln!("{prog}: cannot trash '{}': in use by {who}", f.display());
                        eprintln!("{prog}: use --in-use=warn to trash it anyway");
                        status = status.max(1);
                        failed += 1;
                        continue;
                    }
                    eprintln!("{prog}: warning: '{}' is in use by {who}", f.display());
                }
            }
            let root = test_bin_root().or_else(|| bin_root_of(&abs));
            let bin = verbose.then(|| root.clone()).flatten();
            match trash::delete(f) {
                Ok(()) => {
                    if verbose && !json {
                        println!(
                            "{}",
                            removed_line(&crate::util::term_path(f), bin.as_deref())
                        );
                    }
                    moved.push(abs);
                    if let Some(r) = root {
                        if !roots.contains(&r) {
                            roots.push(r);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("{prog}: cannot trash '{}': {}", f.display(), err_msg(e));
                    status = status.max(1);
                    failed += 1;
                }
            }
        }
        if json {
            let found = locate_trashed(&moved);
            let items: Vec<(&Path, &str, &Path)> = found
                .iter()
                .map(|(o, n, d)| (o.as_path(), n.as_str(), d.as_path()))
                .collect();
            println!("{}", util::put_summary_json(&items, failed));
        }
        if !moved.is_empty() {
            prune_after_put(prog, &moved, json);
        }
        // The Recycle Bin keeps items on their own volume, so a full volume
        // stays full.
        for r in &roots {
            let what = format!("the Recycle Bin of {}", r.display());
            if let Some(w) = crate::space::low_warning(&what, r) {
                eprintln!("{prog}: warning: {w}");
            }
        }
        status
    }

    /// Test seam, in debug builds only: `RTRASH_TEST_BIN_ROOT=DIR` names
    /// DIR as the volume root of every put, so a test can pair it with
    /// `RTRASH_TEST_FS` and fake a short volume.
    fn test_bin_root() -> Option<PathBuf> {
        if !cfg!(debug_assertions) {
            return None;
        }
        std::env::var_os("RTRASH_TEST_BIN_ROOT").map(PathBuf::from)
    }

    /// `RTRASH_IN_USE`, else `policy.in_use`, else warn.
    fn in_use_default(prog: &str) -> String {
        let ok = |v: &str| matches!(v, "warn" | "refuse" | "ignore");
        if let Ok(v) = std::env::var("RTRASH_IN_USE") {
            if ok(&v) {
                return v;
            }
            eprintln!(
                "{prog}: warning: ignoring RTRASH_IN_USE='{v}' (want warn, refuse or ignore)"
            );
        }
        match crate::config::get().str("policy", "in_use") {
            Some(v) if ok(v) => v.to_string(),
            _ => "warn".to_string(),
        }
    }

    /// For each moved path, its name and folder in the Recycle Bin: the
    /// newest item with that original path. The shell API does not return
    /// them from the move itself.
    fn locate_trashed(moved: &[PathBuf]) -> Vec<(PathBuf, String, PathBuf)> {
        let items = os_limited::list().unwrap_or_default();
        moved
            .iter()
            .map(|orig| {
                let hit = items
                    .iter()
                    .filter(|it| original_of(it) == *orig)
                    .max_by_key(|it| it.time_deleted);
                let (name, dir) = match hit {
                    Some(it) => {
                        let id = PathBuf::from(&it.id);
                        let name = id
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| it.name.to_string_lossy().into_owned());
                        (name, id.parent().map(Path::to_path_buf).unwrap_or_default())
                    }
                    None => (String::new(), PathBuf::new()),
                };
                (orig.clone(), name, dir)
            })
            .collect()
    }

    /// `C:\\...` for a verbatim `\\?\C:\\...` path from canonicalize.
    fn strip_verbatim(p: &Path) -> PathBuf {
        let s = p.to_string_lossy();
        match s.strip_prefix(r"\\?\") {
            Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
            _ => p.to_path_buf(),
        }
    }

    pub fn list_run(prog: &str, args: &[String]) -> i32 {
        let filters = parse_with_filters(prog, args, |arg| match arg {
            "--help" => {
                print!(
                    "\
Usage: {prog} [OPTION]...
List items in the Windows Recycle Bin (system trash).

      --older-than=DAYS only items deleted more than DAYS days ago
      --newer-than=DAYS only items deleted within the last DAYS days
      --larger-than=SIZE  only items over SIZE (500M, 1.5G, 64k)
      --smaller-than=SIZE only items under SIZE
      --help     display this help and exit
      --version  output version information and exit

Note: --home-only / --trash-dir are FreeDesktop options and are ignored
on Windows (there is no FreeDesktop layout here).
"
                );
                Some(0)
            }
            "--version" => {
                println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                Some(0)
            }
            "--home-only" => None,
            a if a.starts_with("--trash-dir=") => None,
            a => {
                eprintln!("{prog}: unrecognized option '{a}'");
                Some(2)
            }
        });
        let filters = match filters {
            Ok(f) => f,
            Err(code) => return code,
        };
        match os_limited::list() {
            Ok(items) => {
                let mut items = filters.apply(items, |it| it.time_deleted, item_size);
                items.sort_by_key(|i| i.time_deleted);
                for it in items {
                    let date = platform::epoch_to_iso_utc(it.time_deleted);
                    println!(
                        "{}",
                        platform::format_deletion_list_line(Some(&date), &original_of(&it))
                    );
                }
                0
            }
            Err(e) => {
                eprintln!("{prog}: cannot list Recycle Bin: {}", err_msg(e));
                1
            }
        }
    }

    pub fn empty_run(prog: &str, args: &[String]) -> i32 {
        let mut dry_run = false;
        let mut verbose = false;
        let mut json = false;
        let mut until: Option<(String, crate::reap::Floor)> = None;
        let mut max_size: Option<u64> = None;
        let mut largest = false;
        let mut age = AgeFilter::default();
        let mut i = 0usize;
        while i < args.len() {
            match age.take(args, &mut i) {
                // As on Linux, `empty` takes no --newer-than.
                Some(_) if age.newer_than.is_some() => {
                    eprintln!("{prog}: unrecognized option '--newer-than'");
                    return 2;
                }
                Some(Ok(())) => {
                    i += 1;
                    continue;
                }
                Some(Err(e)) => {
                    eprintln!("{prog}: {e}");
                    return 2;
                }
                None => {}
            }
            let arg = &args[i];
            i += 1;
            match arg.as_str() {
                "-n" | "--dry-run" => dry_run = true,
                "-v" | "--verbose" => verbose = true,
                "-f" | "--force" | "--plain" | "--home-only" => {}
                "--json" => json = true,
                a if a.starts_with("--max-size=") => {
                    match crate::filters::parse_size(&a["--max-size=".len()..]) {
                        Some(n) => max_size = Some(n),
                        None => {
                            eprintln!("{prog}: invalid SIZE in '{a}'");
                            return 2;
                        }
                    }
                }
                "--prune=oldest" => largest = false,
                "--prune=largest" => largest = true,
                a if a.starts_with("--prune=") => {
                    eprintln!("{prog}: --prune takes oldest or largest");
                    return 2;
                }
                a if a.starts_with("--until-free=") => {
                    let v = &a["--until-free=".len()..];
                    match crate::reap::Floor::parse(v) {
                        Some(f) => until = Some((v.to_string(), f)),
                        None => {
                            eprintln!("{prog}: invalid SPEC in '{a}' (a percent such as 10%, a size such as 20G)");
                            return 2;
                        }
                    }
                }
                "--help" => {
                    print!(
                        "\
Usage: {prog} [OPTION]... [DAYS]
Permanently empty the Windows Recycle Bin (system trash).
With DAYS, only items deleted more than DAYS days ago, as on Linux.

  -n, --dry-run  list what would be removed
  -v, --verbose  print each removed original path
      --older-than=AGE  same as DAYS; AGE may also be a duration such as
                   2h, 30m or 1d12h
      --until-free=SPEC  on each drive under SPEC free (a percent such as
                   10% or a size such as 20G), remove the oldest items of
                   its Recycle Bin until it is back at SPEC; exits 1 when
                   one stays short
      --max-size=SIZE  remove items until the Recycle Bins together fit
                   in SIZE, such as 500M or 10G; with DAYS, the age cut
                   runs first
      --prune=oldest|largest  which items --max-size removes first
                   (default oldest)
      --json     print a JSON summary ({{dry_run, removed, bytes, days,
                   older_than_secs, max_size}}; with --until-free, the
                   per-drive summary)
      --help     display this help and exit
      --version  output version information and exit

FreeDesktop --trash-dir pins do not apply to the Recycle Bin.
"
                    );
                    return 0;
                }
                "--version" => {
                    println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                    return 0;
                }
                a if a.starts_with("--trash-dir=") => {}
                a if !a.starts_with('-') => match crate::filters::parse_age_secs(a) {
                    Ok(d) => age.older_than = Some(d),
                    Err(e) => {
                        eprintln!("{prog}: {e}");
                        return 2;
                    }
                },
                a => {
                    eprintln!("{prog}: unrecognized option '{a}'");
                    return 2;
                }
            }
        }
        if let Some((spec, floor)) = until {
            if age.older_than.is_some() || max_size.is_some() {
                eprintln!("{prog}: --until-free takes no DAYS or --max-size");
                return 2;
            }
            let roots: Vec<PathBuf> = bin_items().into_iter().map(|(b, _)| b).collect();
            return crate::reap::free_until(prog, &roots, &spec, floor, dry_run, verbose, json);
        }
        let all = match os_limited::list() {
            Ok(i) => i,
            Err(e) => {
                eprintln!("{prog}: cannot list Recycle Bin: {}", err_msg(e));
                return 1;
            }
        };
        // DAYS alone, or nothing, takes by age (everything without DAYS);
        // a budget then takes the oldest or largest of the rest.
        let age_pass = age.older_than.is_some() || max_size.is_none();
        let (mut items, rest): (Vec<TrashItem>, Vec<TrashItem>) = all
            .into_iter()
            .partition(|it| age_pass && age.keeps(Some(it.time_deleted)));
        if let Some(max) = max_size {
            let sized: Vec<(i64, u64)> = rest
                .iter()
                .map(|it| (it.time_deleted, recycled_size(Path::new(&it.id))))
                .collect();
            for i in pick_budget(&sized, max, largest) {
                items.push(rest[i].clone());
            }
        }
        let n = items.len() as u64;
        let summary = |bytes_of: &[TrashItem]| {
            let bytes = bytes_of
                .iter()
                .map(|it| recycled_size(Path::new(&it.id)))
                .sum();
            println!(
                "{}",
                empty_summary_json(dry_run, n, bytes, age.older_than, max_size)
            );
        };
        if dry_run {
            if json {
                summary(&items);
                return 0;
            }
            for it in &items {
                println!("would remove {}", crate::util::term_path(&original_of(it)));
            }
            println!("{} item(s)", items.len());
            return 0;
        }
        if verbose && !json {
            for it in &items {
                println!("{}", crate::util::term_path(&original_of(it)));
            }
        }
        // Sizes come from the records, so read them before the purge.
        if json {
            summary(&items);
        }
        match os_limited::purge_all(items) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("{prog}: cannot empty Recycle Bin: {}", err_msg(e));
                1
            }
        }
    }

    /// With a trash budget (`RTRASH_MAX_SIZE` or `policy.max_size`), trim
    /// the Recycle Bins together back under it, never touching the items
    /// this put moved (matched by original path); they still count.
    fn prune_after_put(prog: &str, moved: &[PathBuf], quiet: bool) {
        let Some((max, policy, source)) = crate::budget::budget(prog) else {
            return;
        };
        let Ok(all) = os_limited::list() else {
            return;
        };
        let rows: Vec<(PathBuf, i64, u64)> = all
            .iter()
            .map(|it| {
                (
                    original_of(it),
                    it.time_deleted,
                    recycled_size(Path::new(&it.id)),
                )
            })
            .collect();
        let picked = crate::budget::trim_after_put(&rows, moved, max, policy);
        if picked.is_empty() {
            return;
        }
        let n = picked.len() as u64;
        let chosen: Vec<TrashItem> = picked.iter().map(|&i| all[i].clone()).collect();
        match os_limited::purge_all(chosen) {
            Ok(()) if !quiet => eprintln!(
                "{prog}: removed {} from the Recycle Bins to keep them under {} ({source})",
                crate::util::plural(n, "item", "items"),
                crate::util::format_bytes(max)
            ),
            Ok(()) => {}
            Err(e) => eprintln!("{prog}: cannot trim the Recycle Bin: {}", err_msg(e)),
        }
    }

    /// Purge the oldest items in the Recycle Bin of `drive` until `need`
    /// (bytes, files) goes, and return (items, bytes, files) taken.
    pub fn purge_oldest_on(
        prog: &str,
        at: &Path,
        need: (u64, u64),
        dry_run: bool,
        verbose: bool,
    ) -> (u64, u64, u64) {
        let items: Vec<TrashItem> = match os_limited::list() {
            Ok(all) => {
                let ids: Vec<PathBuf> = all.iter().map(|it| PathBuf::from(&it.id)).collect();
                on_volume(&ids, volume_key, volume_key(at))
                    .into_iter()
                    .map(|i| all[i].clone())
                    .collect()
            }
            Err(e) => {
                eprintln!("{prog}: cannot list Recycle Bin: {}", err_msg(e));
                return (0, 0, 0);
            }
        };
        let sized: Vec<(i64, u64, u64)> = items
            .iter()
            .map(|it| {
                let p = Path::new(&it.id);
                (it.time_deleted, recycled_size(p), tree_files(p))
            })
            .collect();
        let picked = pick_oldest(&sized, need);
        let (bytes, files) = picked
            .iter()
            .fold((0u64, 0u64), |(b, f), &i| (b + sized[i].1, f + sized[i].2));
        let chosen: Vec<TrashItem> = picked.iter().map(|&i| items[i].clone()).collect();
        if verbose || dry_run {
            for it in &chosen {
                println!(
                    "{} {}",
                    if dry_run { "would remove" } else { "removed" },
                    crate::util::term_path(&original_of(it))
                );
            }
        }
        let n = chosen.len() as u64;
        if !dry_run && n > 0 {
            if let Err(e) = os_limited::purge_all(chosen) {
                eprintln!("{prog}: cannot empty Recycle Bin: {}", err_msg(e));
                return (0, 0, 0);
            }
        }
        (n, bytes, files)
    }

    /// Every item, grouped by the Recycle Bin (`X:\\$Recycle.Bin`) of its
    /// drive, in drive order.
    pub fn bin_items() -> Vec<(PathBuf, Vec<BinStat>)> {
        let mut out: Vec<(PathBuf, Vec<BinStat>)> = Vec::new();
        for it in os_limited::list().unwrap_or_default() {
            let p = Path::new(&it.id);
            let Some(drive) = bin_root_of(p) else {
                continue;
            };
            let bin = drive.join("$Recycle.Bin");
            let item = BinStat {
                name: it.name.to_string_lossy().into_owned(),
                original: original_of(&it),
                epoch: it.time_deleted,
                bytes: recycled_size(p),
                files: tree_files(p),
            };
            match out.iter_mut().find(|(b, _)| *b == bin) {
                Some((_, v)) => v.push(item),
                None => out.push((bin, vec![item])),
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    pub fn restore_run(prog: &str, args: &[String]) -> i32 {
        let mut force = false;
        let mut plain = false;
        let mut newest_first = false;
        let mut target: Option<PathBuf> = None;
        for arg in args {
            match arg.as_str() {
                "-f" | "--force" => force = true,
                "--plain" | "--cwd-only" | "--home-only" => plain = true,
                "--newest-first" => newest_first = true,
                "--oldest-first" => newest_first = false,
                "--no-space-check" => crate::space::skip_check(true),
                "--help" => {
                    print!(
                        "\
Usage: {prog} [OPTION]... [PATH]
Restore an item from the Windows Recycle Bin to its original location.

With PATH, restore the item whose original path equals PATH.
Without PATH, print a numbered list and read an index from stdin.

  -f, --force   overwrite when the destination exists (best-effort)
      --plain   numbered selection instead of the browser
      --newest-first  list the most recently deleted item first
      --oldest-first  list the oldest deleted item first (default)
      --no-space-check  restore onto another drive even when it would
                leave less than 5% (or RTRASH_MIN_FREE) free
      --help    display this help and exit
      --version output version information and exit
"
                    );
                    return 0;
                }
                "--version" => {
                    println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                    return 0;
                }
                a if a.starts_with("--trash-dir=") => {}
                a if a.starts_with('-') && a.len() > 1 => {
                    eprintln!("{prog}: unrecognized option '{a}'");
                    return 2;
                }
                a => {
                    if target.is_some() {
                        eprintln!("{prog}: only one PATH may be given");
                        return 2;
                    }
                    target = Some(PathBuf::from(a));
                }
            }
        }
        let items = match os_limited::list() {
            Ok(i) => i,
            Err(e) => {
                eprintln!("{prog}: cannot list Recycle Bin: {}", err_msg(e));
                return 1;
            }
        };
        let mut matches: Vec<TrashItem> = match &target {
            Some(t) => {
                let abs = std::fs::canonicalize(t).unwrap_or_else(|_| t.clone());
                items
                    .into_iter()
                    .filter(|i| original_of(i) == abs || original_of(i) == *t)
                    .collect()
            }
            None => items,
        };
        if matches.is_empty() {
            eprintln!("{prog}: no trashed files match");
            return 1;
        }
        if matches.len() == 1 {
            return restore_one(prog, matches, force);
        }
        matches.sort_by_key(|it| it.time_deleted);
        if newest_first {
            matches.reverse();
        }
        #[cfg(feature = "tui")]
        if !plain && util::stdin_is_tty() {
            let items: Vec<BinItem> = matches.into_iter().map(BinItem::new).collect();
            let refs: Vec<&BinItem> = items.iter().collect();
            return crate::restore_tui::run(prog, refs, force, newest_first);
        }
        let _ = plain;
        // Numbered pick.
        for (i, it) in matches.iter().enumerate() {
            let date = platform::epoch_to_iso_utc(it.time_deleted);
            println!(
                "  {i} {}",
                platform::format_deletion_list_line(Some(&date), &original_of(it))
            );
        }
        eprint!("What to restore [0..{}]: ", matches.len() - 1);
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return 1;
        }
        let Ok(idx) = line.trim().parse::<usize>() else {
            eprintln!("{prog}: invalid index");
            return 1;
        };
        if idx >= matches.len() {
            eprintln!("{prog}: index out of range");
            return 1;
        }
        let item = matches.into_iter().nth(idx).unwrap();
        restore_one(prog, vec![item], force)
    }

    /// A Recycle Bin item as the restore browser sees it.
    #[cfg(feature = "tui")]
    pub struct BinItem {
        item: TrashItem,
        original: PathBuf,
        date: String,
    }

    #[cfg(feature = "tui")]
    impl BinItem {
        pub fn new(item: TrashItem) -> Self {
            let original = original_of(&item);
            let date = platform::epoch_to_iso_utc(item.time_deleted);
            Self {
                item,
                original,
                date,
            }
        }
    }

    #[cfg(feature = "tui")]
    impl crate::restore_tui::TuiItem for BinItem {
        fn original(&self) -> &Path {
            &self.original
        }
        fn date(&self) -> Option<&str> {
            Some(&self.date)
        }
        fn restore(&self, force: bool) -> Result<(PathBuf, Vec<String>), String> {
            let dest = &self.original;
            if dest.symlink_metadata().is_ok() {
                if !force {
                    return Err(format!(
                        "refusing to overwrite '{}' (use -f)",
                        dest.display()
                    ));
                }
                super::remove_forcing(dest)
                    .map_err(|e| format!("cannot replace '{}': {e}", dest.display()))?;
            }
            os_limited::restore_all(vec![self.item.clone()])
                .map(|()| (dest.clone(), Vec::new()))
                .map_err(|e| format!("restore failed: {}", err_msg(e)))
        }
    }

    fn restore_one(prog: &str, items: Vec<TrashItem>, force: bool) -> i32 {
        for it in &items {
            let dest = original_of(it);
            if dest.exists() && !force {
                eprintln!(
                    "{prog}: refusing to overwrite '{}' (use -f)",
                    dest.display()
                );
                return 1;
            }
            if dest.exists() && force {
                let _ = super::remove_forcing(&dest);
            }
        }
        let pairs: Vec<(PathBuf, PathBuf, u64)> = items
            .iter()
            .map(|it| (PathBuf::from(&it.id), original_of(it), item_size(it)))
            .collect();
        for (dest, need) in super::cross_volume_needs(&pairs, super::bin_root_of) {
            if let Err(e) = crate::space::guard_bytes(need, &dest) {
                eprintln!("{prog}: cannot restore: {e}");
                return 1;
            }
        }
        match os_limited::restore_all(items) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("{prog}: restore failed: {}", err_msg(e));
                1
            }
        }
    }

    pub fn status_run(prog: &str, args: &[String]) -> i32 {
        let filters = parse_with_filters(prog, args, |arg| match arg {
            "--help" => {
                print!(
                    "\
Usage: {prog} [OPTION]...
Summarize the Windows Recycle Bin: items, size, and the cap of each
drive and each volume mounted on a folder (MaxCapacity in the
registry), then the total and the free space on each (a warning under
10% free).

      --older-than=DAYS only count items deleted more than DAYS days ago
      --newer-than=DAYS only count items deleted within the last DAYS days
      --larger-than=SIZE  only count items over SIZE (500M, 1.5G, 64k)
      --smaller-than=SIZE only count items under SIZE
                     (a filter counts item by item, without the caps)
      --help     display this help and exit
      --version  output version information and exit
"
                );
                Some(0)
            }
            "--version" => {
                println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                Some(0)
            }
            "--home-only" => None,
            a if a.starts_with("--trash-dir=") => None,
            a => {
                eprintln!("{prog}: unrecognized option '{a}'");
                Some(2)
            }
        });
        let filters = match filters {
            Ok(f) => f,
            Err(code) => return code,
        };
        if filters.is_set() {
            return filtered_status(prog, &filters);
        }
        // One line per volume with a bin or a cap, like the per-trash lines
        // of the FreeDesktop status, then the total. A volume mounted on a
        // folder keeps a Recycle Bin of its own, so it gets a line too.
        let (mut items, mut bytes) = (0u64, 0u64);
        for root in crate::trash_history::bin_roots() {
            let Some((n, b)) = query_bin(&root) else {
                continue;
            };
            let quota = bin_quota(&root);
            let note = cap_note(b, &quota);
            if n == 0 && note.is_empty() {
                continue;
            }
            items += n;
            bytes += b;
            let mut line = format!(
                "{}\t{}\t{}",
                root.display(),
                crate::util::plural(n, "item", "items"),
                crate::util::format_bytes(b)
            );
            if !note.is_empty() {
                line.push('\t');
                line.push_str(&note);
            }
            println!("{line}");
        }
        println!(
            "Total: {} ({})",
            crate::util::plural(items, "item", "items"),
            crate::util::format_bytes(bytes)
        );
        print_free(&crate::trash_history::bin_roots());
        0
    }

    /// A `Free on` line per drive, and a warning for a drive under
    /// [`crate::space::WARN_FREE_PERCENT`] free.
    fn print_free(drives: &[PathBuf]) {
        for root in drives {
            let Ok(s) = crate::space::query(root) else {
                continue;
            };
            println!(
                "Free on {}: {}",
                root.display(),
                crate::space::free_line(&s)
            );
            let lows = s.lows(crate::space::WARN_FREE_PERCENT);
            if !lows.is_empty() {
                eprintln!(
                    "rtrash: warning: {} is low: {}; free space with `rtrash empty --until-free=10%` (only the oldest items it needs) or `rtrash empty`",
                    root.display(),
                    lows.join(", ")
                );
            }
        }
    }

    /// `status` with a filter: the shell's per-drive totals cannot be
    /// filtered, so count the passing items, by the volume of their bin.
    fn filtered_status(prog: &str, filters: &BinFilters) -> i32 {
        let items = match os_limited::list() {
            Ok(i) => filters.apply(i, |it| it.time_deleted, item_size),
            Err(e) => {
                eprintln!("{prog}: cannot list Recycle Bin: {}", err_msg(e));
                return 1;
            }
        };
        // By volume, so items in the bin of a volume mounted on a folder
        // count under that folder, as trash-stats groups them.
        let mut by_drive: std::collections::BTreeMap<PathBuf, (u64, u64)> =
            std::collections::BTreeMap::new();
        for it in &items {
            let root = bin_root_of(Path::new(&it.id)).unwrap_or_default();
            let e = by_drive.entry(root).or_default();
            e.0 += 1;
            e.1 = e.1.saturating_add(item_size(it));
        }
        let (mut items, mut bytes) = (0u64, 0u64);
        for (root, (n, b)) in &by_drive {
            items += n;
            bytes = bytes.saturating_add(*b);
            println!(
                "{}\t{}\t{}",
                root.display(),
                crate::util::plural(*n, "item", "items"),
                crate::util::format_bytes(*b)
            );
        }
        println!(
            "Total: {} ({})",
            crate::util::plural(items, "item", "items"),
            crate::util::format_bytes(bytes)
        );
        print_free(&crate::trash_history::bin_roots());
        0
    }

    pub fn rm_run(prog: &str, args: &[String]) -> i32 {
        let mut dry_run = false;
        let mut force = false;
        let mut verbose = false;
        let mut patterns: Vec<String> = Vec::new();
        let filters = parse_with_filters(prog, args, |arg| {
            match arg {
                "-n" | "--dry-run" => dry_run = true,
                "-f" | "--force" => force = true,
                "-v" | "--verbose" => verbose = true,
                "--home-only" => {}
                "--help" => {
                    print!(
                        "\
Usage: {prog} [OPTION]... PATTERN...
Permanently delete Recycle Bin items matching PATTERN (glob).

      --older-than=AGE  only match items deleted more than AGE ago: DAYS,
                   or a duration such as 2h, 30m or 1d12h
      --newer-than=AGE  only match items deleted within the last AGE
      --larger-than=SIZE  only match items over SIZE (500M, 1.5G, 64k)
      --smaller-than=SIZE only match items under SIZE
  -n, --dry-run  list matches only
  -f, --force    allow mass patterns (e.g. '*')
  -v, --verbose  print each path
      --help     display this help and exit
"
                    );
                    return Some(0);
                }
                "--version" => {
                    println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                    return Some(0);
                }
                a if a.starts_with("--trash-dir=") => {}
                a if a.starts_with('-') => {
                    eprintln!("{prog}: unrecognized option '{a}'");
                    return Some(2);
                }
                a => patterns.push(a.to_string()),
            }
            None
        });
        let filters = match filters {
            Ok(f) => f,
            Err(code) => return code,
        };
        if patterns.is_empty() {
            eprintln!("{prog}: missing PATTERN");
            return 2;
        }
        if patterns.iter().any(|p| p == "*") && !force {
            eprintln!("{prog}: refusing pattern '*' without --force");
            return 2;
        }
        let items = match os_limited::list() {
            Ok(i) => i,
            Err(e) => {
                eprintln!("{prog}: cannot list Recycle Bin: {}", err_msg(e));
                return 1;
            }
        };
        let matched: Vec<TrashItem> = items
            .into_iter()
            .filter(|it| {
                let orig = original_of(it);
                let full = orig.to_string_lossy().into_owned();
                let base = orig
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default();
                patterns
                    .iter()
                    .any(|p| util::fnmatch(p, &full) || util::fnmatch(p, &base))
            })
            .collect();
        // Size last: only the matches need their `$I` read.
        let matched = filters.apply(matched, |it| it.time_deleted, item_size);
        if dry_run {
            for it in &matched {
                println!("{}", crate::util::term_path(&original_of(it)));
            }
            return 0;
        }
        if verbose {
            for it in &matched {
                println!("{}", crate::util::term_path(&original_of(it)));
            }
        }
        match os_limited::purge_all(matched) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("{prog}: purge failed: {}", err_msg(e));
                1
            }
        }
    }

    pub fn setup_run(prog: &str, args: &[String]) -> i32 {
        let mut profile: Option<Option<String>> = None;
        let mut dry_run = false;
        for arg in args {
            match arg.as_str() {
                "--help" => {
                    print!(
                        "\
Usage: {prog} [OPTION]...
Windows note: multi-call symlinks and man/completions install are Unix-oriented.
On Windows, invoke `rtrash put|list|restore|empty|rm|status` directly.

      --powershell-profile[=PATH]  add a line that loads the PowerShell
                   completer to PATH (default: the profile pwsh, else
                   Windows PowerShell, reports); nothing else is written
  -n, --dry-run    say what --powershell-profile would do
      --help       display this help and exit
"
                    );
                    return 0;
                }
                "-n" | "--dry-run" => dry_run = true,
                "--powershell-profile" => profile = Some(None),
                a if a.starts_with("--powershell-profile=") => {
                    profile = Some(Some(a["--powershell-profile=".len()..].to_string()));
                }
                "--version" => {
                    println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                    return 0;
                }
                _ => {}
            }
        }
        if let Some(path) = profile {
            return crate::shell_assets::setup_powershell_profile(path.as_deref(), dry_run);
        }
        eprintln!(
            "{prog}: setup multi-call install is Unix-oriented; on Windows use subcommands directly"
        );
        eprintln!("{prog}: for PowerShell completions, run `rtrash setup --powershell-profile`");
        eprintln!("{prog}: trash backend: {}", platform::trash_backend_label());
        0
    }
}

/// The `{GUID}` part of a volume name such as `\\?\Volume{GUID}\`, the
/// name the Recycle Bin settings in the registry are filed under.
pub fn volume_guid(name: &str) -> Option<&str> {
    let start = name.find('{')?;
    let end = start + name[start..].find('}')?;
    Some(&name[start..=end])
}

/// How full the Recycle Bin is against its cap: `cap 4.0 GiB, 30% used`,
/// or a note that files skip the bin. Empty when there is nothing to say.
pub fn cap_note(bytes: u64, quota: &BinQuota) -> String {
    if quota.nuke_on_delete {
        return "deletes skip the Recycle Bin on this drive".into();
    }
    match quota.max_bytes {
        Some(0) => "cap 0 bytes: deletes skip the Recycle Bin".into(),
        Some(cap) => format!(
            "cap {}, {}% used",
            crate::util::format_bytes(cap),
            (u128::from(bytes) * 100 / u128::from(cap)).min(999)
        ),
        None => String::new(),
    }
}

/// The Recycle Bin settings of one volume. A missing `MaxCapacity` means
/// Windows applies its own default share of the volume.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BinQuota {
    /// `MaxCapacity` (stored in megabytes) in bytes, when set.
    pub max_bytes: Option<u64>,
    /// `NukeOnDelete`: deleted files skip the bin.
    pub nuke_on_delete: bool,
}

/// The Recycle Bin settings of the volume whose root is `root`, from
/// `HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume\{GUID}`.
#[cfg(windows)]
pub fn bin_quota(root: &Path) -> BinQuota {
    let Some(guid) = volume_guid_of(root) else {
        return BinQuota::default();
    };
    let key =
        format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume\{guid}");
    BinQuota {
        max_bytes: reg_dword(&key, "MaxCapacity").map(|mb| u64::from(mb) * 1024 * 1024),
        nuke_on_delete: reg_dword(&key, "NukeOnDelete").is_some_and(|v| v != 0),
    }
}

#[cfg(windows)]
fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain(Some(0)).collect()
}

#[cfg(windows)]
fn reg_dword(key: &str, name: &str) -> Option<u32> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let (k, n) = (wide(key.as_ref()), wide(name.as_ref()));
    let mut data: u32 = 0;
    let mut size: u32 = 4;
    // SAFETY: both names are NUL-terminated and `data` is the 4 bytes `size` names.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            k.as_ptr(),
            n.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut data as *mut u32).cast(),
            &mut size,
        )
    };
    (rc == 0).then_some(data)
}

#[cfg(windows)]
fn volume_guid_of(root: &Path) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeNameForVolumeMountPointW;
    let mut r = root.as_os_str().to_os_string();
    if !r.to_string_lossy().ends_with('\\') {
        r.push("\\");
    }
    let w = wide(&r);
    let mut buf = [0u16; 64];
    // SAFETY: `w` is NUL-terminated and `buf` holds the 50 characters a
    // volume name needs.
    let ok = unsafe { GetVolumeNameForVolumeMountPointW(w.as_ptr(), buf.as_mut_ptr(), 64) };
    if ok == 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let name = String::from_utf16_lossy(&buf[..len]);
    volume_guid(&name).map(str::to_string)
}

/// Roots of the fixed and removable drives, such as `C:\`, where a
/// Recycle Bin can live.
#[cfg(windows)]
pub fn bin_drives() -> Vec<PathBuf> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    // SAFETY: no arguments.
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| PathBuf::from(format!("{}:\\", char::from(b'A' + i))))
        .filter(|root| {
            let w = wide(root.as_os_str());
            // SAFETY: `w` is NUL-terminated.
            let t = unsafe { GetDriveTypeW(w.as_ptr()) };
            t == DRIVE_FIXED || t == DRIVE_REMOVABLE
        })
        .collect()
}

/// Items and bytes in the Recycle Bin of the drive whose root is `root`
/// (for example `C:\\`).
#[cfg(windows)]
pub fn query_bin(root: &Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::{SHQueryRecycleBinW, SHQUERYRBINFO};
    let w: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut info = SHQUERYRBINFO {
        cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32,
        i64Size: 0,
        i64NumItems: 0,
    };
    // SAFETY: `w` is NUL-terminated and `info` carries its own size.
    let hr = unsafe { SHQueryRecycleBinW(w.as_ptr(), &mut info) };
    (hr >= 0).then(|| (info.i64NumItems.max(0) as u64, info.i64Size.max(0) as u64))
}

#[cfg(windows)]
pub fn put_run(prog: &str, args: &[String]) -> i32 {
    os::put_run(prog, args)
}
#[cfg(windows)]
pub fn list_run(prog: &str, args: &[String]) -> i32 {
    os::list_run(prog, args)
}
#[cfg(windows)]
pub fn empty_run(prog: &str, args: &[String]) -> i32 {
    os::empty_run(prog, args)
}
#[cfg(windows)]
pub fn restore_run(prog: &str, args: &[String]) -> i32 {
    os::restore_run(prog, args)
}
#[cfg(windows)]
pub fn status_run(prog: &str, args: &[String]) -> i32 {
    os::status_run(prog, args)
}
#[cfg(windows)]
pub fn rm_run(prog: &str, args: &[String]) -> i32 {
    os::rm_run(prog, args)
}
#[cfg(windows)]
pub fn setup_run(prog: &str, args: &[String]) -> i32 {
    os::setup_run(prog, args)
}
#[cfg(windows)]
pub fn bin_items() -> Vec<(PathBuf, Vec<BinStat>)> {
    os::bin_items()
}
#[cfg(windows)]
pub fn purge_oldest_on(
    prog: &str,
    at: &Path,
    need: (u64, u64),
    dry_run: bool,
    verbose: bool,
) -> (u64, u64, u64) {
    os::purge_oldest_on(prog, at, need, dry_run, verbose)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn removed_line_names_the_bin_of_the_volume() {
        assert_eq!(
            removed_line(r"C:\mnt\data\a.txt", Some(Path::new(r"C:\mnt\data\"))),
            r"removed 'C:\mnt\data\a.txt' (Recycle Bin of C:\mnt\data\)"
        );
        assert_eq!(removed_line("a.txt", None), "removed 'a.txt'");
    }

    #[test]
    fn drive_key_tells_drives_and_shares_apart() {
        let c = drive_key(Path::new(r"C:\Users\me\tmp"));
        assert_eq!(c, u64::from(b'C'));
        assert_eq!(drive_key(Path::new(r"c:/other")), c);
        assert_eq!(drive_key(Path::new(r"\\?\C:\x")), c);
        assert_ne!(drive_key(Path::new(r"D:\x")), c);
        let share = drive_key(Path::new(r"\\srv\Share\a"));
        assert_eq!(share, drive_key(Path::new(r"\\SRV\share\b\c")));
        assert_eq!(share, drive_key(Path::new(r"\\?\UNC\srv\share\d")));
        assert_ne!(share, drive_key(Path::new(r"\\srv\other")));
        assert_ne!(share, c);
        let v = serial_key(u32::from(b'C'));
        assert!(v != c && v != share && v & 1 << 63 == 0, "{v:x}");
        assert_ne!(serial_key(1), serial_key(2));
    }

    #[cfg(windows)]
    #[test]
    fn volume_key_reads_the_serial_of_the_temp_volume() {
        let t = std::env::temp_dir();
        let k = volume_key(&t);
        assert_ne!(k & 1 << 62, 0, "no serial for {}: {k:x}", t.display());
        assert_eq!(volume_key(&t.join("rtrash-no-such-dir")), k);
    }

    #[test]
    fn on_volume_takes_the_items_of_a_mounted_volume_apart_from_its_drive() {
        let ids: Vec<PathBuf> = [
            r"C:\$Recycle.Bin\S-1\$R1.txt",
            r"C:\mnt\data\$Recycle.Bin\S-1\$R2.txt",
            r"D:\$Recycle.Bin\S-1\$R3.txt",
            r"C:\mnt\data\$Recycle.Bin\S-1\$R4.txt",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        // The mounted folder is a volume of its own, as GetVolumePathNameW
        // and its serial would tell.
        let key = |p: &Path| {
            if p.to_string_lossy().starts_with(r"C:\mnt\data\") {
                serial_key(7)
            } else {
                drive_key(p)
            }
        };
        assert_eq!(on_volume(&ids, key, serial_key(7)), vec![1, 3]);
        assert_eq!(on_volume(&ids, key, drive_key(Path::new(r"C:\"))), vec![0]);
        assert_eq!(on_volume(&ids, key, 0), Vec::<usize>::new());
    }

    #[test]
    fn history_files_are_named_by_drive_letter_or_volume_serial() {
        assert!(!mounted_folder(r"C:\"));
        assert!(!mounted_folder("d:"));
        assert!(mounted_folder(r"C:\rtrash-mnt\"));
        assert_eq!(history_name(r"c:\", 0), "C.txt");
        assert_eq!(
            history_name(r"C:\rtrash-mnt\", serial_key(0x1234_abcd)),
            "vol-1234abcd.txt"
        );
    }

    #[test]
    fn empty_summary_json_matches_the_unix_fields() {
        assert_eq!(
            empty_summary_json(true, 2, 10, Some(2 * 86_400), None),
            r#"{"dry_run":true,"removed":2,"bytes":10,"days":2,"older_than_secs":172800,"max_size":null}"#
        );
        assert_eq!(
            empty_summary_json(false, 0, 0, Some(43_200), Some(5)),
            r#"{"dry_run":false,"removed":0,"bytes":0,"days":0.5,"older_than_secs":43200,"max_size":5}"#
        );
    }

    #[test]
    fn pick_oldest_takes_items_by_age_until_enough_goes() {
        let items = [(30, 5, 1), (10, 4, 1), (20, 100, 9), (40, 1, 1)];
        assert_eq!(pick_oldest(&items, (0, 0)), Vec::<usize>::new());
        assert_eq!(pick_oldest(&items, (4, 0)), vec![1]);
        assert_eq!(pick_oldest(&items, (5, 0)), vec![1, 2]);
        assert_eq!(pick_oldest(&items, (0, 11)), vec![1, 2, 0]);
        assert_eq!(pick_oldest(&items, (u64::MAX, 0)), vec![1, 2, 0, 3]);
    }

    #[test]
    fn recycle_i_v2_roundtrip() {
        let ft = FILETIME_UNIX_EPOCH + 86_400 * HUNDRED_NS_PER_SEC; // 1970-01-02
        let buf = encode_recycle_i_v2(Path::new(r"C:\Users\me\doc.txt"), 42, ft);
        let meta = parse_recycle_i(&buf).expect("parse");
        assert_eq!(meta.version, 2);
        assert_eq!(meta.size, 42);
        assert_eq!(meta.delete_filetime, ft);
        assert_eq!(meta.original, PathBuf::from(r"C:\Users\me\doc.txt"));
        assert_eq!(filetime_to_unix_epoch(ft), 86_400);
    }

    /// Hand-built records in the on-disk layout, not via the encoder, so
    /// a shared mistake in both cannot hide.
    #[test]
    fn recycle_i_parses_real_v1_and_v2_layouts() {
        let path = r"C:\Users\runneradmin\AppData\Local\Temp\rtrash-purge-6716-1.txt";
        let utf16: Vec<u8> = path.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let head = |v: u64| -> Vec<u8> {
            [v.to_le_bytes(), 7u64.to_le_bytes(), 9u64.to_le_bytes()].concat()
        };
        // v2: count of UTF-16 units with the NUL, then the path and NUL.
        let mut v2 = head(2);
        v2.extend_from_slice(&(path.encode_utf16().count() as u32 + 1).to_le_bytes());
        v2.extend_from_slice(&utf16);
        v2.extend_from_slice(&[0, 0]);
        let m = parse_recycle_i(&v2).expect("v2");
        assert_eq!((m.version, m.size, m.delete_filetime), (2, 7, 9));
        assert_eq!(m.original, PathBuf::from(path), "{:?}", m.original);
        // v1: the path in a 520-byte buffer padded with NULs and junk.
        let mut v1 = head(1);
        let mut buf = utf16.clone();
        buf.extend_from_slice(&[0, 0, b'x', 0, b'\r', 0]);
        buf.resize(520, 0);
        v1.extend_from_slice(&buf);
        let m = parse_recycle_i(&v1).expect("v1");
        assert_eq!(m.original, PathBuf::from(path), "{:?}", m.original);
        // A relative path or a control character is not a path to trust.
        let mut bad = head(2);
        let rel: Vec<u8> = "Temp\r\rx"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        bad.extend_from_slice(&5u32.to_le_bytes());
        bad.extend_from_slice(&rel);
        assert!(parse_recycle_i(&bad).is_none());
        let mut ctl = head(2);
        let s: Vec<u8> = "C:\\a\rb"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        ctl.extend_from_slice(&6u32.to_le_bytes());
        ctl.extend_from_slice(&s);
        assert!(parse_recycle_i(&ctl).is_none());
    }

    #[test]
    fn volume_guid_and_cap_note() {
        let name = r"\\?\Volume{0a1b2c3d-0000-0000-0000-100000000000}\";
        assert_eq!(
            volume_guid(name),
            Some("{0a1b2c3d-0000-0000-0000-100000000000}")
        );
        assert_eq!(volume_guid("C:\\"), None);
        let gib = 1u64 << 30;
        let q = BinQuota {
            max_bytes: Some(4 * gib),
            nuke_on_delete: false,
        };
        let want = format!("cap {}, 25% used", crate::util::format_bytes(4 * gib));
        assert_eq!(cap_note(gib, &q), want);
        assert_eq!(cap_note(gib, &BinQuota::default()), "");
        let nuke = BinQuota {
            max_bytes: Some(gib),
            nuke_on_delete: true,
        };
        assert!(cap_note(0, &nuke).contains("skip"));
        let zero = BinQuota {
            max_bytes: Some(0),
            nuke_on_delete: false,
        };
        assert!(cap_note(5, &zero).contains("skip"));
    }

    #[test]
    fn index_files_and_drive_roots_come_from_the_payload_path() {
        assert_eq!(
            index_file_for(Path::new("bin/$RAB12CD.txt")),
            Some(PathBuf::from("bin/$IAB12CD.txt"))
        );
        assert_eq!(index_file_for(Path::new("bin/notes.txt")), None);
        let id = Path::new(r"d:\$Recycle.Bin\S-1-5-21-1\$RX1.txt");
        assert_eq!(drive_root_of(id), Some(PathBuf::from(r"D:\")));
        assert_eq!(drive_root_of(Path::new("relative")), None);
    }

    #[test]
    fn recycled_size_reads_the_index_and_walks_without_one() {
        let dir = std::env::temp_dir().join(format!("rtrash-bin-size-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("$RFOLDER/sub")).unwrap();
        std::fs::write(dir.join("$RFOLDER/a"), [0u8; 300]).unwrap();
        std::fs::write(dir.join("$RFOLDER/sub/b"), [0u8; 700]).unwrap();
        std::fs::write(dir.join("$RFILE.txt"), b"five!").unwrap();
        let index = encode_recycle_i_v2(Path::new(r"C:\x.txt"), 4242, FILETIME_UNIX_EPOCH);
        std::fs::write(dir.join("$IFILE.txt"), index).unwrap();
        // The index wins: Windows records the size at deletion.
        assert_eq!(recycled_size(&dir.join("$RFILE.txt")), 4242);
        // No index: the files under the folder.
        assert_eq!(recycled_size(&dir.join("$RFOLDER")), 1000);
        // A broken index falls back to the payload too.
        std::fs::write(dir.join("$IFOLDER"), [1u8; 3]).unwrap();
        assert_eq!(recycled_size(&dir.join("$RFOLDER")), 1000);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn remove_forcing_clears_readonly_and_leaves_a_link_target() {
        let dir = std::env::temp_dir().join(format!("rtrash-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let nested = dir.join("nested");
        std::fs::create_dir_all(nested.join("inner")).unwrap();
        std::fs::write(nested.join("inner/f"), b"x").unwrap();
        std::fs::write(nested.join("f"), b"y").unwrap();
        let outside = dir.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("precious"), b"keep").unwrap();
        let _ = std::os::windows::fs::symlink_dir(&outside, nested.join("link"));
        for p in [
            nested.join("inner/f"),
            nested.join("inner"),
            nested.join("f"),
            nested.clone(),
        ] {
            let mut perms = std::fs::symlink_metadata(&p).unwrap().permissions();
            perms.set_readonly(true);
            std::fs::set_permissions(&p, perms).unwrap();
        }
        remove_forcing(&nested).unwrap();
        assert!(!nested.exists());
        assert_eq!(std::fs::read(outside.join("precious")).unwrap(), b"keep");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restores_count_only_items_that_change_drive() {
        let p = PathBuf::from;
        let items = [
            (p("D:\\$Recycle.Bin\\S-1\\$R1"), p("C:\\a\\x"), 10),
            (p("D:\\$Recycle.Bin\\S-1\\$R2"), p("D:\\b"), 99),
            (p("E:\\$Recycle.Bin\\S-1\\$R3"), p("c:/y"), 5),
            (p("relative"), p("C:\\z"), 7),
        ];
        assert_eq!(cross_drive_needs(&items), [(p("C:\\"), 15)]);
    }

    #[test]
    fn restores_onto_a_mounted_folder_count_against_that_volume() {
        let p = PathBuf::from;
        // C:\mnt\data is a volume of its own, as GetVolumePathNameW tells.
        let root_of = |x: &Path| {
            let s = x.to_string_lossy();
            if s.starts_with(r"C:\mnt\data\") {
                Some(p(r"C:\mnt\data\"))
            } else {
                drive_root_of(x)
            }
        };
        let items = [
            (p(r"C:\$Recycle.Bin\S-1\$R1"), p(r"C:\mnt\data\x"), 10),
            (
                p(r"C:\mnt\data\$Recycle.Bin\S-1\$R2"),
                p(r"C:\mnt\data\y"),
                99,
            ),
            (p(r"C:\mnt\data\$Recycle.Bin\S-1\$R3"), p(r"C:\z"), 5),
        ];
        assert_eq!(
            cross_volume_needs(&items, root_of),
            [(p(r"C:\"), 5), (p(r"C:\mnt\data\"), 10)]
        );
        // By drive letter alone, all three look like moves on C.
        assert_eq!(cross_drive_needs(&items), []);
    }

    #[test]
    fn bin_filters_parse_like_the_freedesktop_ones_and_apply() {
        let args: Vec<String> = ["--older-than=1", "-n", "--larger-than", "1K", "pat"]
            .map(String::from)
            .to_vec();
        let mut rest = Vec::new();
        let f = parse_with_filters("t", &args, |a| {
            rest.push(a.to_string());
            None
        })
        .unwrap();
        assert_eq!(rest, ["-n", "pat"]);
        assert_eq!(
            (f.age.older_than, f.size.larger_than),
            (Some(86_400), Some(1024))
        );
        let now = crate::util::now_epoch();
        let items = vec![
            (now - 3 * 86_400, 4096u64),
            (now - 60, 4096),
            (now - 3 * 86_400, 10),
        ];
        let kept = f.apply(items, |it| it.0, |it| it.1);
        assert_eq!(kept, [(now - 3 * 86_400, 4096)]);
        // No size bound: sizes are never asked for.
        let age_only = BinFilters {
            age: crate::filters::AgeFilter {
                older_than: None,
                newer_than: Some(86_400),
            },
            ..BinFilters::default()
        };
        let kept = age_only.apply(
            vec![now - 60, now - 9 * 86_400],
            |t| *t,
            |_| panic!("sized"),
        );
        assert_eq!(kept, [now - 60]);
        let bad = |a: &[&str]| {
            let a: Vec<String> = a.iter().map(|s| s.to_string()).collect();
            parse_with_filters("t", &a, |_| None).unwrap_err()
        };
        assert_eq!(bad(&["--newer-than=x"]), 2);
        assert_eq!(bad(&["--larger-than=2K", "--smaller-than=1K"]), 2);
        let stop = parse_with_filters("t", &["--help".to_string()], |_| Some(0));
        assert_eq!(stop.unwrap_err(), 0);
    }

    #[test]
    fn recycle_i_rejects_short() {
        assert!(parse_recycle_i(&[0u8; 8]).is_none());
    }

    #[test]
    fn filetime_matches_a_known_date() {
        // 2026-01-01T00:00:00Z: 11644473600 s after 1601 plus the Unix time.
        assert_eq!(
            filetime_to_unix_epoch(134_116_992_000_000_000),
            1_767_225_600
        );
    }

    #[test]
    fn filetime_epoch_zero() {
        assert_eq!(filetime_to_unix_epoch(FILETIME_UNIX_EPOCH), 0);
        assert_eq!(filetime_to_unix_epoch(0), 0);
    }
}
