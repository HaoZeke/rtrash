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
            "macOS FreeDesktop home (experimental; plain commands default to the Finder Trash)"
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
        Some(d) if !d.is_empty() => format!(
            "{} {}",
            crate::util::term_text(d),
            crate::util::term_path(original)
        ),
        _ => format!("? {}", crate::util::term_path(original)),
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

/// `epoch` as local wall-clock time, `YYYY-MM-DDTHH:MM:SS`, through the
/// current Windows time zone (trash-cli shows local time). Falls back to
/// UTC when Windows cannot convert it.
#[cfg(windows)]
pub fn epoch_to_local(epoch: i64) -> String {
    local_in(epoch, None)
}

/// [`epoch_to_local`] in `zone`, or the current zone for `None`.
#[cfg(windows)]
fn local_in(
    epoch: i64,
    zone: Option<&windows_sys::Win32::System::Time::TIME_ZONE_INFORMATION>,
) -> String {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::Time::SystemTimeToTzSpecificLocalTime;
    let iso = epoch_to_iso_utc(epoch);
    let num = |r: std::ops::Range<usize>| iso.get(r).and_then(|t| t.parse::<u16>().ok());
    let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(s)) = (
        num(0..4),
        num(5..7),
        num(8..10),
        num(11..13),
        num(14..16),
        num(17..19),
    ) else {
        return iso;
    };
    let utc = SYSTEMTIME {
        wYear: y,
        wMonth: mo,
        wDayOfWeek: 0,
        wDay: d,
        wHour: h,
        wMinute: mi,
        wSecond: s,
        wMilliseconds: 0,
    };
    // SAFETY: SYSTEMTIME is plain data, so all zeroes is a valid value.
    let mut local: SYSTEMTIME = unsafe { std::mem::zeroed() };
    let zone = zone.map_or(std::ptr::null(), |z| z as *const _);
    // SAFETY: both SYSTEMTIME pointers name live values, and the zone is
    // null (the current one) or a live TIME_ZONE_INFORMATION.
    if unsafe { SystemTimeToTzSpecificLocalTime(zone, &utc, &mut local) } == 0 {
        return iso;
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute, local.wSecond
    )
}

#[cfg(all(test, windows))]
mod local_tests {
    use super::*;

    #[test]
    fn local_in_applies_the_zone_bias() {
        use windows_sys::Win32::System::Time::TIME_ZONE_INFORMATION;
        // SAFETY: TIME_ZONE_INFORMATION is plain data; a zero StandardDate
        // means the zone has no daylight saving time.
        let mut zone: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
        // Bias is minutes west of UTC: -90 is UTC+01:30, 300 is UTC-05:00.
        zone.Bias = -90;
        assert_eq!(local_in(0, Some(&zone)), "1970-01-01T01:30:00");
        zone.Bias = 300;
        assert_eq!(local_in(86_400, Some(&zone)), "1970-01-01T19:00:00");
        // The current zone still gives the trashinfo layout.
        let now = epoch_to_local(1_700_000_000);
        assert_eq!(now.len(), 19, "{now}");
        assert_eq!(&now[10..11], "T", "{now}");
    }
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
