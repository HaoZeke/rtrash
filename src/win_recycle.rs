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
        let mut json = false;
        let mut pinned = false;
        let mut no_more_opts = false;
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
                "-h" | "--help" => {
                    print!(
                        "\
Usage: {prog} [OPTION]... [FILE]...
Move FILE(s) to the Windows Recycle Bin (system trash; not FreeDesktop).

  -f, --force    ignore nonexistent files
  -v, --verbose  print each path
      --plain    accepted for CLI parity (no TUI put browser on Windows)
      --json     print a JSON summary on stdout ({{trashed, failed}});
                   replaces -v output
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
        let mut status = 0;
        let mut failed = 0usize;
        let mut moved: Vec<PathBuf> = Vec::new();
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
            match trash::delete(f) {
                Ok(()) => {
                    if verbose && !json {
                        println!("removed '{}'", crate::util::term_path(f));
                    }
                    moved.push(abs);
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
        status
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
                    .filter(|it| it.original_path() == *orig)
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
                println!(
                    "would remove {}",
                    crate::util::term_path(&it.original_path())
                );
            }
            println!("{} item(s)", items.len());
            return 0;
        }
        if verbose {
            for it in &items {
                println!("{}", crate::util::term_path(&it.original_path()));
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
        let mut plain = false;
        let mut newest_first = false;
        let mut target: Option<PathBuf> = None;
        for arg in args {
            match arg.as_str() {
                "-f" | "--force" => force = true,
                "--plain" | "--cwd-only" | "--home-only" => plain = true,
                "--newest-first" => newest_first = true,
                "--oldest-first" => newest_first = false,
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
            let original = item.original_path();
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
                std::fs::remove_file(dest)
                    .or_else(|_| std::fs::remove_dir_all(dest))
                    .map_err(|e| format!("cannot replace '{}': {e}", dest.display()))?;
            }
            os_limited::restore_all(vec![self.item.clone()])
                .map(|()| (dest.clone(), Vec::new()))
                .map_err(|e| format!("restore failed: {}", err_msg(e)))
        }
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
Summarize the Windows Recycle Bin: items, size, and the cap of each
drive (MaxCapacity in the registry), then the total.

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
        // One line per drive with a bin or a cap, like the per-trash lines
        // of the FreeDesktop status, then the total.
        let (mut items, mut bytes) = (0u64, 0u64);
        for root in bin_drives() {
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
        0
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
                println!("{}", crate::util::term_path(&it.original_path()));
            }
            return 0;
        }
        if verbose {
            for it in &matched {
                println!("{}", crate::util::term_path(&it.original_path()));
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
