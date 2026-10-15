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

use crate::win_recycle::{restore_payload, scan_bin_dir};

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
        for r in scan_bin_dir(&bin) {
            let original = r.original.to_string_lossy().into_owned();
            let Some(local) = win_to_wsl(root, &original) else {
                continue;
            };
            out.push(BinEntry {
                payload: r.payload,
                original,
                local,
                deleted: r.deleted,
                size: r.size,
            });
        }
    }
    out.sort_by(|a, b| a.deleted.cmp(&b.deleted).then(a.local.cmp(&b.local)));
    out
}

const USAGE: &str = "\
Usage: rtrash wsl list [--json] [MATCH]
       rtrash wsl restore [-f] PATH...
       rtrash wsl rm [-n] MATCH|PATH...
The Windows Recycle Bin from inside WSL: list its items by original path
under the automount root, restore the newest item from each PATH, or
delete items for good. MATCH is --glob=PATTERN, --regex=RE or --name=NAME,
tested against the original path as WSL sees it.
      --json             list: a JSON array
      --glob=PATTERN     items whose path (or name, without a slash) matches
      --regex=RE         items whose path matches RE
      --name=NAME        items with exactly this file name
  -n, --dry-run          rm: print what would go, delete nothing
  -f, --force            restore over an existing file
  -h, --help             show this help
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
    if !matches!(cmd, "list" | "restore" | "rm") {
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
    match cmd {
        "list" => return list(prog, &args[1..], &items),
        "rm" => return rm(prog, &args[1..], &items),
        _ => {}
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

/// Take one `--glob`, `--regex` or `--name` from `a`: Ok(false) when `a` is no
/// matcher, Err(exit code) after an error message.
fn take_matcher(
    prog: &str,
    a: &str,
    slot: &mut Option<crate::budget::PathMatch>,
) -> Result<bool, i32> {
    match crate::budget::PathMatch::parse(a) {
        Some(Ok(m)) if slot.is_none() => {
            *slot = Some(m);
            Ok(true)
        }
        Some(Ok(_)) => {
            eprintln!("{prog}: give one of --glob, --regex and --name");
            Err(2)
        }
        Some(Err(e)) => {
            eprintln!("{prog}: {e}");
            Err(2)
        }
        None => Ok(false),
    }
}

/// `wsl list [--json] [MATCH]`.
fn list(prog: &str, args: &[String], items: &[BinEntry]) -> i32 {
    let mut json = false;
    let mut matcher = None;
    for a in args {
        if a == "--json" {
            json = true;
            continue;
        }
        match take_matcher(prog, a, &mut matcher) {
            Ok(true) => {}
            Ok(false) => {
                eprintln!("{prog}: list: unknown argument '{a}'");
                return 2;
            }
            Err(c) => return c,
        }
    }
    let shown: Vec<&BinEntry> = items
        .iter()
        .filter(|it| matcher.as_ref().is_none_or(|m| m.keeps(&it.local)))
        .collect();
    if json {
        println!("{}", list_json(&shown));
        return 0;
    }
    for it in shown {
        let date = crate::platform::epoch_to_iso_utc(it.deleted);
        println!(
            "{}",
            crate::platform::format_deletion_list_line(Some(&date), &it.local)
        );
    }
    0
}

/// The `wsl list --json` array: original Windows path, path under the
/// automount root, deletion time (epoch and ISO date) and size.
pub fn list_json(items: &[&BinEntry]) -> String {
    let rows: Vec<String> = items
        .iter()
        .map(|it| {
            format!(
                r#"{{"original":"{}","path":"{}","deleted":{},"date":"{}","size":{}}}"#,
                crate::util::json_escape(&it.original),
                crate::util::json_escape(&it.local.to_string_lossy()),
                it.deleted,
                crate::platform::epoch_to_iso_utc(it.deleted),
                it.size
            )
        })
        .collect();
    format!("[{}]", rows.join(","))
}

/// `wsl rm [-n] MATCH|PATH...`: delete the matching items for good, the
/// `$R` payload and then its `$I` record. No match is not an error, as with
/// `rtrash rm`.
fn rm(prog: &str, args: &[String], items: &[BinEntry]) -> i32 {
    let mut dry = false;
    let mut matcher = None;
    let mut paths = Vec::new();
    for a in args {
        if matches!(a.as_str(), "-n" | "--dry-run") {
            dry = true;
            continue;
        }
        match take_matcher(prog, a, &mut matcher) {
            Ok(true) => {}
            Ok(false) if a.starts_with('-') && a != "-" => {
                eprintln!("{prog}: rm: unknown option '{a}'");
                return 2;
            }
            Ok(false) => {
                let p = crate::util::arg_path(a);
                paths.push(std::path::absolute(&p).unwrap_or(p));
            }
            Err(c) => return c,
        }
    }
    if matcher.is_none() && paths.is_empty() {
        eprintln!("{prog}: rm needs --glob, --regex, --name or a PATH");
        return 2;
    }
    let chosen: Vec<&BinEntry> = items
        .iter()
        .filter(|it| {
            matcher.as_ref().is_some_and(|m| m.keeps(&it.local)) || paths.contains(&it.local)
        })
        .collect();
    if chosen.is_empty() {
        eprintln!("{prog}: no Recycle Bin items match");
        return 0;
    }
    let mut status = 0;
    for it in chosen {
        if dry {
            println!("would remove '{}'", it.local.display());
            continue;
        }
        match remove_entry(it) {
            Ok(()) => println!("removed '{}'", it.local.display()),
            Err(e) => {
                eprintln!("{prog}: cannot remove '{}': {e}", it.local.display());
                status = 1;
            }
        }
    }
    status
}

/// Delete one item: the payload (a file or a whole tree), then its record,
/// so a failure leaves the record that still names the payload.
fn remove_entry(it: &BinEntry) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(&it.payload)?;
    if meta.is_dir() {
        std::fs::remove_dir_all(&it.payload)?;
    } else {
        std::fs::remove_file(&it.payload)?;
    }
    if let Some(index) = crate::win_recycle::index_file_for(&it.payload) {
        match std::fs::remove_file(&index) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
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
