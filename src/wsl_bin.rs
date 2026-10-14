//! WSL: list and restore the items in the Windows Recycle Bin from inside
//! WSL (`rtrash wsl list`, `rtrash wsl restore PATH`), so items the
//! Recycle Bin route sent stay within reach.
//!
//! Each drive keeps its bin at `X:\$Recycle.Bin\SID`, which WSL shows
//! under the automount root (`/mnt/x/$Recycle.Bin/SID`). Listing reads
//! the `$I` records there directly, so names keep their extension and
//! nothing on the Windows side changes. Restore moves the `$R` payload
//! back to its original path and removes the `$I` record, the same steps
//! as rtrash on Windows. Only the user's SID needs `powershell.exe`, once.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::win_recycle::{filetime_to_unix_epoch, parse_recycle_i, restore_payload};

/// One Recycle Bin item as WSL sees it.
#[derive(Debug, Clone)]
pub struct BinEntry {
    /// The `$R` payload, under the automount root.
    pub payload: PathBuf,
    /// The original Windows path from the `$I` record.
    pub original: String,
    /// The original path under the automount root.
    pub local: PathBuf,
    /// Deletion time, Unix seconds.
    pub deleted: i64,
    /// Size the `$I` record gives, in bytes.
    pub size: u64,
}

/// `C:\a\b` as `ROOT/c/a/b`; none for a share or anything else that is
/// not a drive path.
pub fn win_to_wsl(root: &Path, win: &str) -> Option<PathBuf> {
    let b = win.as_bytes();
    if b.len() < 3 || !b[0].is_ascii_alphabetic() || b[1] != b':' || b[2] != b'\\' {
        return None;
    }
    let mut p = root.join(char::from(b[0].to_ascii_lowercase()).to_string());
    for part in win[3..].split('\\').filter(|s| !s.is_empty()) {
        if part == "." || part == ".." {
            return None;
        }
        p.push(part);
    }
    Some(p)
}

/// The user's Windows SID: `RTRASH_WSL_SID` (tests), else one call to
/// `powershell.exe`.
pub fn user_sid() -> Result<String, String> {
    if let Ok(s) = std::env::var("RTRASH_WSL_SID") {
        return Ok(s);
    }
    let out = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value",
        ])
        .output()
        .map_err(|e| format!("cannot run powershell.exe: {e}"))?;
    let sid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || !sid.starts_with("S-1-") {
        return Err(format!(
            "cannot read the Windows user id: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(sid)
}

/// Every item in the user's Recycle Bins on the drives under `root`,
/// oldest first. A record without its payload, or one that does not
/// parse, is skipped.
pub fn scan(root: &Path, sid: &str) -> Vec<BinEntry> {
    let mut out = Vec::new();
    let Ok(drives) = std::fs::read_dir(root) else {
        return out;
    };
    for d in drives.flatten() {
        let name = d.file_name();
        let n = name.to_string_lossy();
        if n.len() != 1 || !n.as_bytes()[0].is_ascii_alphabetic() {
            continue;
        }
        let bin = d.path().join("$Recycle.Bin").join(sid);
        let Ok(rd) = std::fs::read_dir(&bin) else {
            continue;
        };
        for e in rd.flatten() {
            let fname = e.file_name();
            let Some(rest) = fname.to_str().and_then(|s| s.strip_prefix("$I")) else {
                continue;
            };
            let payload = bin.join(format!("$R{rest}"));
            if std::fs::symlink_metadata(&payload).is_err() {
                continue;
            }
            let Some(meta) = std::fs::read(e.path())
                .ok()
                .and_then(|b| parse_recycle_i(&b))
            else {
                continue;
            };
            let original = meta.original.to_string_lossy().into_owned();
            let Some(local) = win_to_wsl(root, &original) else {
                continue;
            };
            out.push(BinEntry {
                payload,
                original,
                local,
                deleted: filetime_to_unix_epoch(meta.delete_filetime),
                size: meta.size,
            });
        }
    }
    out.sort_by(|a, b| a.deleted.cmp(&b.deleted).then(a.local.cmp(&b.local)));
    out
}

const USAGE: &str = "\
Usage: rtrash wsl list
       rtrash wsl restore [-f] PATH...
The Windows Recycle Bin from inside WSL: list its items by original path
under the automount root, or restore the newest item from each PATH.
  -f, --force   restore over an existing file
  -h, --help    show this help
";

/// `rtrash wsl ...`.
pub fn run(prog: &str, args: &[String]) -> i32 {
    let Some(cmd) = args.first().map(String::as_str) else {
        eprint!("{USAGE}");
        return 2;
    };
    if matches!(cmd, "-h" | "--help") {
        print!("{USAGE}");
        return 0;
    }
    if !matches!(cmd, "list" | "restore") {
        eprintln!("{prog}: unknown action '{cmd}'");
        eprint!("{USAGE}");
        return 2;
    }
    if !crate::wsl::is_wsl() {
        eprintln!("{prog}: not running under WSL");
        return 1;
    }
    let sid = match user_sid() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{prog}: {e}");
            return 1;
        }
    };
    let root = crate::wsl::automount_root();
    let items = scan(&root, &sid);
    if cmd == "list" {
        if args.len() > 1 {
            eprintln!("{prog}: list takes no arguments");
            return 2;
        }
        for it in &items {
            let date = crate::platform::epoch_to_iso_utc(it.deleted);
            println!(
                "{}",
                crate::platform::format_deletion_list_line(Some(&date), &it.local)
            );
        }
        return 0;
    }
    let mut force = false;
    let mut paths = Vec::new();
    for a in &args[1..] {
        match a.as_str() {
            "-f" | "--force" => force = true,
            s if s.starts_with('-') && s != "-" => {
                eprintln!("{prog}: unknown option '{s}'");
                return 2;
            }
            _ => paths.push(crate::util::arg_path(a)),
        }
    }
    if paths.is_empty() {
        eprintln!("{prog}: restore needs a PATH");
        return 2;
    }
    let mut status = 0;
    for p in paths {
        let abs = std::path::absolute(&p).unwrap_or_else(|_| p.clone());
        let Some(it) = items
            .iter()
            .filter(|i| i.local == abs)
            .max_by_key(|i| i.deleted)
        else {
            eprintln!(
                "{prog}: '{}' is not in the Recycle Bin of its drive",
                abs.display()
            );
            status = 1;
            continue;
        };
        if std::fs::symlink_metadata(&it.local).is_ok() {
            if !force {
                eprintln!(
                    "{prog}: refusing to overwrite '{}' (use -f)",
                    it.local.display()
                );
                status = 1;
                continue;
            }
            let gone = if std::fs::symlink_metadata(&it.local).is_ok_and(|m| m.is_dir()) {
                std::fs::remove_dir_all(&it.local)
            } else {
                std::fs::remove_file(&it.local)
            };
            if let Err(e) = gone {
                eprintln!("{prog}: cannot replace '{}': {e}", it.local.display());
                status = 1;
                continue;
            }
        }
        match restore_payload(&it.payload, &it.local) {
            Ok(()) if std::fs::symlink_metadata(&it.local).is_ok() => {}
            Ok(()) => {
                eprintln!(
                    "{prog}: restored '{}' but nothing is there now",
                    it.local.display()
                );
                status = 1;
            }
            Err(e) => {
                eprintln!("{prog}: cannot restore '{}': {e}", it.local.display());
                status = 1;
            }
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_paths_map_under_the_automount_root() {
        let r = Path::new("/mnt");
        assert_eq!(
            win_to_wsl(r, r"C:\Users\me\a b.txt"),
            Some(PathBuf::from("/mnt/c/Users/me/a b.txt"))
        );
        assert_eq!(
            win_to_wsl(Path::new("/"), r"D:\x"),
            Some(PathBuf::from("/d/x"))
        );
        assert_eq!(win_to_wsl(r, r"\\srv\share\x"), None);
        assert_eq!(win_to_wsl(r, r"C:\a\..\b"), None);
        assert_eq!(win_to_wsl(r, "C:"), None);
    }
}
