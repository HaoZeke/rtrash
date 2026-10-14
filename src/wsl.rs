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

/// Whether the user asked for the route (env first, then the config file).
pub fn wanted() -> bool {
    match std::env::var("RTRASH_WSL_RECYCLE").ok().as_deref() {
        Some("1" | "true" | "yes") => true,
        Some(_) => false,
        None => crate::config::get().bool("wsl", "recycle").unwrap_or(false),
    }
}

/// True for a path on a Windows drive under the default automount root:
/// `/mnt/<letter>/...`, the drive itself excluded.
pub fn on_windows_drive(abs: &Path) -> bool {
    let mut c = abs.components();
    use std::path::Component::{Normal, RootDir};
    matches!(
        (c.next(), c.next(), c.next(), c.next()),
        (Some(RootDir), Some(Normal(m)), Some(Normal(d)), Some(_))
            if m == "mnt" && d.len() == 1 && d.to_string_lossy().chars().all(|ch| ch.is_ascii_alphabetic())
    )
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
        assert!(on_windows_drive(Path::new("/mnt/c/Users/me/a.txt")));
        assert!(on_windows_drive(Path::new("/mnt/D/x")));
        assert!(!on_windows_drive(Path::new("/mnt/c")));
        assert!(!on_windows_drive(Path::new("/mnt/usb/x")));
        assert!(!on_windows_drive(Path::new("/home/me/mnt/c/x")));
        assert!(!on_windows_drive(Path::new("/mnt/1/x")));
    }
}
