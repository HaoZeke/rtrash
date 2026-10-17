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
    // NTFS names are UTF-16 units, not checked text, so a name may hold
    // an unpaired surrogate. Windows keeps it exact; elsewhere such a
    // path has no form and is skipped.
    if !is_plausible_original(&String::from_utf16_lossy(&units)) {
        return None;
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        Some(PathBuf::from(std::ffi::OsString::from_wide(&units)))
    }
    #[cfg(not(windows))]
    {
        String::from_utf16(&units).ok().map(PathBuf::from)
    }
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

/// Move a Recycle Bin payload back to `dest`, its original path, and
/// drop the `$I` record beside it. The shell's restore names the result
/// after the display name, which loses a known extension, so rtrash moves
/// the payload itself. Another volume gets a copy, then the payload goes.
pub fn restore_payload(payload: &Path, dest: &Path) -> std::io::Result<()> {
    crate::allow_roots::check(dest)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::rename(payload, dest).is_err() {
        // Most likely another volume: copy, then remove the payload.
        // scratch: a move; the copy is whole first.
        copy_tree(payload, dest)?;
        let meta = std::fs::symlink_metadata(payload)?;
        if meta.is_dir() {
            std::fs::remove_dir_all(payload)?;
        } else {
            std::fs::remove_file(payload)?;
        }
    }
    if let Some(index) = index_file_for(payload) {
        match std::fs::remove_file(&index) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Copy a file or directory tree; links copy as links where the system
/// allows, else as their targets' content.
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.is_dir() {
        std::fs::create_dir(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_tree(&e.path(), &dst.join(e.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(src, dst).map(|_| ())
    }
}

/// One Recycle Bin item read from its `$I` record, paired with its `$R`
/// payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinRecord {
    /// The `$R` payload.
    pub payload: PathBuf,
    /// The original path, as the record holds it.
    pub original: PathBuf,
    /// Deletion time, Unix seconds.
    pub deleted: i64,
    /// The size the record gives, in bytes.
    pub size: u64,
}

/// Every item in one user's Recycle Bin folder (`X:\$Recycle.Bin\SID`):
/// each `$I` record that parses and still has its `$R` payload, oldest
/// first. Names stay exact, so one odd name cannot hide the rest. A
/// missing or unreadable folder gives none.
pub fn scan_bin_dir(dir: &Path) -> Vec<BinRecord> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let kept = crate::util::str_from_os(e.file_name());
        let Some(rest) = kept.strip_prefix("$I") else {
            continue;
        };
        let payload = dir.join(crate::util::os_from_str(&format!("$R{rest}")));
        if std::fs::symlink_metadata(&payload).is_err() {
            continue;
        }
        let Some(meta) = std::fs::read(e.path())
            .ok()
            .and_then(|b| parse_recycle_i(&b))
        else {
            continue;
        };
        out.push(BinRecord {
            payload,
            original: meta.original,
            deleted: filetime_to_unix_epoch(meta.delete_filetime),
            size: meta.size,
        });
    }
    out.sort_by(|a, b| {
        a.deleted
            .cmp(&b.deleted)
            .then_with(|| a.payload.cmp(&b.payload))
    });
    out
}

/// The largest `$I` record: a version 2 header and a path of 32767
/// UTF-16 units with its NUL. A bigger file is not a record.
const I_RECORD_CAP: u64 = 0x1C + 2 * 32_768;

/// One finding of `status --check` in a Recycle Bin folder.
#[derive(Debug, PartialEq, Eq)]
pub enum BinProblem {
    /// A `$I` record whose `$R` payload is gone; the original path it names.
    Dangling { record: PathBuf, origin: PathBuf },
    /// A `$I` record that cannot be read or parsed, and why.
    BadRecord { record: PathBuf, why: String },
    /// A `$R` payload with no `$I` record.
    Orphan { payload: PathBuf },
    /// A `$I` name that is a folder or a link, not a record file.
    Stray { path: PathBuf },
}

impl BinProblem {
    /// The record `--fix` removes, when it removes one.
    pub fn fix_target(&self) -> Option<&Path> {
        match self {
            BinProblem::Dangling { record, .. } | BinProblem::BadRecord { record, .. } => {
                Some(record.as_path())
            }
            _ => None,
        }
    }

    /// The report line, the same with and without `--fix`; the prefixes
    /// are those of the FreeDesktop check.
    pub fn line(&self) -> String {
        let t = crate::util::term_path;
        match self {
            BinProblem::Dangling { record, .. } => {
                format!("dangling: {} (the payload is gone)", t(record))
            }
            BinProblem::BadRecord { record, why } => format!("bad record: {}: {why}", t(record)),
            BinProblem::Orphan { payload } => format!(
                "orphan: {} (kept; with no record, list and rm do not show it)",
                t(payload)
            ),
            BinProblem::Stray { path } => {
                format!("stray: {} (kept; not a record file)", t(path))
            }
        }
    }
}

/// Read a `$I` record, refusing one larger than [`I_RECORD_CAP`].
fn read_index(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut body = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(I_RECORD_CAP + 1).read_to_end(&mut body))
        .map_err(|e| format!("cannot be read ({e})"))?;
    if body.len() as u64 > I_RECORD_CAP {
        return Err("larger than any $I record".to_string());
    }
    Ok(body)
}

/// The problems in one user's Recycle Bin folder (`X:\$Recycle.Bin\SID`):
/// the `$I` records in name order, then the `$R` payloads with no record.
/// The same reading as [`scan_bin_dir`], which skips what this reports.
/// Other names (`desktop.ini`) are the shell's and are not looked at.
pub fn check_bin_dir(dir: &Path) -> Vec<BinProblem> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| crate::util::str_from_os(e.file_name()))
        .collect();
    names.sort();
    let at = |name: &str| dir.join(crate::util::os_from_str(name));
    let mut out = Vec::new();
    for name in &names {
        let Some(rest) = name.strip_prefix("$I") else {
            continue;
        };
        let record = at(name);
        let is_file = std::fs::symlink_metadata(&record).is_ok_and(|m| m.file_type().is_file());
        if !is_file {
            out.push(BinProblem::Stray { path: record });
            continue;
        }
        let body = match read_index(&record) {
            Ok(b) => b,
            Err(why) => {
                out.push(BinProblem::BadRecord { record, why });
                continue;
            }
        };
        let Some(meta) = parse_recycle_i(&body) else {
            let why = "cannot be parsed as a $I record".to_string();
            out.push(BinProblem::BadRecord { record, why });
            continue;
        };
        if std::fs::symlink_metadata(at(&format!("$R{rest}"))).is_err() {
            out.push(BinProblem::Dangling {
                record,
                origin: meta.original,
            });
        }
    }
    for name in &names {
        let Some(rest) = name.strip_prefix("$R") else {
            continue;
        };
        if std::fs::symlink_metadata(at(&format!("$I{rest}"))).is_err() {
            out.push(BinProblem::Orphan { payload: at(name) });
        }
    }
    out
}

/// `status --check`, and `--fix` with or without `-n`, over the Recycle
/// Bin folders `dirs`, as [`crate::trash_check`] does for FreeDesktop
/// trashes. `--fix` removes dangling and bad `$I` records only, through
/// [`crate::permanent_guard::remove`]: `RTRASH_NO_PERMANENT` refuses it,
/// `RTRASH_MAX_PERMANENT` caps it before the first removal, and with
/// `RTRASH_ALLOW_ROOTS` set a dangling record must name a path under a
/// root while a bad record, whose origin is unknown, refuses the run.
/// A payload a bad record named stays, as an orphan. Exit 0 when nothing
/// is found or every removal went through; 1 when a check or a preview
/// finds problems, or a removal failed; 2 when a guard refuses the fix.
pub fn check_bins(prog: &str, dirs: &[PathBuf], fix: bool, dry_run: bool) -> i32 {
    let found: Vec<BinProblem> = dirs
        .iter()
        .map(PathBuf::as_path)
        .flat_map(check_bin_dir)
        .collect();
    let fixable = found.iter().filter(|p| p.fix_target().is_some()).count() as u64;
    let total = found.len() as u64;
    let acting = fix && !dry_run;
    if acting && fixable > 0 {
        let scoped: Vec<(PathBuf, Option<PathBuf>)> = found
            .iter()
            .filter_map(|p| match p {
                BinProblem::Dangling { record, origin } => {
                    Some((record.clone(), Some(origin.clone())))
                }
                BinProblem::BadRecord { record, .. } => Some((record.clone(), None)),
                _ => None,
            })
            .collect();
        if let Some(code) = crate::allow_roots::refuse_outside_or_unplaced(prog, &scoped) {
            return code;
        }
        if let Some(code) = crate::permanent_guard::check_count(prog, fixable) {
            return code;
        }
    }
    let (mut removed, mut failed) = (0u64, 0u64);
    for p in &found {
        println!("{}", p.line());
        let Some(record) = p.fix_target().filter(|_| fix) else {
            continue;
        };
        if dry_run {
            println!("would remove {}", crate::util::term_path(record));
            continue;
        }
        match crate::permanent_guard::remove(record) {
            Ok(()) => {
                println!("removed {}", crate::util::term_path(record));
                removed += 1;
            }
            Err(e) => {
                eprintln!(
                    "{prog}: cannot remove '{}': {e}",
                    crate::util::err_path(record)
                );
                failed += 1;
            }
        }
    }
    let plural = crate::util::plural;
    if acting {
        eprintln!(
            "{prog}: removed {}; {} left to review",
            plural(removed, "record", "records"),
            plural(total - fixable, "problem", "problems")
        );
    } else {
        eprintln!(
            "{prog}: {} found, {} --fix would remove",
            plural(total, "problem", "problems"),
            plural(fixable, "record", "records")
        );
    }
    i32::from(failed > 0 || (!acting && total > 0))
}

/// Windows: the current user's SID as text (`S-1-5-21-…`).
#[cfg(windows)]
pub fn current_user_sid() -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut tok: HANDLE = std::ptr::null_mut();
    // SAFETY: the pseudo handle needs no closing; `tok` receives a handle.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) } == 0 {
        return None;
    }
    let mut len = 0u32;
    // SAFETY: a size query with no buffer.
    unsafe { GetTokenInformation(tok, TokenUser, std::ptr::null_mut(), 0, &mut len) };
    // u64 units keep TOKEN_USER aligned.
    let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
    // SAFETY: `buf` holds at least `len` bytes.
    let ok = unsafe { GetTokenInformation(tok, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) };
    // SAFETY: `tok` came from OpenProcessToken and is closed once.
    unsafe { CloseHandle(tok) };
    if ok == 0 {
        return None;
    }
    // SAFETY: the call filled `buf` with a TOKEN_USER.
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: `sid` points into `buf`, alive here; `text` receives a LocalAlloc string.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 || text.is_null() {
        return None;
    }
    // SAFETY: `text` is NUL-terminated, and freed once after the copy.
    let out = unsafe {
        let n = (0..).take_while(|&i| *text.add(i) != 0).count();
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(text, n));
        LocalFree(text.cast());
        s
    };
    Some(out)
}

/// Windows: every item in the current user's Recycle Bins, on the drives
/// and on folder-mounted volumes, oldest first. Reads the `$I` records
/// itself: the `trash` crate's list fails as a whole when one name holds
/// an unpaired surrogate.
#[cfg(windows)]
pub fn list_items() -> io::Result<Vec<trash::TrashItem>> {
    let sid = current_user_sid().ok_or_else(|| io::Error::other("cannot read the user's SID"))?;
    let mut recs = Vec::new();
    for root in crate::trash_history::bin_roots() {
        recs.extend(scan_bin_dir(&root.join("$Recycle.Bin").join(&sid)));
    }
    recs.sort_by(|a, b| {
        a.deleted
            .cmp(&b.deleted)
            .then_with(|| a.payload.cmp(&b.payload))
    });
    Ok(recs
        .into_iter()
        .map(|r| trash::TrashItem {
            id: r.payload.into_os_string(),
            name: r
                .original
                .file_name()
                .map(|n| n.to_os_string())
                .unwrap_or_default(),
            original_parent: r
                .original
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default(),
            time_deleted: r.deleted,
        })
        .collect())
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

/// Windows: each volume's GUID path (`\\?\Volume{…}\`) with the first
/// folder or drive it is mounted on, so a path the shell wrote through the
/// GUID can be read back as the path the user typed.
#[cfg(windows)]
pub fn volume_mounts() -> Vec<(String, String)> {
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
            let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            let guid = std::ffi::OsString::from_wide(&name[..end])
                .to_string_lossy()
                .into_owned();
            if let Some(first) = paths.split(|&c| c == 0).find(|p| !p.is_empty()) {
                let mount = std::ffi::OsString::from_wide(first)
                    .to_string_lossy()
                    .into_owned();
                out.push((guid, mount));
            }
        }
        // SAFETY: `h` came from FindFirstVolumeW; `name` as above.
        if unsafe { FindNextVolumeW(h, name.as_mut_ptr(), name.len() as u32) } == 0 {
            break;
        }
    }
    // SAFETY: `h` came from FindFirstVolumeW and is closed once.
    unsafe { FindVolumeClose(h) };
    out
}

/// A comparison key for a Windows path: the verbatim prefix gone, a volume
/// GUID path read as its mount point (`mounts` from `volume_mounts`),
/// forward slashes turned back, no trailing separator, and lower case,
/// since Windows file names compare without case.
pub fn path_key(path: &str, mounts: &[(String, String)]) -> String {
    let mut s = path.replace('/', "\\");
    let lower = s.to_ascii_lowercase();
    for (guid, mount) in mounts {
        let g = guid.trim_end_matches('\\').to_ascii_lowercase();
        if lower == g || lower.starts_with(&format!("{g}\\")) {
            let rest = s[g.len()..].trim_start_matches('\\').to_string();
            s = format!("{}\\{rest}", mount.trim_end_matches('\\'));
            break;
        }
    }
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        s = format!(r"\\{rest}");
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        s = rest.to_string();
    }
    let t = s.trim_end_matches('\\');
    // Keep `c:\` for a drive root.
    let t = if t.len() == 2 && t.ends_with(':') {
        s.as_str()
    } else {
        t
    };
    t.to_lowercase()
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

    /// `list --depth=N`: what a deleted folder's `$R` payload holds, N
    /// levels down, indented by level, folders with a trailing `\`, names
    /// in order; links are not followed. A file or depth 0 prints nothing.
    fn write_tree(out: &mut impl std::io::Write, dir: &Path, depth: usize) -> std::io::Result<()> {
        let is_dir = std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir());
        if depth == 0 || !is_dir {
            return Ok(());
        }
        write_tree_below(out, dir, 0, depth)
    }

    /// The rest of [`write_tree`], from `level` down.
    fn write_tree_below(
        out: &mut impl std::io::Write,
        dir: &Path,
        level: usize,
        depth: usize,
    ) -> std::io::Result<()> {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return Ok(());
        };
        let mut kids: Vec<(String, PathBuf, bool)> = rd
            .flatten()
            .map(|e| {
                let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
                let name = e.file_name().to_string_lossy().into_owned();
                (name, e.path(), is_dir)
            })
            .collect();
        kids.sort();
        for (name, path, is_dir) in kids {
            let slash = if is_dir { "\\" } else { "" };
            let indent = "  ".repeat(level);
            writeln!(out, "    {indent}{name}{slash}")?;
            if is_dir && level + 1 < depth {
                write_tree_below(out, &path, level + 1, depth)?;
            }
        }
        Ok(())
    }

    /// `put --inspect`: ask before trashing `f`, naming its size and, for
    /// a directory, its file count; true on y.
    fn inspect_ok(prog: &str, f: &Path) -> bool {
        let is_dir = std::fs::symlink_metadata(f).is_ok_and(|m| m.is_dir());
        let size = crate::util::format_bytes(tree_bytes(f));
        let (kind, what) = if is_dir {
            let n = files_below(f);
            let s = if n == 1 { "" } else { "s" };
            ("directory", format!("{size}, {n} file{s}"))
        } else {
            ("file", size)
        };
        let shown = crate::util::term_path(f);
        crate::util::confirm(&format!("{prog}: remove {kind} '{shown}' ({what})? "))
    }

    /// Files below `dir` (not directories), without following links.
    fn files_below(dir: &Path) -> u64 {
        let mut n = 0u64;
        let mut todo = vec![dir.to_path_buf()];
        while let Some(d) = todo.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in rd.flatten() {
                match e.file_type() {
                    Ok(t) if t.is_dir() => todo.push(e.path()),
                    Ok(_) => n += 1,
                    Err(_) => {}
                }
            }
        }
        n
    }

    fn item_size(it: &TrashItem) -> u64 {
        recycled_size(Path::new(&it.id))
    }

    pub fn put_run(prog: &str, args: &[String]) -> i32 {
        // A second early, so the record covers every item this run moves.
        let started = crate::util::now_epoch() - 1;
        let mut force = false;
        let mut verbose = false;
        let mut files: Vec<PathBuf> = Vec::new();
        let mut plain = false;
        let mut json = false;
        let mut pinned = false;
        let mut no_more_opts = false;
        let mut in_use: Option<String> = None;
        let mut dry_run = false;
        let mut inspect = false;
        for arg in args {
            if no_more_opts || arg == "-" || !arg.starts_with('-') {
                files.push(crate::util::arg_path(arg));
                continue;
            }
            match arg.as_str() {
                "--" => no_more_opts = true,
                // A later -f turns --inspect off, as on FreeDesktop.
                "-f" | "--force" => {
                    force = true;
                    inspect = false;
                }
                "--inspect" => {
                    force = false;
                    inspect = true;
                }
                "-v" | "--verbose" => verbose = true,
                "--plain" => plain = true,
                "--json" => json = true,
                "--dry-run" => dry_run = true,
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
      --inspect  ask before each item, showing its size and, for a
                   directory, how many files it holds
      --dry-run  run every check (missing files, holders) and print
                   'would trash PATH' for each item that would move,
                   moving nothing
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
                            inspect = false;
                        }
                        if a.contains('v') {
                            verbose = true;
                        }
                        continue;
                    }
                    eprintln!("{prog}: unrecognized option '{a}'");
                    return 2;
                }
                a => files.push(crate::util::arg_path(a)),
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
        // RTRASH_ALLOW_ROOTS: every operand in scope, or nothing moves.
        if let Some(code) = crate::allow_roots::refuse_outside(prog, &files) {
            return code;
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
            if inspect && !inspect_ok(prog, f) {
                continue;
            }
            if dry_run {
                println!("would trash '{}'", crate::util::term_path(&abs));
                continue;
            }
            let root = test_bin_root().or_else(|| bin_root_of(&abs));
            let bin = verbose.then(|| root.clone()).flatten();
            match crate::allow_roots::trash_delete(f) {
                Ok(()) => {
                    if verbose && !json {
                        println!(
                            "{}",
                            removed_line(&crate::util::term_path(f), bin.as_deref())
                        );
                    }
                    // The path as the user gave it, made absolute; the
                    // canonical form can name a mounted folder by its
                    // volume GUID.
                    moved.push(
                        std::path::absolute(f)
                            .map(|p| strip_verbatim(&p))
                            .unwrap_or_else(|_| abs.clone()),
                    );
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
            write_last_put(started, &moved);
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
        let items = super::list_items().unwrap_or_default();
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

    /// What Windows `list --sort` orders by, as on FreeDesktop.
    #[derive(Clone, Copy)]
    enum ListSort {
        Date,
        Size,
        Name,
        Path,
    }

    /// The current directory as given and as resolved, for --cwd-only.
    /// Empty when the directory cannot be read.
    fn cwd_forms() -> Vec<PathBuf> {
        match std::env::current_dir() {
            Ok(here) => dir_forms(&here),
            Err(_) => Vec::new(),
        }
    }

    /// A directory as given and as resolved, for --cwd-only and
    /// --directory: a volume mounted on a folder may resolve to its GUID
    /// path, which no original path starts with. A directory that is gone
    /// keeps only its absolute form.
    fn dir_forms(dir: &Path) -> Vec<PathBuf> {
        let mut dirs = vec![std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf())];
        if let Ok(c) = std::fs::canonicalize(dir) {
            dirs.push(strip_verbatim(&c));
        }
        dirs
    }

    /// The forms a PATH operand may take in a `$I` record: as given, and
    /// with its parent resolved. A short (8.3) name such as `RUNNER~1` in
    /// %TEMP% names the same folder as the long name the shell records,
    /// and the item itself is gone from there, so only the parent resolves.
    fn path_forms(abs: &Path) -> Vec<PathBuf> {
        let mut forms = vec![abs.to_path_buf()];
        let parent = abs.parent().and_then(|p| std::fs::canonicalize(p).ok());
        if let (Some(dir), Some(name)) = (parent, abs.file_name()) {
            forms.push(strip_verbatim(&dir).join(name));
        }
        forms
    }

    /// The value of --directory=DIR: absolute from here, or an error.
    fn directory_arg(v: &str) -> Result<PathBuf, String> {
        if v.is_empty() {
            return Err("--directory needs a non-empty DIR".into());
        }
        let d = crate::util::arg_path(v);
        Ok(std::path::absolute(&d).unwrap_or(d))
    }

    pub fn list_run(prog: &str, args: &[String]) -> i32 {
        let mut matcher: Option<crate::budget::PathMatch> = None;
        let mut cwd_only = false;
        let mut directory: Option<PathBuf> = None;
        let mut last: Option<usize> = None;
        let mut depth = 0usize;
        let mut sort = ListSort::Date;
        let mut reverse = false;
        let mut bad: Option<String> = None;
        let mut with_size = false;
        let mut show_trash = false;
        let mut format: Option<Vec<crate::listfmt::FmtPart>> = None;
        let filters = parse_with_filters(prog, args, |arg| match arg {
            "--cwd-only" => {
                cwd_only = true;
                None
            }
            "--size" => {
                with_size = true;
                None
            }
            "--show-trashpath" => {
                show_trash = true;
                None
            }
            "--reverse" => {
                reverse = true;
                None
            }
            a if a.starts_with("--sort=") => match &a["--sort=".len()..] {
                "date" | "time" => {
                    sort = ListSort::Date;
                    None
                }
                "size" => {
                    sort = ListSort::Size;
                    with_size = true;
                    None
                }
                "name" => {
                    sort = ListSort::Name;
                    None
                }
                "path" => {
                    sort = ListSort::Path;
                    None
                }
                v => {
                    bad = Some(format!("invalid --sort '{v}' (date, size, name or path)"));
                    Some(2)
                }
            },
            a if a.starts_with("--directory=") => match directory_arg(&a["--directory=".len()..]) {
                Ok(d) => {
                    directory = Some(d);
                    None
                }
                Err(e) => {
                    bad = Some(e);
                    Some(2)
                }
            },
            a if a.starts_with("--depth=") => match a["--depth=".len()..].parse::<usize>() {
                Ok(n) => {
                    depth = n;
                    None
                }
                Err(_) => {
                    bad = Some(format!(
                        "--depth needs a count of levels, not '{}'",
                        &a["--depth=".len()..]
                    ));
                    Some(2)
                }
            },
            a if a.starts_with("--last=") => match a["--last=".len()..].parse::<usize>() {
                Ok(n) => {
                    last = Some(n);
                    None
                }
                Err(_) => {
                    bad = Some(format!(
                        "--last needs a count of rows, not '{}'",
                        &a["--last=".len()..]
                    ));
                    Some(2)
                }
            },
            a if a.starts_with("--format=") => {
                match crate::listfmt::parse_format(&a["--format=".len()..]) {
                    Ok(f) => {
                        format = Some(f);
                        None
                    }
                    Err(e) => {
                        bad = Some(e);
                        Some(2)
                    }
                }
            }
            a if crate::budget::PathMatch::parse(a).is_some() => {
                match crate::budget::PathMatch::parse(a) {
                    Some(Ok(_)) if matcher.is_some() => {
                        bad = Some("give only one of --glob, --regex and --name".into())
                    }
                    Some(Ok(m)) => matcher = Some(m),
                    Some(Err(e)) => bad = Some(e),
                    None => {}
                }
                if bad.is_some() {
                    Some(2)
                } else {
                    None
                }
            }
            "--help" => {
                print!(
                    "\
Usage: {prog} [OPTION]...
List items in the Windows Recycle Bin (system trash).

      --older-than=DAYS only items deleted more than DAYS days ago
      --newer-than=DAYS only items deleted within the last DAYS days
      --larger-than=SIZE  only items over SIZE (500M, 1.5G, 64k)
      --smaller-than=SIZE only items under SIZE
      --glob=PATTERN  only items whose original path (PATTERN with a
                   separator) or file name matches a shell glob
      --regex=RE  only items whose original path matches RE
      --name=NAME  only items whose file name is NAME
      --cwd-only only items that came from the current directory or below
      --directory=DIR  only items that came from DIR or below it (DIR
                   need not exist any more)
      --sort=KEY order by date (default), size (largest first; implies
                   --size), name (file name) or path (original path)
      --reverse  reverse the order
      --depth=N  under each deleted folder, show what it holds, N levels
                   down, indented (not with --format)
      --last=N   only the last N rows (after --sort and --reverse), like
                   tail -n: the N most recently deleted by default
      --size     add each item's size in the Recycle Bin
      --show-trashpath  under each row, show the item's file in the
                   Recycle Bin ($R...; not with --format)
      --format=TEMPLATE  print each item through TEMPLATE: {{date}} {{epoch}}
                   {{path}} {{name}} {{trash}} {{size}} (bytes); {{batch}} and
                   {{owner}} print nothing here; \\t \\n \\0 escapes
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
        if let Some(e) = &bad {
            eprintln!("{prog}: {e}");
            return 2;
        }
        let filters = match filters {
            Ok(f) => f,
            Err(code) => return code,
        };
        if cwd_only && directory.is_some() {
            eprintln!("{prog}: give --cwd-only or --directory, not both");
            return 2;
        }
        if depth > 0 && format.is_some() {
            eprintln!("{prog}: --depth works on plain rows, not --format");
            return 2;
        }
        if show_trash && format.is_some() {
            eprintln!("{prog}: --show-trashpath works on plain rows, not --format");
            return 2;
        }
        let cwd = match &directory {
            Some(d) => Some(dir_forms(d)),
            None => cwd_only.then(cwd_forms),
        };
        match super::list_items() {
            Ok(items) => {
                let items: Vec<TrashItem> = items
                    .into_iter()
                    .filter(|it| {
                        let o = original_of(it);
                        matcher.as_ref().is_none_or(|m| m.keeps(&o))
                            && cwd
                                .as_ref()
                                .is_none_or(|dirs| dirs.iter().any(|d| o.starts_with(d)))
                    })
                    .collect();
                let mut items = filters.apply(items, |it| it.time_deleted, item_size);
                items.sort_by_key(|i| i.time_deleted);
                // Other keys sort stably, so ties keep the date order.
                match sort {
                    ListSort::Date => {}
                    ListSort::Size => items.sort_by_cached_key(|i| std::cmp::Reverse(item_size(i))),
                    ListSort::Name => items.sort_by_cached_key(|i| {
                        original_of(i).file_name().map(|n| n.to_os_string())
                    }),
                    ListSort::Path => items.sort_by_cached_key(original_of),
                }
                if reverse {
                    items.reverse();
                }
                // --last keeps the tail of the listing, as tail -n would.
                if let Some(n) = last {
                    let cut = items.len().saturating_sub(n);
                    items.drain(..cut);
                }
                use std::io::Write;
                let mut out = std::io::BufWriter::new(std::io::stdout().lock());
                for it in items {
                    let date = platform::epoch_to_iso_utc(it.time_deleted);
                    let original = original_of(&it);
                    let res = if let Some(parts) = &format {
                        let id = Path::new(&it.id);
                        // The shell's display name drops known extensions;
                        // the original path keeps the real file name.
                        let name = original
                            .file_name()
                            .map(|n| n.to_string_lossy())
                            .unwrap_or_else(|| it.name.to_string_lossy());
                        let row = crate::listfmt::FmtRow {
                            date: &date,
                            epoch: it.time_deleted,
                            path: &original,
                            name: &name,
                            trash: id.parent().unwrap_or(id),
                            batch: "",
                            size: Some(item_size(&it)),
                            owner: Some(""),
                        };
                        out.write_all(&crate::listfmt::render_format(parts, &row))
                    } else if with_size {
                        writeln!(
                            out,
                            "{} {:>10} {}",
                            date.replacen('T', " ", 1),
                            crate::util::format_bytes(item_size(&it)),
                            crate::util::term_path(&original)
                        )
                    } else {
                        writeln!(
                            out,
                            "{}",
                            platform::format_deletion_list_line(Some(&date), &original)
                        )
                    };
                    let payload = Path::new(&it.id);
                    let res = res
                        .and_then(|()| crate::listfmt::trash_line(&mut out, payload, show_trash))
                        .and_then(|()| write_tree(&mut out, payload, depth));
                    if res.is_err() {
                        return 0;
                    }
                }
                let _ = out.flush();
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
        let mut policy = crate::budget::Policy::Oldest;
        let mut every = false;
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
                "--all" => every = true,
                a if a.starts_with("--max-count=") => {
                    match crate::permanent_guard::max_count_arg(a) {
                        Some(Ok(n)) => crate::permanent_guard::set_max_count(n),
                        _ => {
                            eprintln!(
                                "{prog}: --max-count takes a whole number of items, not '{a}'"
                            );
                            return 2;
                        }
                    }
                }
                a if a.starts_with("--max-size=") => {
                    match crate::filters::parse_size(&a["--max-size=".len()..]) {
                        Some(n) => max_size = Some(n),
                        None => {
                            eprintln!("{prog}: invalid SIZE in '{a}'");
                            return 2;
                        }
                    }
                }
                a if a.starts_with("--prune=") => {
                    match crate::budget::Policy::parse(&a["--prune=".len()..]) {
                        Some(p) => policy = p,
                        None => {
                            eprintln!("{prog}: --prune takes oldest or largest");
                            return 2;
                        }
                    }
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
      --max-count=N  refuse, deleting nothing, when more than N items
                   would go (also RTRASH_MAX_PERMANENT)
      --all      let an empty with no DAYS, --max-size or --until-free
                   remove items other puts left; without it such an
                   empty asks on a terminal and refuses elsewhere unless
                   every item came from the latest put
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
        let all = match super::list_items() {
            Ok(i) => i,
            Err(e) => {
                eprintln!("{prog}: cannot list Recycle Bin: {}", err_msg(e));
                return 1;
            }
        };
        // An empty of everything may take what other sessions or agents
        // put; it goes ahead alone only for the latest put's items.
        if age.older_than.is_none() && max_size.is_none() && !every && !dry_run {
            let rows: Vec<(PathBuf, i64)> = all
                .iter()
                .map(|it| (original_of(it), it.time_deleted))
                .collect();
            let (since, last) = last_put_path()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|t| crate::budget::parse_last_put(&t))
                .unwrap_or((i64::MAX, Vec::new()));
            let mounts = super::volume_mounts();
            let rows: Vec<(PathBuf, i64)> = rows
                .into_iter()
                .map(|(p, t)| (key_of(&p, &mounts), t))
                .collect();
            let last: Vec<PathBuf> = last.iter().map(|p| key_of(p, &mounts)).collect();
            let foreign = crate::budget::not_from_last_put(&rows, &last, since);
            if !crate::budget::guard_ok(prog, rows.len(), foreign) {
                return 2;
            }
        }
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
            let largest = policy == crate::budget::Policy::Largest;
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
        if let Some(code) = crate::permanent_guard::check_count(prog, items.len() as u64) {
            return code;
        }
        if verbose && !json {
            for it in &items {
                println!("removed {}", crate::util::term_path(&original_of(it)));
            }
        }
        // Sizes come from the records, so read them before the purge.
        if json {
            summary(&items);
        }
        if let Err(e) = crate::permanent_guard::gate_many(items.len() as u64) {
            eprintln!("{prog}: {e}");
            return 1;
        }
        match os_limited::purge_all(items) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("{prog}: cannot empty Recycle Bin: {}", err_msg(e));
                1
            }
        }
    }

    /// `%LOCALAPPDATA%\\rtrash\\last-put.txt`, the record the empty guard
    /// reads.
    fn last_put_path() -> Option<PathBuf> {
        let base = std::env::var_os("LOCALAPPDATA")?;
        Some(PathBuf::from(base).join("rtrash").join("last-put.txt"))
    }

    /// The Recycle Bin items the latest put moved, by the last-put record.
    fn last_put_items(prog: &str, items: &[TrashItem]) -> Result<Vec<TrashItem>, i32> {
        let Some((since, last)) = last_put_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| crate::budget::parse_last_put(&t))
        else {
            eprintln!("{prog}: no put run recorded");
            return Err(1);
        };
        let rows: Vec<(PathBuf, i64)> = items
            .iter()
            .map(|it| (original_of(it), it.time_deleted))
            .collect();
        let mounts = super::volume_mounts();
        let rows: Vec<(PathBuf, i64)> = rows
            .into_iter()
            .map(|(p, t)| (key_of(&p, &mounts), t))
            .collect();
        let want: Vec<PathBuf> = last.iter().map(|p| key_of(p, &mounts)).collect();
        let keep = crate::budget::from_last_put(&rows, &want, since);
        let picked: Vec<TrashItem> = keep.into_iter().map(|i| items[i].clone()).collect();
        if picked.is_empty() {
            eprintln!("{prog}: nothing from the latest put is left in the Recycle Bin");
            for p in &last {
                eprintln!("{prog}:   the latest put moved '{}'", p.display());
            }
            explain_no_match(prog, items, since);
            return Err(1);
        }
        if picked.len() < want.len() {
            eprintln!(
                "{prog}: only {} of the {} items from the latest put are left in the Recycle Bin",
                picked.len(),
                want.len()
            );
        }
        Ok(picked)
    }

    /// `path_key` as a path, for matching.
    fn key_of(p: &Path, mounts: &[(String, String)]) -> PathBuf {
        PathBuf::from(super::path_key(&p.to_string_lossy(), mounts))
    }

    /// When nothing matched: what the Recycle Bin holds, as each `$I`
    /// record names it, so a path form that does not compare shows up.
    fn explain_no_match(prog: &str, items: &[TrashItem], since: i64) {
        let mounts = super::volume_mounts();
        eprintln!("{prog}: the Recycle Bin holds {} items", items.len());
        for it in items.iter().filter(|it| it.time_deleted >= since) {
            let o = original_of(it);
            eprintln!(
                "{prog}:   original '{}' (compared as '{}', payload '{}')",
                o.display(),
                key_of(&o, &mounts).display(),
                it.id.to_string_lossy()
            );
        }
    }

    fn write_last_put(since: i64, moved: &[PathBuf]) {
        let Some(p) = last_put_path() else { return };
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(&p, crate::budget::last_put_text(since, moved));
    }

    /// With a trash budget (`RTRASH_MAX_SIZE` or `policy.max_size`), trim
    /// the Recycle Bins together back under it, never touching the items
    /// this put moved (matched by original path); they still count.
    fn prune_after_put(prog: &str, moved: &[PathBuf], quiet: bool) {
        let Some((max, policy, source)) = crate::budget::budget(prog) else {
            return;
        };
        let Ok(all) = super::list_items() else {
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
        if let Err(e) = crate::permanent_guard::gate_many(n) {
            if !quiet {
                eprintln!("{prog}: {e}");
            }
            return;
        }
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
        let items: Vec<TrashItem> = match super::list_items() {
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
            if let Err(e) = crate::permanent_guard::gate_many(n) {
                eprintln!("{prog}: {e}");
                return (0, 0, 0);
            }
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
        for it in super::list_items().unwrap_or_default() {
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
        let joined = match crate::filters::join_spaced(args) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{prog}: {e}");
                return 2;
            }
        };
        let args = &joined[..];
        let mut conflict = Conflict::Refuse;
        let mut json = false;
        let mut plain = false;
        let mut newest_first = false;
        let mut target: Option<PathBuf> = None;
        let mut last_run = false;
        let mut dry_run = false;
        let mut matcher: Option<crate::budget::PathMatch> = None;
        // `--match=KIND`: the PATH operand is a pattern of that kind.
        let mut kind: Option<crate::filters::Match> = None;
        let mut operand: Option<String> = None;
        let mut latest = false;
        let mut all = false;
        let mut to: Option<PathBuf> = None;
        let mut sort: Option<TableSort> = None;
        let mut cwd_only = false;
        let mut directory: Option<PathBuf> = None;
        // Run as trash-restore, scope to the current directory as trash-cli does.
        let trash_cli = prog == "trash-restore";
        let mut age = crate::filters::AgeFilter::default();
        let mut size = crate::filters::SizeFilter::default();
        for arg in args {
            match arg.as_str() {
                "-f" | "--force" => conflict = Conflict::Overwrite,
                a if a.starts_with("--older-than=") || a.starts_with("--newer-than=") => {
                    let (name, value) = a.split_once('=').unwrap_or((a, ""));
                    match crate::filters::parse_age_secs(value) {
                        Ok(d) if name == "--older-than" => age.older_than = Some(d),
                        Ok(d) => age.newer_than = Some(d),
                        Err(e) => {
                            eprintln!("{prog}: {e}");
                            return 2;
                        }
                    }
                }
                a if a.starts_with("--larger-than=") || a.starts_with("--smaller-than=") => {
                    let (name, value) = a.split_once('=').unwrap_or((a, ""));
                    let Some(n) = crate::filters::parse_size(value) else {
                        eprintln!("{prog}: invalid size '{value}' for {name}");
                        return 2;
                    };
                    if name == "--larger-than" {
                        size.larger_than = Some(n);
                    } else {
                        size.smaller_than = Some(n);
                    }
                }
                "--latest" => latest = true,
                "--all" => all = true,
                a if a.starts_with("--to=") => {
                    let d = &a["--to=".len()..];
                    if d.is_empty() {
                        eprintln!("{prog}: --to needs a non-empty DIR");
                        return 2;
                    }
                    let d = crate::util::arg_path(d);
                    // A relative --to counts from here, as the shell would.
                    to = Some(std::path::absolute(&d).unwrap_or(d));
                }
                a if crate::budget::PathMatch::parse(a).is_some() => {
                    match crate::budget::PathMatch::parse(a) {
                        Some(Ok(_)) if matcher.is_some() => {
                            eprintln!("{prog}: give only one of --name, --glob and --regex");
                            return 2;
                        }
                        Some(Ok(m)) => matcher = Some(m),
                        Some(Err(e)) => {
                            eprintln!("{prog}: {e}");
                            return 2;
                        }
                        None => {}
                    }
                }
                "-n" | "--dry-run" => {
                    dry_run = true;
                    plain = true;
                }
                "--json" => {
                    json = true;
                    plain = true;
                }
                a if a.starts_with("--match=") || a.starts_with("-m=") => {
                    match crate::filters::match_arg(a) {
                        Some(Ok(k)) => kind = Some(k),
                        Some(Err(e)) => {
                            eprintln!("{prog}: {e}");
                            return 2;
                        }
                        None => {}
                    }
                }
                a if a.starts_with("--conflict=") => {
                    let v = &a["--conflict=".len()..];
                    match Conflict::parse(v) {
                        Some(c) => conflict = c,
                        None => {
                            eprintln!(
                                "{prog}: invalid --conflict '{v}' (refuse, overwrite, rename or skip)"
                            );
                            return 2;
                        }
                    }
                }
                "--batch=last" => last_run = true,
                a if a.starts_with("--batch=") => {
                    eprintln!(
                        "{prog}: the Recycle Bin keeps only the latest put run; use --batch=last"
                    );
                    return 2;
                }
                "--cwd-only" => {
                    cwd_only = true;
                    plain = true;
                }
                a if a.starts_with("--directory=") => {
                    match directory_arg(&a["--directory=".len()..]) {
                        Ok(d) => directory = Some(d),
                        Err(e) => {
                            eprintln!("{prog}: {e}");
                            return 2;
                        }
                    }
                    plain = true;
                }
                "--plain" | "--home-only" => plain = true,
                a if a.starts_with("--sort=") => match &a["--sort=".len()..] {
                    "date" => sort = Some(TableSort::Date),
                    "size" => sort = Some(TableSort::Size),
                    "path" => sort = Some(TableSort::Path),
                    "none" => sort = Some(TableSort::None),
                    v => {
                        eprintln!("{prog}: invalid --sort '{v}' (date, size, path or none)");
                        return 2;
                    }
                },
                "--newest-first" => newest_first = true,
                "--oldest-first" => newest_first = false,
                "--no-space-check" => crate::space::skip_check(true),
                "--help" => {
                    print!(
                        "\
Usage: {prog} [OPTION]... [PATH]
Restore an item from the Windows Recycle Bin to its original location.

With PATH, restore the item whose original path equals PATH.
--name, --glob and --regex match many items at once. When several items
match, --latest restores the most recently deleted one and --all restores
every one, both without asking.
Without PATH, print a numbered list and read an index from stdin.

  -f, --force   overwrite when the destination exists (best-effort)
      --conflict=POLICY  when the destination exists: refuse (default),
                overwrite, rename (restore next to it as NAME.1.EXT)
                or skip
      --name=NAME  match items whose file name is NAME
      --glob=PATTERN  match items whose original path (PATTERN with a
                separator) or file name matches a shell glob
      --regex=RE  match items whose original path matches RE
  -m, --match=KIND  read PATH as a pattern of KIND, as trashy's -m does:
                glob, regex, substring or exact (the whole original path)
      --to=DIR  restore into DIR, keeping each item's file name
      --older-than=AGE  only items deleted more than AGE ago; AGE is DAYS
                or a duration such as 2h, 30m or 1d12h
      --newer-than=AGE  only items deleted within the last AGE
      --larger-than=SIZE  only items over SIZE (500M, 1.5G, 64k)
      --smaller-than=SIZE  only items under SIZE
      --latest  of several matches, restore the most recently deleted
      --all     of several matches, restore every one
      --plain   numbered selection instead of the browser
  -n, --dry-run show where each chosen item would go and change nothing
      --json    print {{restored, failed, skipped}} as JSON on stdout;
                implies --plain, and the numbered list goes to stderr
      --cwd-only  without PATH or a match, only items deleted from the
                current directory or below (implies --plain)
      --directory=DIR  as --cwd-only, for items deleted from under DIR;
                DIR need not exist any more (implies --plain)
      --sort=ORDER  numbered list order: date (oldest first, default),
                size (largest first), path (original path) or none
                (as read)
      --newest-first  list the most recently deleted item first
      --oldest-first  list the oldest deleted item first (default)
      --batch=last  restore every item the latest put moved, by the
                record put keeps in %LOCALAPPDATA%\\rtrash
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
                    operand = Some(a.to_string());
                    target = Some(crate::util::arg_path(a));
                }
            }
        }
        if let Some(k) = kind {
            let Some(pat) = operand.as_deref() else {
                eprintln!("{prog}: --match needs a PATTERN");
                return 2;
            };
            if matcher.is_some() {
                eprintln!(
                    "{prog}: give --match with a PATTERN or a --name, --glob or --regex match"
                );
                return 2;
            }
            match crate::budget::PathMatch::of_kind(k, pat) {
                Ok(m) => matcher = Some(m),
                Err(e) => {
                    eprintln!("{prog}: {e}");
                    return 2;
                }
            }
            target = None;
        }
        if latest && all {
            eprintln!("{prog}: give --latest or --all, not both");
            return 2;
        }
        if let Err(e) = size.check() {
            eprintln!("{prog}: {e}");
            return 2;
        }
        if cwd_only && directory.is_some() {
            eprintln!("{prog}: give --cwd-only or --directory, not both");
            return 2;
        }
        if target.is_some() && matcher.is_some() {
            eprintln!("{prog}: give PATH or a --name, --glob or --regex match, not both");
            return 2;
        }
        if let Some(d) = &to {
            if !d.is_dir() {
                eprintln!("{prog}: --to: '{}' is not a directory", d.display());
                return 2;
            }
        }
        let opts = RestoreOpts {
            conflict,
            json,
            dry_run,
            to: to.clone(),
        };
        let mut items = match super::list_items() {
            Ok(i) => i,
            Err(e) => {
                eprintln!("{prog}: cannot list Recycle Bin: {}", err_msg(e));
                return 1;
            }
        };
        // --older-than and --newer-than, as list reads them.
        if age.is_set() {
            let now = crate::util::now_epoch();
            items.retain(|i| age.keeps_at(Some(i.time_deleted), now));
        }
        // --larger-than and --smaller-than; only the items left get sized.
        if size.is_set() {
            items.retain(|i| size.keeps(item_size(i)));
        }
        if last_run {
            if target.is_some() {
                eprintln!("{prog}: give PATH or --batch=last, not both");
                return 2;
            }
            return match last_put_items(prog, &items) {
                Ok(run) => restore_one(prog, run, &opts),
                Err(c) => c,
            };
        }
        let mut matches: Vec<TrashItem> = match &target {
            Some(t) => {
                let mounts = super::volume_mounts();
                let abs = std::path::absolute(t).unwrap_or_else(|_| t.clone());
                let want: Vec<PathBuf> = path_forms(&abs)
                    .iter()
                    .map(|p| key_of(p, &mounts))
                    .collect();
                // trash-restore takes PATH as a scope, as trash-cli does:
                // the item there and everything trashed from under it.
                let scope = if trash_cli {
                    dir_forms(&abs)
                } else {
                    Vec::new()
                };
                let matched: Vec<TrashItem> = items
                    .iter()
                    .filter(|i| {
                        let o = original_of(i);
                        want.contains(&key_of(&o, &mounts))
                            || scope.iter().any(|d| o.starts_with(d))
                    })
                    .cloned()
                    .collect();
                if matched.is_empty() {
                    eprintln!("{prog}: no trashed files match");
                    if json {
                        println!("{}", restore_json(&[], 0, 0, dry_run));
                    }
                    if std::env::var_os("RTRASH_DEBUG").is_some() {
                        for w in &want {
                            eprintln!("{prog}:   wanted '{}'", w.display());
                        }
                        explain_no_match(prog, &items, i64::MIN);
                    }
                    return 1;
                }
                matched
            }
            None => match &matcher {
                Some(m) => items
                    .into_iter()
                    .filter(|i| m.keeps(&original_of(i)))
                    .collect(),
                // Without PATH or a match, --cwd-only keeps the items that
                // came from here or below, as on FreeDesktop.
                // The directory as given and as resolved both count, since
                // a volume mounted on a folder may resolve to its GUID path.
                None if cwd_only || trash_cli || directory.is_some() => {
                    let dirs = match &directory {
                        Some(d) => dir_forms(d),
                        None => cwd_forms(),
                    };
                    if dirs.is_empty() {
                        eprintln!("{prog}: cannot read the current directory");
                        return 1;
                    }
                    items
                        .into_iter()
                        .filter(|i| {
                            let o = original_of(i);
                            dirs.iter().any(|d| o.starts_with(d))
                        })
                        .collect()
                }
                None => items,
            },
        };
        if matches.is_empty() && trash_cli && target.is_none() && matcher.is_none() {
            let cwd = std::env::current_dir().unwrap_or_default();
            println!("No files trashed from current dir ('{}')", cwd.display());
            return 1;
        }
        if matches.is_empty() {
            eprintln!("{prog}: no trashed files match");
            if json {
                println!("{}", restore_json(&[], 0, 0, dry_run));
            }
            return 1;
        }
        let as_read = (sort == Some(TableSort::None)).then(|| matches.clone());
        matches.sort_by_key(|it| it.time_deleted);
        if latest {
            let newest = matches.pop().into_iter().collect();
            return restore_one(prog, newest, &opts);
        }
        // trash-cli asks even when one item matches.
        if all || (matches.len() == 1 && !trash_cli) {
            return restore_one(prog, matches, &opts);
        }
        match sort {
            Some(TableSort::Path) => matches.sort_by_key(original_of),
            // Largest first; matches are oldest first, so ties keep that.
            Some(TableSort::Size) => {
                matches.sort_by_cached_key(|i| std::cmp::Reverse(item_size(i)));
            }
            Some(TableSort::None) => matches = as_read.unwrap_or(matches),
            Some(TableSort::Date) | None => {
                if newest_first {
                    matches.reverse();
                }
            }
        }
        #[cfg(feature = "tui")]
        if !plain && to.is_none() && util::stdin_is_tty() {
            let items: Vec<BinItem> = matches.into_iter().map(BinItem::new).collect();
            let refs: Vec<&BinItem> = items.iter().collect();
            return crate::restore_tui::run(
                prog,
                refs,
                conflict == Conflict::Overwrite,
                newest_first,
            );
        }
        let _ = plain;
        // Numbered pick.
        for (i, it) in matches.iter().enumerate() {
            // trash-cli shows the deletion time in local time.
            let date = if trash_cli {
                platform::epoch_to_local(it.time_deleted)
            } else {
                platform::epoch_to_iso_utc(it.time_deleted)
            };
            // trash-cli prints `   0 2026-01-02 03:04:05 PATH`: a width-4
            // index and one space between the columns.
            let row = if trash_cli {
                format!(
                    "{i:4} {}",
                    platform::format_deletion_list_line(
                        Some(&date.replacen('T', " ", 1)),
                        &original_of(it)
                    )
                )
            } else {
                format!(
                    "  {i} {}",
                    platform::format_deletion_list_line(Some(&date), &original_of(it))
                )
            };
            // Keep stdout for the JSON summary.
            if json {
                eprintln!("{row}");
            } else {
                println!("{row}");
            }
        }
        let ask = if trash_cli {
            "What file to restore"
        } else {
            "What to restore"
        };
        // A piped answer gets no prompt, as on FreeDesktop.
        if util::stdin_is_tty() {
            eprint!("{ask} [0..{}]: ", matches.len() - 1);
        }
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return 1;
        }
        if line.trim().is_empty() && trash_cli {
            println!("No files were restored");
            return 0;
        }
        if line.trim().is_empty() {
            eprintln!(
                "{prog}: {} matches; provide an index 0..{} on stdin",
                matches.len(),
                matches.len() - 1
            );
            return 1;
        }
        // A pick like `0-2,4`, as on FreeDesktop.
        let picked = match crate::util::parse_selection(&line, matches.len()) {
            Ok(idx) => idx.into_iter().map(|i| matches[i].clone()).collect(),
            Err(e) => {
                eprintln!("{prog}: invalid selection: {e}");
                return 1;
            }
        };
        restore_one(prog, picked, &opts)
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
                crate::permanent_guard::remove(dest)
                    .map_err(|e| format!("cannot replace '{}': {e}", dest.display()))?;
            }
            super::restore_payload(Path::new(&self.item.id), dest)
                .map(|()| (dest.clone(), Vec::new()))
                .map_err(|e| format!("restore failed: {e}"))
        }
    }

    /// When a restore finds its destination taken, as on FreeDesktop.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Conflict {
        Refuse,
        Overwrite,
        Rename,
        Skip,
    }

    impl Conflict {
        fn parse(s: &str) -> Option<Self> {
            Some(match s {
                "refuse" => Conflict::Refuse,
                "overwrite" => Conflict::Overwrite,
                "rename" => Conflict::Rename,
                "skip" => Conflict::Skip,
                _ => return None,
            })
        }
    }

    /// `--sort`: the numbered list order, as trash-cli's trash-restore
    /// takes it.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum TableSort {
        Date,
        Size,
        Path,
        None,
    }

    struct RestoreOpts {
        conflict: Conflict,
        json: bool,
        dry_run: bool,
        /// `--to=DIR`, absolute: restore there, keeping each file name.
        to: Option<PathBuf>,
    }

    /// The first `STEM.N.EXT` (or `NAME.N`) next to `dest` that is free.
    fn free_name(dest: &Path) -> PathBuf {
        let parent = dest.parent().unwrap_or(Path::new(""));
        let stem = dest.file_stem().unwrap_or_default().to_string_lossy();
        let ext = dest.extension().map(|e| e.to_string_lossy());
        (1u64..)
            .map(|n| {
                let name = match &ext {
                    Some(x) => format!("{stem}.{n}.{x}"),
                    None => format!("{stem}.{n}"),
                };
                parent.join(name)
            })
            .find(|p| p.symlink_metadata().is_err())
            .unwrap_or_else(|| dest.to_path_buf())
    }

    /// `{restored, failed, skipped}` as the FreeDesktop restore prints it,
    /// with `"dry_run":true` first for a dry run.
    fn restore_json(
        done: &[(PathBuf, PathBuf)],
        failed: usize,
        skipped: usize,
        dry: bool,
    ) -> String {
        let rows: Vec<String> = done
            .iter()
            .map(|(orig, dest)| {
                format!(
                    "{{\"original\":\"{}\",\"restored_to\":\"{}\"}}",
                    util::json_escape(&orig.to_string_lossy()),
                    util::json_escape(&dest.to_string_lossy())
                )
            })
            .collect();
        let head = if dry { "\"dry_run\":true," } else { "" };
        format!(
            "{{{head}\"restored\":[{}],\"failed\":{failed},\"skipped\":{skipped}}}",
            rows.join(",")
        )
    }

    /// Restore `items` under `o`: each taken destination is refused,
    /// overwritten, renamed or skipped by itself, as on FreeDesktop. A dry
    /// run says what each would do and moves nothing.
    fn restore_one(prog: &str, items: Vec<TrashItem>, o: &RestoreOpts) -> i32 {
        // A line for the user: stderr when stdout carries the JSON.
        let say = |line: String| {
            if o.json {
                eprintln!("{line}");
            } else {
                println!("{line}");
            }
        };
        let mut plan: Vec<(&TrashItem, PathBuf, PathBuf)> = Vec::new();
        let (mut failed, mut skipped) = (0usize, 0usize);
        for it in &items {
            let orig = original_of(it);
            let base = match (&o.to, orig.file_name()) {
                (Some(dir), Some(name)) => dir.join(name),
                (Some(_), None) => {
                    eprintln!(
                        "{prog}: cannot restore '{}' by name: no file name",
                        orig.display()
                    );
                    failed += 1;
                    continue;
                }
                (None, _) => orig.clone(),
            };
            let taken = base.symlink_metadata().is_ok();
            let dest = match (taken, o.conflict) {
                (true, Conflict::Skip) => {
                    if o.dry_run {
                        say(format!("would skip '{}': it exists", base.display()));
                    } else {
                        eprintln!("{prog}: skipping '{}': it exists", base.display());
                    }
                    skipped += 1;
                    continue;
                }
                (true, Conflict::Refuse) => {
                    if o.dry_run {
                        say(format!("would fail: '{}' exists", base.display()));
                    } else {
                        eprintln!(
                            "{prog}: refusing to overwrite '{}' (use -f)",
                            base.display()
                        );
                    }
                    failed += 1;
                    continue;
                }
                (true, Conflict::Rename) => free_name(&base),
                _ => base,
            };
            if o.dry_run {
                if Path::new(&it.id).symlink_metadata().is_err() {
                    say(format!(
                        "would fail: the payload of '{}' is gone",
                        orig.display()
                    ));
                    failed += 1;
                    continue;
                }
                if dest == orig {
                    say(format!("would restore '{}'", orig.display()));
                } else {
                    say(format!(
                        "would restore '{}' as '{}'",
                        orig.display(),
                        dest.display()
                    ));
                }
            }
            plan.push((it, orig, dest));
        }
        if o.dry_run {
            let done: Vec<(PathBuf, PathBuf)> = plan
                .into_iter()
                .map(|(_, orig, dest)| (orig, dest))
                .collect();
            if o.json {
                println!("{}", restore_json(&done, failed, skipped, true));
            }
            return i32::from(failed > 0);
        }
        // RTRASH_ALLOW_ROOTS: every destination in scope, or nothing moves.
        let dests: Vec<PathBuf> = plan.iter().map(|(_, _, dest)| dest.clone()).collect();
        if let Some(code) = crate::allow_roots::refuse_outside(prog, &dests) {
            return code;
        }
        let pairs: Vec<(PathBuf, PathBuf, u64)> = plan
            .iter()
            .map(|(it, _, dest)| (PathBuf::from(&it.id), dest.clone(), item_size(it)))
            .collect();
        for (dest, need) in super::cross_volume_needs(&pairs, super::bin_root_of) {
            if let Err(e) = crate::space::guard_bytes(need, &dest) {
                eprintln!("{prog}: cannot restore: {e}");
                if o.json {
                    println!("{}", restore_json(&[], failed + plan.len(), skipped, false));
                }
                return 1;
            }
        }
        let mut done: Vec<(PathBuf, PathBuf)> = Vec::new();
        for (it, orig, dest) in plan {
            if o.conflict == Conflict::Overwrite && dest.symlink_metadata().is_ok() {
                let gone = crate::permanent_guard::remove(&dest);
                if let Err(e) = gone {
                    eprintln!("{prog}: cannot replace '{}': {e}", dest.display());
                    failed += 1;
                    continue;
                }
            }
            if let Err(e) = super::restore_payload(Path::new(&it.id), &dest) {
                eprintln!("{prog}: cannot restore '{}': {e}", dest.display());
                failed += 1;
            } else if dest.symlink_metadata().is_err() {
                eprintln!(
                    "{prog}: restored '{}' but nothing is there now",
                    dest.display()
                );
                failed += 1;
            } else {
                done.push((orig, dest));
            }
        }
        if o.json {
            println!("{}", restore_json(&done, failed, skipped, false));
        }
        i32::from(failed > 0)
    }

    pub fn status_run(prog: &str, args: &[String]) -> i32 {
        let (mut check, mut fix, mut dry_run, mut json) = (false, false, false, false);
        let filters = parse_with_filters(prog, args, |arg| match arg {
            "--json" => {
                json = true;
                None
            }
            "--check" => {
                check = true;
                None
            }
            "--fix" => {
                fix = true;
                None
            }
            "-n" | "--dry-run" => {
                dry_run = true;
                None
            }
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
      --json     emit a JSON object (scripts): one root per volume, with
                     orphans and dangling null, cap_bytes and skips_bin
      --check    check each $I record instead: list records whose $R
                     payload is gone, records that cannot be parsed and
                     payloads with no record; exit 1 when it finds any
      --fix      with --check, remove the records whose payload is gone
                     or that cannot be parsed (a payload stays)
  -n, --dry-run  with --fix, print what it would remove
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
        if (fix || dry_run) && !check {
            eprintln!("{prog}: --fix and --dry-run need --check");
            return 2;
        }
        if check {
            if filters.is_set() || json {
                eprintln!("{prog}: --check takes only --fix and -n");
                return 2;
            }
            let Some(dirs) = check_dirs() else {
                eprintln!("{prog}: cannot read the user's SID");
                return 1;
            };
            return super::check_bins(prog, &dirs, fix, dry_run);
        }
        if filters.is_set() {
            return filtered_status(prog, &filters, json);
        }
        // One line per volume with a bin or a cap, like the per-trash lines
        // of the FreeDesktop status, then the total. A volume mounted on a
        // folder keeps a Recycle Bin of its own, so it gets a line too.
        let (mut items, mut bytes) = (0u64, 0u64);
        let mut rows = Vec::new();
        for root in crate::trash_history::bin_roots() {
            let Some((n, b)) = query_bin(&root) else {
                continue;
            };
            let quota = bin_quota(&root);
            let note = cap_note(b, &quota);
            if n == 0 && note.is_empty() {
                continue;
            }
            if json {
                rows.push(super::BinStatusRow {
                    root: root.display().to_string(),
                    items: n,
                    bytes: b,
                    quota,
                    space: crate::space::query(&root).ok(),
                });
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
        if json {
            print!("{}", super::bin_status_json(&rows));
            return 0;
        }
        println!(
            "Total: {} ({})",
            crate::util::plural(items, "item", "items"),
            crate::util::format_bytes(bytes)
        );
        print_free(&crate::trash_history::bin_roots());
        0
    }

    /// The current user's Recycle Bin folders that exist, one per drive
    /// and per volume mounted on a folder. Test seam, in debug builds
    /// only: `RTRASH_TEST_BIN_DIR=DIR` checks DIR alone, as such a folder,
    /// so a test never reads or changes the real bin. None without a SID.
    fn check_dirs() -> Option<Vec<PathBuf>> {
        let seam = std::env::var_os("RTRASH_TEST_BIN_DIR").filter(|_| cfg!(debug_assertions));
        if let Some(d) = seam {
            return Some(vec![PathBuf::from(d)]);
        }
        let sid = super::current_user_sid()?;
        Some(
            crate::trash_history::bin_roots()
                .into_iter()
                .map(|r| r.join("$Recycle.Bin").join(&sid))
                .filter(|d| d.is_dir())
                .collect(),
        )
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
    fn filtered_status(prog: &str, filters: &BinFilters, json: bool) -> i32 {
        let items = match super::list_items() {
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
        if json {
            let rows: Vec<super::BinStatusRow> = by_drive
                .iter()
                .map(|(root, &(items, bytes))| super::BinStatusRow {
                    root: root.display().to_string(),
                    items,
                    bytes,
                    quota: bin_quota(root),
                    space: crate::space::query(root).ok(),
                })
                .collect();
            print!("{}", super::bin_status_json(&rows));
            return 0;
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
        let mut kind = crate::filters::Match::Glob;
        let mut bad_match: Option<String> = None;
        let mut json = false;
        let mut patterns: Vec<String> = Vec::new();
        let mut batch: Option<String> = None;
        let filters = parse_with_filters(prog, args, |arg| {
            match arg {
                "-n" | "--dry-run" => dry_run = true,
                "--regex" => kind = crate::filters::Match::Regex,
                a if a.starts_with("--match=") => match crate::filters::Match::parse(&a[8..]) {
                    Ok(k) => kind = k,
                    Err(e) => bad_match = Some(e),
                },
                "--json" => json = true,
                "-f" | "--force" => force = true,
                a if a.starts_with("--max-count=") => {
                    match crate::permanent_guard::max_count_arg(a) {
                        Some(Ok(n)) => crate::permanent_guard::set_max_count(n),
                        _ => {
                            eprintln!(
                                "{prog}: --max-count takes a whole number of items, not '{a}'"
                            );
                            return Some(2);
                        }
                    }
                }
                a if a.starts_with("--batch=") => batch = Some(a["--batch=".len()..].to_string()),
                "-v" | "--verbose" => verbose = true,
                "--home-only" => {}
                "--help" => {
                    print!(
                        "\
Usage: {prog} [OPTION]... PATTERN...
Permanently delete Recycle Bin items matching PATTERN (glob).

      --regex   PATTERNs are regular expressions searched in the original
                   path (the same as --match=regex)
      --match=KIND  how to read PATTERNs (trashy's -m): glob (default),
                   regex, substring or exact (the whole original path)
      --older-than=AGE  only match items deleted more than AGE ago: DAYS,
                   or a duration such as 2h, 30m or 1d12h
      --newer-than=AGE  only match items deleted within the last AGE
      --larger-than=SIZE  only match items over SIZE (500M, 1.5G, 64k)
      --smaller-than=SIZE only match items under SIZE
      --batch=last  only the items the latest put moved (Windows keeps
                   that one run); with no PATTERN, all of them
  -n, --dry-run  list matches only
  -f, --force    allow mass patterns (e.g. '*')
      --max-count=N  refuse, deleting nothing, when more than N items
                   would go (also RTRASH_MAX_PERMANENT)
  -v, --verbose  print each path
      --json     print a JSON summary (and the matched originals)
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
        if let Some(b) = &batch {
            if b != "last" {
                eprintln!(
                    "{prog}: the Recycle Bin keeps only the latest put run; use --batch=last"
                );
                return 2;
            }
        }
        // The latest put is a scope of its own: with no PATTERN, take all.
        let scoped = batch.is_some() && patterns.is_empty();
        if let Some(e) = bad_match {
            eprintln!("{prog}: {e}");
            return 2;
        }
        if scoped {
            kind = crate::filters::Match::Glob;
            patterns.push("*".into());
        }
        if patterns.is_empty() {
            eprintln!("{prog}: missing PATTERN");
            return 2;
        }
        let pats = match crate::filters::Patterns::new(patterns, kind) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{prog}: {e}");
                return 2;
            }
        };
        if pats.is_mass() && !force && !scoped {
            eprintln!("{prog}: refusing a pattern that matches everything without --force");
            return 2;
        }
        let items = match super::list_items() {
            Ok(i) => i,
            Err(e) => {
                eprintln!("{prog}: cannot list Recycle Bin: {}", err_msg(e));
                return 1;
            }
        };
        let items = if batch.is_some() {
            match last_put_items(prog, &items) {
                Ok(p) => p,
                Err(c) => return c,
            }
        } else {
            items
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
                pats.hit(&full, &base, &base)
            })
            .collect();
        // Size last: only the matches need their `$I` read.
        let matched = filters.apply(matched, |it| it.time_deleted, item_size);
        // Taken before the purge: the `$R` payloads are gone after it.
        let summary = json.then(|| {
            let names: Vec<String> = matched
                .iter()
                .map(|it| original_of(it).to_string_lossy().into_owned())
                .collect();
            let sum = crate::filters::RmSummary {
                dry_run,
                removed: matched.len() as u64,
                bytes: matched.iter().map(item_size).sum(),
            };
            (sum, names)
        });
        if !dry_run {
            // RTRASH_ALLOW_ROOTS: rm deletes only items trashed from there.
            let origins: Vec<PathBuf> = matched.iter().map(original_of).collect();
            if let Some(code) = crate::allow_roots::refuse_outside(prog, &origins) {
                return code;
            }
            if let Some(code) = crate::permanent_guard::check_count(prog, matched.len() as u64) {
                return code;
            }
        }
        if !json && (dry_run || verbose) {
            for it in &matched {
                println!("{}", crate::util::term_path(&original_of(it)));
            }
        }
        let code = if dry_run {
            0
        } else if let Err(e) = crate::permanent_guard::gate_many(matched.len() as u64) {
            eprintln!("{prog}: {e}");
            1
        } else {
            match os_limited::purge_all(matched) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("{prog}: purge failed: {}", err_msg(e));
                    1
                }
            }
        };
        if let Some((sum, names)) = summary {
            print!("{}", sum.json(&names));
        }
        code
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

/// One Recycle Bin volume for `status --json`.
pub struct BinStatusRow {
    /// The volume root, such as `C:\`, or the folder it is mounted on.
    pub root: String,
    pub items: u64,
    pub bytes: u64,
    pub quota: BinQuota,
    /// The volume's free space; None leaves the `fs_*` fields out.
    pub space: Option<crate::space::Space>,
}

/// One volume as a `status --json` root. The shell's counts give no
/// orphans and no dangling records, so both are null. The extras are
/// `cap_bytes`, the `MaxCapacity` cap in bytes (null when unset, as
/// Windows then applies its own default share), and `skips_bin`, true
/// when deletes on the volume skip the bin (`NukeOnDelete`).
pub fn bin_status_root(row: &BinStatusRow) -> crate::status_json::Root<'_> {
    crate::status_json::Root {
        path: &row.root,
        items: row.items,
        bytes: row.bytes,
        orphans: None,
        dangling: None,
        space: row.space,
        extra: format!(
            ",\"cap_bytes\":{},\"skips_bin\":{}",
            crate::status_json::count(row.quota.max_bytes),
            row.quota.nuke_on_delete
        ),
    }
}

/// `status --json` for Recycle Bin volumes, in the order given, with
/// their totals.
pub fn bin_status_json(rows: &[BinStatusRow]) -> String {
    let roots: Vec<crate::status_json::Root> = rows.iter().map(bin_status_root).collect();
    let items = rows.iter().map(|r| r.items).fold(0u64, u64::saturating_add);
    let bytes = rows.iter().map(|r| r.bytes).fold(0u64, u64::saturating_add);
    crate::status_json::document(&roots, items, bytes)
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
    fn bin_status_json_has_the_status_schema_with_nulls_and_cap_fields() {
        let rows = [
            BinStatusRow {
                root: "C:\\".into(),
                items: 3,
                bytes: 300,
                quota: BinQuota {
                    max_bytes: Some(4096),
                    nuke_on_delete: false,
                },
                space: Some(crate::space::Space {
                    total: 1000,
                    avail: 50,
                    inodes: None,
                    inodes_avail: None,
                }),
            },
            BinStatusRow {
                root: "D:\\mnt".into(),
                items: u64::MAX,
                bytes: 7,
                quota: BinQuota {
                    max_bytes: None,
                    nuke_on_delete: true,
                },
                space: None,
            },
        ];
        let doc = bin_status_json(&rows);
        let lines: Vec<&str> = doc.lines().collect();
        assert_eq!(
            lines[2],
            "    {\"path\":\"C:\\\\\",\"items\":3,\"bytes\":300,\"orphans\":null,\"dangling\":null,\
             \"fs_bytes\":1000,\"fs_free_bytes\":50,\"fs_inodes\":null,\"fs_free_inodes\":null,\
             \"fs_low\":true,\"cap_bytes\":4096,\"skips_bin\":false},"
        );
        assert_eq!(
            lines[3],
            "    {\"path\":\"D:\\\\mnt\",\"items\":18446744073709551615,\"bytes\":7,\
             \"orphans\":null,\"dangling\":null,\"cap_bytes\":null,\"skips_bin\":true}"
        );
        // Totals saturate rather than wrap.
        assert_eq!(lines[5], "  \"total_items\": 18446744073709551615,");
        assert_eq!(lines[6], "  \"total_bytes\": 307");
        assert_eq!(
            bin_status_json(&[]),
            crate::status_json::document(&[], 0, 0)
        );
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

    #[test]
    fn restore_payload_keeps_the_extension_and_drops_the_index() {
        let top = std::env::temp_dir().join(format!("rtrash-rp-ext-{}", std::process::id()));
        let bin = top.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let payload = bin.join("$RAB12CD.txt");
        let index = bin.join("$IAB12CD.txt");
        std::fs::write(&payload, b"hello").unwrap();
        std::fs::write(&index, b"meta").unwrap();
        let dest = top.join("sub").join("notes.txt");
        restore_payload(&payload, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
        assert!(!payload.exists());
        assert!(!index.exists());
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn restore_payload_moves_a_directory() {
        let top = std::env::temp_dir().join(format!("rtrash-rp-dir-{}", std::process::id()));
        std::fs::create_dir_all(&top).unwrap();
        let payload = top.join("$RAB12CD");
        std::fs::create_dir(&payload).unwrap();
        std::fs::write(payload.join("a.txt"), b"a").unwrap();
        let dest = top.join("dir");
        restore_payload(&payload, &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"a");
        assert!(!payload.exists());
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn path_key_reads_a_volume_guid_as_its_mount_point() {
        let mounts = vec![(
            r"\\?\Volume{1234-ab}\".to_string(),
            r"C:\rtrash-mnt\".to_string(),
        )];
        assert_eq!(
            path_key(r"\\?\Volume{1234-AB}\Run-A.txt", &mounts),
            path_key(r"C:\rtrash-mnt\run-a.txt", &mounts)
        );
        assert_eq!(
            path_key(r"\\?\Volume{1234-ab}\d\x.txt", &mounts),
            r"c:\rtrash-mnt\d\x.txt"
        );
    }

    #[test]
    fn path_key_strips_verbatim_and_case() {
        assert_eq!(path_key(r"\\?\C:\Users\A\B.TXT", &[]), r"c:\users\a\b.txt");
        assert_eq!(path_key(r"\\?\UNC\srv\share\f", &[]), r"\\srv\share\f");
        assert_eq!(path_key("C:/x/y/", &[]), r"c:\x\y");
        assert_eq!(path_key(r"C:\", &[]), r"c:\");
    }

    /// A `$I` record whose name holds an unpaired surrogate (valid on
    /// NTFS) as raw little-endian units.
    fn record_with_lone_surrogate() -> Vec<u8> {
        let units: Vec<u16> = r"C:\d\a"
            .encode_utf16()
            .chain([0xD800])
            .chain(".txt".encode_utf16())
            .collect();
        let mut b = vec![0u8; 0x1C];
        b[0] = 2;
        b[0x18..0x1C].copy_from_slice(&(units.len() as u32 + 1).to_le_bytes());
        for u in units.iter().chain([&0u16]) {
            b.extend_from_slice(&u.to_le_bytes());
        }
        b
    }

    #[cfg(windows)]
    #[test]
    fn lone_surrogate_name_parses_exactly_on_windows() {
        use std::os::windows::ffi::OsStrExt;
        let m = parse_recycle_i(&record_with_lone_surrogate()).expect("parsed");
        let units: Vec<u16> = m.original.as_os_str().encode_wide().collect();
        assert_eq!(units[6], 0xD800);
        assert!(m.original.to_string_lossy().ends_with(".txt"));
    }

    #[cfg(not(windows))]
    #[test]
    fn lone_surrogate_name_is_skipped_off_windows() {
        assert!(parse_recycle_i(&record_with_lone_surrogate()).is_none());
    }

    #[test]
    fn scan_bin_dir_pairs_records_with_payloads() {
        let dir = std::env::temp_dir().join(format!("rtrash-scanbin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ft = |s: u64| FILETIME_UNIX_EPOCH + s * 10_000_000;
        let put = |id: &str, orig: &str, secs: u64| {
            std::fs::write(dir.join(format!("$R{id}")), b"x").unwrap();
            let rec = encode_recycle_i_v2(Path::new(orig), 1, ft(secs));
            std::fs::write(dir.join(format!("$I{id}")), rec).unwrap();
        };
        put("BBBBBB.txt", r"C:\b\new.txt", 200);
        put("AAAAAA.txt", r"C:\a\old.txt", 100);
        // A record without its payload, a payload without its record and
        // a damaged record are skipped; the rest still list.
        std::fs::write(
            dir.join("$ICCCCCC.txt"),
            encode_recycle_i_v2(Path::new(r"C:\gone.txt"), 1, ft(50)),
        )
        .unwrap();
        std::fs::write(dir.join("$RDDDDDD"), b"x").unwrap();
        std::fs::write(dir.join("$REEEEEE"), b"x").unwrap();
        std::fs::write(dir.join("$IEEEEEE"), b"junk").unwrap();
        std::fs::write(dir.join("desktop.ini"), b"x").unwrap();
        let got = scan_bin_dir(&dir);
        let originals: Vec<String> = got
            .iter()
            .map(|r| r.original.to_string_lossy().into_owned())
            .collect();
        assert_eq!(originals, [r"C:\a\old.txt", r"C:\b\new.txt"]);
        assert_eq!(got[0].payload, dir.join("$RAAAAAA.txt"));
        assert_eq!(got[0].deleted, 100);
        assert!(scan_bin_dir(&dir.join("missing")).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `status --check` on a Recycle Bin folder: the records in name
    /// order, then the orphans; a check and a dry run change nothing, and
    /// `--fix` removes only the dangling and bad records, so their
    /// payloads turn up as orphans.
    #[test]
    fn check_bin_dir_sorts_problems_and_fix_removes_only_records() {
        let dir = std::env::temp_dir().join(format!("rtrash-checkbin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let w = |name: &str, body: &[u8]| std::fs::write(dir.join(name), body).unwrap();
        let rec = |orig: &str| encode_recycle_i_v2(Path::new(orig), 1, FILETIME_UNIX_EPOCH);
        w("$RAAAAAA.txt", b"x");
        w("$IAAAAAA.txt", &rec(r"C:\a.txt"));
        w("$RBBBBBB", b"x");
        w("$IBBBBBB", &[0u8; I_RECORD_CAP as usize + 1]);
        w("$ICCCCCC.txt", &rec(r"C:\gone.txt"));
        w("$RDDDDDD", b"x");
        w("$REEEEEE", b"x");
        w("$IEEEEEE", b"junk");
        std::fs::create_dir(dir.join("$IFFFFFF")).unwrap();
        w("desktop.ini", b"x");
        let bad = |name: &str, why: &str| BinProblem::BadRecord {
            record: dir.join(name),
            why: why.to_string(),
        };
        let found = vec![
            bad("$IBBBBBB", "larger than any $I record"),
            BinProblem::Dangling {
                record: dir.join("$ICCCCCC.txt"),
                origin: PathBuf::from(r"C:\gone.txt"),
            },
            bad("$IEEEEEE", "cannot be parsed as a $I record"),
            BinProblem::Stray {
                path: dir.join("$IFFFFFF"),
            },
            BinProblem::Orphan {
                payload: dir.join("$RDDDDDD"),
            },
        ];
        assert_eq!(check_bin_dir(&dir), found);
        let prefixes: Vec<String> = found
            .iter()
            .map(|p| p.line().split(':').next().unwrap_or("").to_string())
            .collect();
        assert_eq!(
            prefixes,
            ["bad record", "dangling", "bad record", "stray", "orphan"]
        );
        let fixable = found.iter().filter(|p| p.fix_target().is_some()).count();
        assert_eq!(fixable, 3);
        let dirs = [dir.clone()];
        assert_eq!(check_bins("t", &dirs, false, false), 1);
        assert_eq!(check_bins("t", &dirs, true, true), 1);
        assert_eq!(check_bin_dir(&dir), found);
        // The guards are the integration tests' business; a set one here
        // would refuse the fix.
        let guarded = [
            "RTRASH_NO_PERMANENT",
            "RTRASH_MAX_PERMANENT",
            "RTRASH_ALLOW_ROOTS",
        ]
        .iter()
        .any(|v| std::env::var_os(v).is_some_and(|x| !x.is_empty()));
        if !guarded {
            assert_eq!(check_bins("t", &dirs, true, false), 0);
            let orphan = |name: &str| BinProblem::Orphan {
                payload: dir.join(name),
            };
            assert_eq!(
                check_bin_dir(&dir),
                [
                    BinProblem::Stray {
                        path: dir.join("$IFFFFFF")
                    },
                    orphan("$RBBBBBB"),
                    orphan("$RDDDDDD"),
                    orphan("$REEEEEE"),
                ]
            );
            assert!(dir.join("$IAAAAAA.txt").exists() && dir.join("desktop.ini").exists());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn scan_bin_dir_keeps_a_payload_name_that_is_not_text() {
        use std::os::unix::ffi::OsStrExt;
        let dir = std::env::temp_dir().join(format!("rtrash-scanbin-raw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let r = dir.join(std::ffi::OsStr::from_bytes(b"$RABCDEF.\xe9"));
        let i = dir.join(std::ffi::OsStr::from_bytes(b"$IABCDEF.\xe9"));
        // APFS and some other file systems refuse names that are not
        // UTF-8 (EILSEQ); there is nothing to check then.
        if let Err(e) = std::fs::write(&r, b"x") {
            if e.raw_os_error() == Some(libc::EILSEQ)
                || e.kind() == std::io::ErrorKind::InvalidInput
            {
                eprintln!("skipped: the file system refused the name: {e}");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
            panic!("cannot write {r:?}: {e}");
        }
        std::fs::write(
            &i,
            encode_recycle_i_v2(Path::new(r"C:\odd"), 1, FILETIME_UNIX_EPOCH),
        )
        .unwrap();
        let got = scan_bin_dir(&dir);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].payload, r);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
