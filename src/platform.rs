//! Platform trash backend selection and pure path helpers.
//!
//! - **Linux / other Unix (not macOS):** FreeDesktop home + per-mount trash.
//! - **macOS (experimental):** FreeDesktop *home* trash only (`XDG_DATA_HOME/Trash`
//!   or `~/.local/share/Trash`). This is **not** Finder / system Trash.
//! - **Windows:** system Recycle Bin via shell APIs (see `win_recycle`), **not**
//!   a FreeDesktop on-disk layout pretending to be Recycle Bin.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Which on-disk / OS trash backend this build uses at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrashBackend {
    /// FreeDesktop.org Trash (home + volume `.Trash-$uid` / `.Trash/$uid`).
    FreeDesktop,
    /// Experimental FreeDesktop home trash on macOS (not Finder Trash).
    MacosFdoExperimental,
    /// Windows system Recycle Bin (shell / `$Recycle.Bin`), not FreeDesktop.
    WindowsRecycleBin,
}

/// Backend for the current target OS (compile-time selection).
pub const fn trash_backend() -> TrashBackend {
    #[cfg(target_os = "macos")]
    {
        TrashBackend::MacosFdoExperimental
    }
    #[cfg(windows)]
    {
        TrashBackend::WindowsRecycleBin
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        TrashBackend::FreeDesktop
    }
}

/// Human-readable backend name for status/docs.
pub fn trash_backend_label() -> &'static str {
    match trash_backend() {
        TrashBackend::FreeDesktop => "FreeDesktop",
        TrashBackend::MacosFdoExperimental => {
            "macOS FreeDesktop home (experimental; not Finder Trash)"
        }
        TrashBackend::WindowsRecycleBin => "Windows Recycle Bin",
    }
}

/// Resolve the FreeDesktop-style home trash root from env values.
///
/// Spec: `$XDG_DATA_HOME/Trash` when set and non-empty; else `$HOME/.local/share/Trash`.
/// Used on Linux and for the experimental macOS FDO path. Not used on Windows.
pub fn home_trash_from(xdg_data_home: Option<&OsStr>, home: Option<&OsStr>) -> PathBuf {
    let data = xdg_data_home
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let h = home.map(PathBuf::from).unwrap_or_default();
            h.join(".local/share")
        });
    data.join("Trash")
}

/// True when this build uses experimental macOS FreeDesktop home-only policy
/// (no volume `.Trash-$uid` discovery).
pub const fn is_macos_fdo_experimental() -> bool {
    matches!(trash_backend(), TrashBackend::MacosFdoExperimental)
}

/// True when put/list/restore/empty must use the Windows Recycle Bin backend.
pub const fn is_windows_recycle() -> bool {
    matches!(trash_backend(), TrashBackend::WindowsRecycleBin)
}

/// Whether volume (per-mount) FreeDesktop trash discovery is enabled.
pub const fn volume_trash_discovery() -> bool {
    matches!(trash_backend(), TrashBackend::FreeDesktop)
}

/// Format a list line as `YYYY-MM-DDTHH:MM:SS original` (local wall when possible).
/// Pure helper shared by FreeDesktop list and Windows recycle list.
pub fn format_deletion_list_line(date: Option<&str>, original: &Path) -> String {
    match date {
        Some(d) if !d.is_empty() => format!("{d} {}", original.display()),
        _ => format!("? {}", original.display()),
    }
}

/// Convert unix epoch seconds to a FreeDesktop-style local date string best-effort.
/// Pure (no tz database): uses UTC formatting so tests are stable; list still
/// prefers `.trashinfo` strings when present.
pub fn epoch_to_iso_utc(epoch: i64) -> String {
    // Manual UTC calendar for stable host-free tests (no chrono dep).
    // Algorithm from civil_from_days (Howard Hinnant).
    let z = epoch.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = y + if m <= 2 { 1 } else { 0 };
    let secs = epoch.rem_euclid(86_400) as u32;
    let hh = secs / 3600;
    let mm = (secs % 3600) / 60;
    let ss = secs % 60;
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn home_trash_prefers_xdg_data_home() {
        let p = home_trash_from(Some(OsStr::new("/custom/xdg")), Some(OsStr::new("/home/u")));
        assert_eq!(p, PathBuf::from("/custom/xdg/Trash"));
    }

    #[test]
    fn home_trash_falls_back_to_home_local_share() {
        let p = home_trash_from(None, Some(OsStr::new("/Users/me")));
        assert_eq!(p, PathBuf::from("/Users/me/.local/share/Trash"));
    }

    #[test]
    fn home_trash_ignores_empty_xdg() {
        let p = home_trash_from(Some(OsStr::new("")), Some(OsStr::new("/home/u")));
        assert_eq!(p, PathBuf::from("/home/u/.local/share/Trash"));
    }

    #[test]
    fn macos_experimental_backend_const() {
        // On Linux CI this is FreeDesktop; the const still typechecks.
        let b = trash_backend();
        assert!(matches!(
            b,
            TrashBackend::FreeDesktop
                | TrashBackend::MacosFdoExperimental
                | TrashBackend::WindowsRecycleBin
        ));
        let _ = trash_backend_label();
        let _ = is_macos_fdo_experimental();
        let _ = volume_trash_discovery();
    }

    #[test]
    fn format_list_line_and_epoch() {
        let line = format_deletion_list_line(Some("2024-01-02T03:04:05"), Path::new("/a/b"));
        assert_eq!(line, "2024-01-02T03:04:05 /a/b");
        let u = epoch_to_iso_utc(0);
        assert_eq!(u, "1970-01-01T00:00:00");
        let _ = OsString::new();
    }
}
