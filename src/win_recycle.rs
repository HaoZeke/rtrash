//! Windows system Recycle Bin backend.
//!
//! This is **not** FreeDesktop. Put/list/restore/empty use the OS Recycle Bin
//! (shell APIs via the `trash` crate on Windows). Host-free pure helpers for
//! `$I` metadata parsing are always compiled so Linux CI can unit-test them.

use std::path::{Path, PathBuf};

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
pub const FILETIME_UNIX_EPOCH: u64 = 11_644_473_600_000_000_000;
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
        if bytes.len() < 0x1C {
            return None;
        }
        let path_bytes_len = u32::from_le_bytes(bytes[0x18..0x1C].try_into().ok()?) as usize;
        if path_bytes_len < 2 || bytes.len() < 0x1C + path_bytes_len {
            return None;
        }
        let path_bytes = &bytes[0x1C..0x1C + path_bytes_len];
        decode_utf16le_path(path_bytes)?
    } else {
        // Version 1: fixed 520 wchar (1040 byte) buffer at 0x18.
        if bytes.len() < 0x18 + 2 {
            return None;
        }
        decode_utf16le_path(&bytes[0x18..])?
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
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    // Strip trailing NULs.
    while units.last().copied() == Some(0) {
        units.pop();
    }
    if units.is_empty() {
        return None;
    }
    let s = String::from_utf16(&units).ok()?;
    Some(PathBuf::from(s))
}

/// Build a minimal version-2 `$I` buffer for tests (round-trip with [`parse_recycle_i`]).
pub fn encode_recycle_i_v2(original: &Path, size: u64, delete_filetime: u64) -> Vec<u8> {
    let path_utf16: Vec<u16> = original
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let path_bytes_len = (path_utf16.len() * 2) as u32;
    let mut out = Vec::with_capacity(0x1C + path_bytes_len as usize);
    out.extend_from_slice(&2u64.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&delete_filetime.to_le_bytes());
    out.extend_from_slice(&path_bytes_len.to_le_bytes());
    for u in path_utf16 {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out
}

// ---- Windows CLI surface (system Recycle Bin via `trash` crate) ----

#[cfg(windows)]
mod os {
    use super::*;
    use trash::os_limited;
    use trash::TrashItem;

    fn err_msg(e: impl std::fmt::Display) -> String {
        e.to_string()
    }

    pub fn put_run(prog: &str, args: &[String]) -> i32 {
        let mut force = false;
        let mut verbose = false;
        let mut files: Vec<PathBuf> = Vec::new();
        let mut plain = false;
        for arg in args {
            match arg.as_str() {
                "-f" | "--force" => force = true,
                "-v" | "--verbose" => verbose = true,
                "--plain" => plain = true,
                "-h" | "--help" => {
                    print!(
                        "\
Usage: {prog} [OPTION]... [FILE]...
Move FILE(s) to the Windows Recycle Bin (system trash; not FreeDesktop).

  -f, --force    ignore nonexistent files
  -v, --verbose  print each path
      --plain    accepted for CLI parity (no TUI put browser on Windows)
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
        if files.is_empty() {
            if force {
                return 0;
            }
            eprintln!("{prog}: missing operand");
            eprintln!("Try '{prog} --help' for more information.");
            return 2;
        }
        let mut status = 0;
        for f in &files {
            if !f.exists() {
                if force {
                    continue;
                }
                eprintln!(
                    "{prog}: cannot remove '{}': No such file or directory",
                    f.display()
                );
                status = status.max(1);
                continue;
            }
            match trash::delete(f) {
                Ok(()) => {
                    if verbose {
                        println!("removed '{}'", f.display());
                    }
                }
                Err(e) => {
                    eprintln!("{prog}: cannot trash '{}': {}", f.display(), err_msg(e));
                    status = status.max(1);
                }
            }
        }
        status
    }

    pub fn list_run(prog: &str, args: &[String]) -> i32 {
        for arg in args {
            match arg.as_str() {
                "--help" => {
                    print!(
                        "\
Usage: {prog} [OPTION]...
List items in the Windows Recycle Bin (system trash).

      --help     display this help and exit
      --version  output version information and exit

Note: --home-only / --trash-dir are FreeDesktop options and are ignored
on Windows (there is no FreeDesktop layout here).
"
                    );
                    return 0;
                }
                "--version" => {
                    println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                    return 0;
                }
                "--home-only" => {}
                a if a.starts_with("--trash-dir=") => {}
                a => {
                    eprintln!("{prog}: unrecognized option '{a}'");
                    return 2;
                }
            }
        }
        match os_limited::list() {
            Ok(items) => {
                let mut items = items;
                items.sort_by_key(|i| i.time_deleted);
                for it in items {
                    let date = platform::epoch_to_iso_utc(it.time_deleted);
                    println!(
                        "{}",
                        platform::format_deletion_list_line(Some(&date), &it.original_path())
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
        for arg in args {
            match arg.as_str() {
                "-n" | "--dry-run" => dry_run = true,
                "-v" | "--verbose" => verbose = true,
                "-f" | "--force" | "--plain" | "--home-only" => {}
                "--help" => {
                    print!(
                        "\
Usage: {prog} [OPTION]...
Permanently empty the Windows Recycle Bin (system trash).

  -n, --dry-run  list what would be removed
  -v, --verbose  print each removed original path
      --help     display this help and exit
      --version  output version information and exit

Age filters (DAYS) and FreeDesktop --trash-dir pins are not supported
on the Recycle Bin backend.
"
                    );
                    return 0;
                }
                "--version" => {
                    println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                    return 0;
                }
                a if a.starts_with("--trash-dir=") => {}
                a if !a.starts_with('-') => {
                    eprintln!("{prog}: DAYS age filter is not supported on Windows Recycle Bin");
                    return 2;
                }
                a => {
                    eprintln!("{prog}: unrecognized option '{a}'");
                    return 2;
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
        if dry_run {
            for it in &items {
                println!("would remove {}", it.original_path().display());
            }
            println!("{} item(s)", items.len());
            return 0;
        }
        if verbose {
            for it in &items {
                println!("{}", it.original_path().display());
            }
        }
        match os_limited::purge_all(items) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("{prog}: cannot empty Recycle Bin: {}", err_msg(e));
                1
            }
        }
    }

    pub fn restore_run(prog: &str, args: &[String]) -> i32 {
        let mut force = false;
        let mut plain = true; // no TUI on Windows recycle path
        let mut target: Option<PathBuf> = None;
        for arg in args {
            match arg.as_str() {
                "-f" | "--force" => force = true,
                "--plain" | "--cwd-only" | "--home-only" => plain = true,
                "--help" => {
                    print!(
                        "\
Usage: {prog} [OPTION]... [PATH]
Restore an item from the Windows Recycle Bin to its original location.

With PATH, restore the item whose original path equals PATH.
Without PATH, print a numbered list and read an index from stdin.

  -f, --force   overwrite when the destination exists (best-effort)
      --plain   numbered selection (default on Windows)
      --help    display this help and exit
      --version output version information and exit
"
                    );
                    let _ = plain;
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
        let matches: Vec<TrashItem> = match &target {
            Some(t) => {
                let abs = std::fs::canonicalize(t).unwrap_or_else(|_| t.clone());
                items
                    .into_iter()
                    .filter(|i| i.original_path() == abs || i.original_path() == *t)
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
        // Numbered pick.
        for (i, it) in matches.iter().enumerate() {
            let date = platform::epoch_to_iso_utc(it.time_deleted);
            println!(
                "  {i} {}",
                platform::format_deletion_list_line(Some(&date), &it.original_path())
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

    fn restore_one(prog: &str, items: Vec<TrashItem>, force: bool) -> i32 {
        for it in &items {
            let dest = it.original_path();
            if dest.exists() && !force {
                eprintln!(
                    "{prog}: refusing to overwrite '{}' (use -f)",
                    dest.display()
                );
                return 1;
            }
            if dest.exists() && force {
                let _ = std::fs::remove_file(&dest).or_else(|_| std::fs::remove_dir_all(&dest));
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
        for arg in args {
            match arg.as_str() {
                "--help" => {
                    print!(
                        "\
Usage: {prog} [OPTION]...
Summarize Windows Recycle Bin item count.

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
                "--home-only" => {}
                a if a.starts_with("--trash-dir=") => {}
                a => {
                    eprintln!("{prog}: unrecognized option '{a}'");
                    return 2;
                }
            }
        }
        match os_limited::list() {
            Ok(items) => {
                println!("Recycle Bin: {} items", items.len());
                println!("Total: {} items", items.len());
                0
            }
            Err(e) => {
                eprintln!("{prog}: cannot list Recycle Bin: {}", err_msg(e));
                1
            }
        }
    }

    pub fn rm_run(prog: &str, args: &[String]) -> i32 {
        let mut dry_run = false;
        let mut force = false;
        let mut verbose = false;
        let mut patterns: Vec<String> = Vec::new();
        for arg in args {
            match arg.as_str() {
                "-n" | "--dry-run" => dry_run = true,
                "-f" | "--force" => force = true,
                "-v" | "--verbose" => verbose = true,
                "--home-only" => {}
                "--help" => {
                    print!(
                        "\
Usage: {prog} [OPTION]... PATTERN...
Permanently delete Recycle Bin items matching PATTERN (glob).

  -n, --dry-run  list matches only
  -f, --force    allow mass patterns (e.g. '*')
  -v, --verbose  print each path
      --help     display this help and exit
"
                    );
                    return 0;
                }
                "--version" => {
                    println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                    return 0;
                }
                a if a.starts_with("--trash-dir=") => {}
                a if a.starts_with('-') => {
                    eprintln!("{prog}: unrecognized option '{a}'");
                    return 2;
                }
                a => patterns.push(a.to_string()),
            }
        }
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
                let orig = it.original_path();
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
        if dry_run {
            for it in &matched {
                println!("{}", it.original_path().display());
            }
            return 0;
        }
        if verbose {
            for it in &matched {
                println!("{}", it.original_path().display());
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
        for arg in args {
            match arg.as_str() {
                "--help" => {
                    print!(
                        "\
Usage: {prog} [OPTION]...
Windows note: multi-call symlinks and man/completions install are Unix-oriented.
On Windows, invoke `rtrash put|list|restore|empty|rm|status` directly.

      --help  display this help and exit
"
                    );
                    return 0;
                }
                "--version" => {
                    println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                    return 0;
                }
                _ => {}
            }
        }
        eprintln!(
            "{prog}: setup multi-call install is Unix-oriented; on Windows use subcommands directly"
        );
        eprintln!(
            "{prog}: trash backend: {}",
            platform::trash_backend_label()
        );
        0
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

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

    #[test]
    fn recycle_i_rejects_short() {
        assert!(parse_recycle_i(&[0u8; 8]).is_none());
    }

    #[test]
    fn filetime_epoch_zero() {
        assert_eq!(filetime_to_unix_epoch(FILETIME_UNIX_EPOCH), 0);
        assert_eq!(filetime_to_unix_epoch(0), 0);
    }
}
