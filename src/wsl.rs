//! WSL: an opt-in route for files on Windows drives (`/mnt/c/...`) to the
//! Windows Recycle Bin, so Explorer can restore them. Off unless
//! `RTRASH_WSL_RECYCLE=1` or `[wsl] recycle = true`, and only when the
//! kernel says WSL; everything else keeps the FreeDesktop trash.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The kernel release names Microsoft on WSL 1 and 2.
/// `RTRASH_WSL_OSRELEASE` points the check at another file (tests).
pub fn is_wsl() -> bool {
    let src = std::env::var_os("RTRASH_WSL_OSRELEASE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/proc/sys/kernel/osrelease"));
    std::fs::read_to_string(src).is_ok_and(|s| s.to_ascii_lowercase().contains("microsoft"))
}

/// The user's choice for the route (env first, then the config file);
/// None when neither says.
fn choice() -> Option<bool> {
    match std::env::var("RTRASH_WSL_RECYCLE").ok().as_deref() {
        Some("1" | "true" | "yes") => Some(true),
        Some(_) => Some(false),
        None => crate::config::get().bool("wsl", "recycle"),
    }
}

/// Whether the user asked for the route.
pub fn wanted() -> bool {
    choice().unwrap_or(false)
}

/// True once per run when a Windows-drive item goes to the FreeDesktop
/// trash on WSL with no choice made: put then names the opt-in.
pub fn hint_due(abs: &Path) -> bool {
    static SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if choice().is_some() || !on_windows_drive(abs) || !is_wsl() {
        return false;
    }
    !SHOWN.swap(true, std::sync::atomic::Ordering::Relaxed)
}

/// The `[automount] root` from `/etc/wsl.conf` (`RTRASH_WSL_CONF` points
/// elsewhere, for tests), else `/mnt/`, where WSL mounts Windows drives.
pub fn automount_root() -> PathBuf {
    let conf = std::env::var_os("RTRASH_WSL_CONF")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/wsl.conf"));
    std::fs::read_to_string(conf)
        .ok()
        .and_then(|t| automount_root_from(&t))
        .unwrap_or_else(|| PathBuf::from("/mnt"))
}

/// `root` in the `[automount]` section of a wsl.conf text, when set.
pub fn automount_root_from(text: &str) -> Option<PathBuf> {
    let mut section = String::new();
    for line in text.lines() {
        let line = line.split(['#', ';']).next().unwrap_or("").trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.trim().to_ascii_lowercase();
            continue;
        }
        if section != "automount" {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim().eq_ignore_ascii_case("root") {
                let v = v.trim().trim_matches('"').trim();
                if v.starts_with('/') {
                    return Some(PathBuf::from(v));
                }
            }
        }
    }
    None
}

/// True for a path on a Windows drive under `root`: `ROOT/<letter>/...`,
/// the drive itself excluded.
pub fn on_drive_under(root: &Path, abs: &Path) -> bool {
    let Ok(rest) = abs.strip_prefix(root) else {
        return false;
    };
    let mut c = rest.components();
    use std::path::Component::Normal;
    matches!(
        (c.next(), c.next()),
        (Some(Normal(d)), Some(_))
            if d.len() == 1 && d.to_string_lossy().chars().all(|ch| ch.is_ascii_alphabetic())
    )
}

/// True for a path on a Windows drive under the automount root.
pub fn on_windows_drive(abs: &Path) -> bool {
    on_drive_under(&automount_root(), abs)
}

/// Route `abs` to the Recycle Bin when the user asked and it applies.
pub fn applies(abs: &Path) -> bool {
    wanted() && on_windows_drive(abs) && is_wsl()
}

/// Send `abs` to the Recycle Bin through `powershell.exe`. The Windows
/// path travels in an environment variable (shared through `WSLENV`),
/// so no quoting reaches the PowerShell command line.
pub fn recycle(abs: &Path, is_dir: bool) -> Result<(), String> {
    let out = Command::new("wslpath")
        .arg("-w")
        .arg(abs)
        .output()
        .map_err(|e| format!("cannot run wslpath: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "wslpath failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let win = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let call = if is_dir {
        "DeleteDirectory"
    } else {
        "DeleteFile"
    };
    let script = format!(
        "Add-Type -AssemblyName Microsoft.VisualBasic; \
         [Microsoft.VisualBasic.FileIO.FileSystem]::{call}($env:RTRASH_WSL_TARGET, 'OnlyErrorDialogs', 'SendToRecycleBin')"
    );
    let wslenv = match std::env::var("WSLENV") {
        Ok(v) if !v.is_empty() => format!("{v}:RTRASH_WSL_TARGET"),
        _ => "RTRASH_WSL_TARGET".to_string(),
    };
    let st = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .env("RTRASH_WSL_TARGET", &win)
        .env("WSLENV", wslenv)
        .output()
        .map_err(|e| format!("cannot run powershell.exe: {e}"))?;
    if !st.status.success() {
        return Err(format!(
            "powershell.exe failed: {}",
            String::from_utf8_lossy(&st.stderr).trim()
        ));
    }
    if std::fs::symlink_metadata(abs).is_ok() {
        return Err("the Recycle Bin call returned, but the item is still there".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_paths_inside_a_drive_letter_mount_qualify() {
        let m = Path::new("/mnt");
        assert!(on_drive_under(m, Path::new("/mnt/c/Users/me/a.txt")));
        assert!(on_drive_under(m, Path::new("/mnt/D/x")));
        assert!(!on_drive_under(m, Path::new("/mnt/c")));
        assert!(!on_drive_under(m, Path::new("/mnt/usb/x")));
        assert!(!on_drive_under(m, Path::new("/home/me/mnt/c/x")));
        assert!(!on_drive_under(m, Path::new("/mnt/1/x")));
        assert!(on_drive_under(Path::new("/"), Path::new("/c/x")));
        assert!(!on_drive_under(Path::new("/win"), Path::new("/mnt/c/x")));
    }

    #[test]
    fn wsl_conf_sets_the_automount_root() {
        let t = "[boot]\nsystemd=true\n[automount]\nenabled = true\nroot = /win/ # here\n";
        assert_eq!(automount_root_from(t), Some(PathBuf::from("/win/")));
        assert_eq!(
            automount_root_from("[automount]\nroot=\"/\"\n"),
            Some(PathBuf::from("/"))
        );
        assert_eq!(automount_root_from("[network]\nroot = /x\n"), None);
        assert_eq!(automount_root_from("[automount]\nroot = relative\n"), None);
    }
}
