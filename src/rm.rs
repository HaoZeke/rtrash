//! Selective permanent delete of trash entries (trash-cli `trash-rm` role).

use std::path::PathBuf;

use crate::list;
use crate::trashdir::{self, TrashDir};
use crate::util::fnmatch;

const HELP: &str = "\
Usage: {prog} [OPTION]... PATTERN...
Permanently remove trashed items whose original path or basename matches
a PATTERN (shell-style glob; quote globs from the shell). Matching entries
are deleted from the trash (files/ + .trashinfo), not restored. An absolute
PATTERN also matches with symlinks in its directory part resolved, the way
put records original paths.

Note: multi-call name `rm` means *put into trash* (safe). This command
(subcommand `rtrash rm` or multi-call `trash-rm`) permanently deletes
matching *trash* entries — not a synonym for put.

      --trash-dir=PATH  only consider this trash directory (repeatable)
      --home-only  only the home trash (skip volume trash)
      --older-than=DAYS only match items trashed more than DAYS days ago
      --newer-than=DAYS only match items trashed within the last DAYS days
  -n, --dry-run    list matches and reclaimable size; do not delete
  -f, --force      allow mass patterns that match everything (e.g. '*')
  -v, --verbose    print each permanently removed original path
      --json       emit a JSON summary (and matched originals array)
      --help       display this help and exit
      --version    output version information and exit

Examples:
  {prog} '*.o'
  {prog} -n '*.o'
  {prog} foo
  {prog} /home/you/old-project
";

fn parse_nonneg_days(s: &str) -> Result<i64, String> {
    s.parse::<i64>()
        .map_err(|_| format!("invalid DAYS '{s}'"))
        .and_then(|d| {
            if d < 0 {
                Err(format!("DAYS must be >= 0 (got {d})"))
            } else {
                Ok(d)
            }
        })
}

pub fn run(prog: &str, args: &[String]) -> i32 {
    let mut verbose = false;
    let mut force = false;
    let mut dry_run = false;
    let mut home_only = false;
    let mut json = false;
    let mut older_than: Option<i64> = None;
    let mut newer_than: Option<i64> = None;
    let mut trash_dirs: Vec<PathBuf> = Vec::new();
    let mut patterns: Vec<String> = Vec::new();

    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-v" | "--verbose" => verbose = true,
            "-f" | "--force" => force = true,
            "-n" | "--dry-run" => dry_run = true,
            "--home-only" => home_only = true,
            "--json" => json = true,
            "--help" => {
                print!("{}", HELP.replace("{prog}", prog));
                return 0;
            }
            "--version" => {
                println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                return 0;
            }
            a if a.starts_with("--trash-dir=") => {
                trash_dirs.push(crate::util::arg_path(&a["--trash-dir=".len()..]));
            }
            a if a.starts_with("--older-than=") => {
                match parse_nonneg_days(&a["--older-than=".len()..]) {
                    Ok(d) => older_than = Some(d),
                    Err(e) => {
                        eprintln!("{prog}: {e}");
                        return 2;
                    }
                }
            }
            a if a.starts_with("--newer-than=") => {
                match parse_nonneg_days(&a["--newer-than=".len()..]) {
                    Ok(d) => newer_than = Some(d),
                    Err(e) => {
                        eprintln!("{prog}: {e}");
                        return 2;
                    }
                }
            }
            "--older-than" | "--newer-than" => {
                let name = arg.as_str();
                i += 1;
                let Some(val) = args.get(i) else {
                    eprintln!("{prog}: {name} requires DAYS");
                    return 2;
                };
                match parse_nonneg_days(val) {
                    Ok(d) if name == "--older-than" => older_than = Some(d),
                    Ok(d) => newer_than = Some(d),
                    Err(e) => {
                        eprintln!("{prog}: {e}");
                        return 2;
                    }
                }
            }
            a if a.starts_with('-') && a.len() > 1 => {
                eprintln!("{prog}: unrecognized option '{a}'");
                eprintln!("Try '{prog} --help' for more information.");
                return 2;
            }
            a => patterns.push(a.to_string()),
        }
        i += 1;
    }

    if patterns.is_empty() {
        eprintln!("{prog}: missing operand");
        eprintln!("Try '{prog} --help' for more information.");
        return 2;
    }

    // Refuse accidental host-wide permanent purge of the whole trash.
    if !force
        && patterns
            .iter()
            .any(|p| p == "*" || p == "*.*" || p == "**" || p == "**/*")
    {
        eprintln!(
            "{prog}: refusing mass pattern {:?} without --force (would permanently delete matching trash entries)",
            patterns
        );
        eprintln!("Try '{prog} --force …' only if you intend a full selective purge.");
        return 2;
    }

    let dirs: Vec<TrashDir> = trashdir::resolve_dirs_opts(&trash_dirs, home_only);
    if dirs.is_empty() && !trash_dirs.is_empty() {
        eprintln!("{prog}: no valid --trash-dir pins");
        return 2;
    }
    // put records originals with their parent directory resolved, so a
    // path typed through a symlinked directory matches in that form too.
    let resolved: Vec<String> = patterns.iter().filter_map(|p| resolve_pattern(p)).collect();
    patterns.extend(resolved);
    let mut entries = list::filter_age(list::collect(&dirs), older_than, newer_than);
    // Payloads with no .trashinfo match by their name or their path under
    // files/; having no date, they stay out of an age-filtered rm.
    if older_than.is_none() && newer_than.is_none() {
        entries.extend(list::orphans(&dirs));
    }
    let mut status = 0i32;
    let mut removed = 0u64;
    let mut bytes = 0u64;
    let mut matched_paths: Vec<String> = Vec::new();

    for entry in &entries {
        let path_s = crate::util::str_from_os_ref(entry.original.as_os_str());
        let base = entry
            .original
            .file_name()
            .map(crate::util::str_from_os_ref)
            .unwrap_or_default();
        let hit = patterns.iter().any(|pat| {
            fnmatch(pat, path_s.as_ref())
                || fnmatch(pat, base.as_str())
                || fnmatch(pat, entry.name.as_str())
        });
        if !hit {
            continue;
        }
        let payload = crate::util::join_name(&entry.dir.files(), &entry.name);
        let info_path = crate::util::join_info(&entry.dir.info(), &entry.name);
        let sz =
            crate::fastdelete::disk_usage(&payload) + crate::fastdelete::disk_usage(&info_path);
        if dry_run {
            bytes = bytes.saturating_add(sz);
            if !json {
                if verbose {
                    println!(
                        "would permanently remove {} ({})",
                        crate::util::term_path(&entry.original),
                        crate::fastdelete::format_bytes(sz)
                    );
                } else {
                    println!("{}", crate::util::term_path(&entry.original));
                }
            }
            matched_paths.push(entry.original.to_string_lossy().into_owned());
            removed += 1;
            continue;
        }
        if let Err(e) = trashdir::remove_any_path(&payload) {
            eprintln!(
                "{prog}: cannot remove '{}': {e}",
                crate::util::err_path(&payload)
            );
            status = 1;
            continue;
        }
        if let Err(e) = std::fs::remove_file(&info_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "{prog}: cannot remove '{}': {e}",
                    crate::util::err_path(&info_path)
                );
                status = 1;
                continue;
            }
        }
        trashdir::directorysizes_remove(&entry.dir, &entry.name);
        bytes = bytes.saturating_add(sz);
        if !json && verbose {
            println!("removed '{}'", crate::util::term_path(&entry.original));
        }
        matched_paths.push(entry.original.to_string_lossy().into_owned());
        removed += 1;
    }

    if json {
        let mut out = String::from("{\n");
        out.push_str(&format!(
            "  \"dry_run\": {},\n  \"removed\": {},\n  \"bytes\": {},\n  \"matches\": [\n",
            if dry_run { "true" } else { "false" },
            removed,
            bytes
        ));
        for (i, path) in matched_paths.iter().enumerate() {
            if i > 0 {
                out.push_str(",\n");
            }
            out.push_str(&format!("    \"{}\"", list::json_escape(path)));
        }
        if !matched_paths.is_empty() {
            out.push('\n');
        }
        out.push_str("  ]\n}\n");
        print!("{out}");
    } else if dry_run {
        let noun = if removed == 1 { "item" } else { "items" };
        eprintln!(
            "Would permanently remove {removed} {noun} ({}, approximately reclaimable)",
            crate::fastdelete::format_bytes(bytes)
        );
    } else if verbose || removed > 0 {
        let noun = if removed == 1 { "item" } else { "items" };
        eprintln!("Permanently removed {removed} {noun}");
    }
    status
}

/// An absolute PATTERN with symlinks resolved in its literal directory
/// part: the components before the first one with a glob character, or
/// the parent when there is none. The rest stays as typed, so a glob or
/// the final name is never followed. `None` when nothing changes.
fn resolve_pattern(pat: &str) -> Option<String> {
    let path = crate::util::arg_path(pat);
    if !path.is_absolute() {
        return None;
    }
    let is_glob = |c: &std::ffi::OsStr| c.to_string_lossy().contains(['*', '?', '[', '\\']);
    let comps: Vec<_> = path.components().collect();
    let split = comps
        .iter()
        .position(|c| is_glob(c.as_os_str()))
        .unwrap_or(comps.len().saturating_sub(1))
        .max(1);
    let prefix: PathBuf = comps[..split].iter().collect();
    let rest: PathBuf = comps[split..].iter().collect();
    let out = trashdir::resolve_existing(&prefix).join(rest);
    let out = crate::util::str_from_os_ref(out.as_os_str());
    // A trailing slash typed by the user is part of the pattern.
    let out = if pat.ends_with('/') && !out.ends_with('/') {
        out + "/"
    } else {
        out
    };
    (out != pat).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::resolve_pattern;
    use crate::util::fnmatch;

    #[test]
    fn absolute_patterns_resolve_their_literal_directories() {
        let base = std::env::temp_dir().join(format!("rtrash-rm-pat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("real/sub")).unwrap();
        // Resolved up front: macOS reaches temp_dir through /var.
        let base = std::fs::canonicalize(&base).unwrap();
        let real = std::fs::canonicalize(base.join("real")).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let l = link.display();
        let r = real.display();
        assert_eq!(
            resolve_pattern(&format!("{l}/a.txt")),
            Some(format!("{r}/a.txt"))
        );
        assert_eq!(
            resolve_pattern(&format!("{l}/sub/*.o")),
            Some(format!("{r}/sub/*.o"))
        );
        // The glob part and everything after it stay as typed.
        assert_eq!(
            resolve_pattern(&format!("{l}/s*/x")),
            Some(format!("{r}/s*/x"))
        );
        // A link as the final name is not followed: it names the trashed item.
        assert_eq!(resolve_pattern(&format!("{r}/sub")), None);
        assert_eq!(resolve_pattern(&format!("{}/link", base.display())), None);
        assert_eq!(resolve_pattern("*.o"), None);
        assert_eq!(resolve_pattern("foo"), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn pattern_matches_basename_glob() {
        assert!(fnmatch("*.o", "foo.o"));
        assert!(fnmatch("*.o", "/tmp/bar.o"));
        assert!(!fnmatch("*.o", "foo.c"));
    }

    #[test]
    fn pattern_literal_name() {
        assert!(fnmatch("foo", "foo"));
        // Full path does not match a bare name; run() also tries the basename.
        assert!(!fnmatch("foo", "/home/u/foo"));
        assert!(fnmatch("/home/u/foo", "/home/u/foo"));
        assert!(!fnmatch("/home/u/foo", "foo"));
    }
}
