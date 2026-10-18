//! Selective permanent delete of trash entries (trash-cli `trash-rm` role).

use std::path::PathBuf;

use crate::list;
use crate::trashdir::{self, TrashDir};

const HELP: &str = "\
Usage: {prog} [OPTION]... PATTERN...
Permanently remove trashed items whose original path or basename matches
a PATTERN (shell-style glob; quote globs from the shell; --match picks
another kind, as trashy's -m does). Matching entries
are deleted from the trash (files/ + .trashinfo), not restored. An absolute
PATTERN also matches with symlinks in its directory part resolved, the way
put records original paths.

Note: multi-call name `rm` means *put into trash* (safe). This command
(subcommand `rtrash rm` or multi-call `trash-rm`) permanently deletes
matching *trash* entries — not a synonym for put.

On a terminal, rm names the matches (the first 10, with sizes) and asks
once before it deletes them, as rm -I does. A script whose stdin or
stderr is not a terminal is not asked. The last of -i, -f and -y wins.

      --trash-dir=PATH  only consider this trash directory (repeatable)
      --home-only  only the home trash (skip volume trash)
      --older-than=AGE  only match items trashed more than AGE ago: DAYS,
                     or a duration such as 2h, 30m or 1d12h
      --newer-than=AGE  only match items trashed within the last AGE
      --larger-than=SIZE  only match items taking more than SIZE on disk
      --smaller-than=SIZE only match items taking less than SIZE on disk
                     (SIZE as in `empty --max-size`: 500M, 1.5G, 64k)
      --regex      PATTERNs are regular expressions searched in the
                     original path (the same as --match=regex)
  -m, --match=KIND  how to read PATTERNs (trashy's -m): glob (default),
                     regex, substring (text in the original path) or
                     exact (the whole original path)
      --batch=ID   only items from one put run: an ID from `undo --list`,
                     or `last` for the latest; with no PATTERN, all of it
  -n, --dry-run    list matches and reclaimable size; do not delete
  -i, --interactive  ask even when stdin is not a terminal (the reply is
                     read from stdin; no reply removes nothing)
  -y, --yes        do not ask
  -f, --force      do not ask, and allow mass patterns that match
                     everything (e.g. '*')
      --max-count=N  refuse, deleting nothing, when more than N items
                     would go (also RTRASH_MAX_PERMANENT)
  -v, --verbose    print each permanently removed original path
      --json       emit a JSON summary (and matched originals array)
      --help       display this help and exit
      --version    output version information and exit

Examples:
  {prog} '*.o'
  {prog} -n '*.o'
  {prog} foo
  {prog} /home/you/old-project
  {prog} --regex '/build-[0-9]+/'
  {prog} --batch=last
";

pub fn run(prog: &str, args: &[String]) -> i32 {
    let snap = crate::finder_total::snapshot();
    let code = run_inner(prog, args);
    crate::trash_total::log_touched("rm");
    crate::finder_total::log("rm", &snap);
    code
}

fn run_inner(prog: &str, args: &[String]) -> i32 {
    let mut verbose = false;
    let mut force = false;
    let mut dry_run = false;
    let mut home_only = false;
    let mut json = false;
    let mut kind = crate::filters::Match::Glob;
    let mut older_than: Option<i64> = None;
    let mut newer_than: Option<i64> = None;
    let mut trash_dirs: Vec<PathBuf> = Vec::new();
    let mut patterns: Vec<String> = Vec::new();
    let mut batch: Option<String> = None;
    let mut size = list::SizeFilter::default();

    let mut i = 0usize;
    while i < args.len() {
        match size.take(args, &mut i) {
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
        match arg.as_str() {
            "-v" | "--verbose" => verbose = true,
            "-f" | "--force" => {
                force = true;
                crate::rm_ask::set(crate::rm_ask::Mode::Never);
            }
            "-y" | "--yes" => crate::rm_ask::set(crate::rm_ask::Mode::Never),
            "-i" | "--interactive" => crate::rm_ask::set(crate::rm_ask::Mode::Always),
            "-n" | "--dry-run" => dry_run = true,
            "--home-only" => home_only = true,
            "--json" => json = true,
            "--regex" => kind = crate::filters::Match::Regex,
            "-m" | "--match" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    eprintln!("{prog}: {arg} requires a kind (glob, regex, substring, exact)");
                    return 2;
                };
                match crate::filters::Match::parse(v) {
                    Ok(k) => kind = k,
                    Err(e) => {
                        eprintln!("{prog}: {e}");
                        return 2;
                    }
                }
            }
            a if a.starts_with("--match=") => match crate::filters::Match::parse(&a[8..]) {
                Ok(k) => kind = k,
                Err(e) => {
                    eprintln!("{prog}: {e}");
                    return 2;
                }
            },
            "--help" => {
                print!("{}", HELP.replace("{prog}", prog));
                return 0;
            }
            "--version" => {
                println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                return 0;
            }
            a if a.starts_with("--batch=") => {
                let id = &a["--batch=".len()..];
                if id.is_empty() {
                    eprintln!("{prog}: --batch needs an ID (or last)");
                    return 2;
                }
                batch = Some(id.to_string());
            }
            "--batch" => {
                eprintln!("{prog}: --batch needs a value (--batch=ID or --batch=last)");
                return 2;
            }
            a if a.starts_with("--trash-dir=") => {
                trash_dirs.push(crate::util::arg_path(&a["--trash-dir=".len()..]));
            }
            a if a.starts_with("--max-count=") => match crate::permanent_guard::max_count_arg(a) {
                Some(Ok(n)) => crate::permanent_guard::set_max_count(n),
                _ => {
                    eprintln!("{prog}: --max-count takes a whole number of items, not '{a}'");
                    return 2;
                }
            },
            a if a.starts_with("--older-than=") => {
                match crate::filters::parse_age_secs(&a["--older-than=".len()..]) {
                    Ok(d) => older_than = Some(d),
                    Err(e) => {
                        eprintln!("{prog}: {e}");
                        return 2;
                    }
                }
            }
            a if a.starts_with("--newer-than=") => {
                match crate::filters::parse_age_secs(&a["--newer-than=".len()..]) {
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
                    eprintln!("{prog}: {name} requires an AGE");
                    return 2;
                };
                match crate::filters::parse_age_secs(val) {
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

    if let Err(e) = size.check() {
        eprintln!("{prog}: {e}");
        return 2;
    }
    // A put run is a scope of its own: with no PATTERN, take all of it.
    let scoped = batch.is_some() && patterns.is_empty();
    if scoped {
        kind = crate::filters::Match::Glob;
        patterns.push("*".into());
    }
    if patterns.is_empty() {
        eprintln!("{prog}: missing operand");
        eprintln!("Try '{prog} --help' for more information.");
        return 2;
    }
    if matches!(
        kind,
        crate::filters::Match::Glob | crate::filters::Match::Exact
    ) {
        // put records originals with their parent directory resolved, so
        // a path typed through a symlinked directory matches that way too.
        let resolved: Vec<String> = patterns.iter().filter_map(|p| resolve_pattern(p)).collect();
        patterns.extend(resolved);
    }
    let pats = match crate::filters::Patterns::new(patterns.clone(), kind) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{prog}: {e}");
            return 2;
        }
    };

    // Refuse accidental host-wide permanent purge of the whole trash.
    if !force && !scoped && pats.is_mass() {
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
    // macOS deletes from the Finder trashes unless a pin or the setting
    // says otherwise.
    if crate::finder::is_default(!trash_dirs.is_empty()) {
        let filter = crate::finder_shared::Filter {
            age: crate::filters::AgeFilter {
                older_than,
                newer_than,
            },
            size,
        };
        return crate::finder_shared::rm(
            prog,
            &pats,
            batch.as_deref(),
            dry_run,
            verbose,
            json,
            filter,
        );
    }
    let mut entries = list::filter_age_secs(list::collect(&dirs), older_than, newer_than);
    // Payloads with no .trashinfo match by their name or their path under
    // files/; having no date, they stay out of an age-filtered rm.
    if older_than.is_none() && newer_than.is_none() {
        entries.extend(list::orphans(&dirs));
    }
    if let Some(want) = &batch {
        let id = if want == "last" {
            crate::undo::batches(&entries).first().map(|b| b.id.clone())
        } else {
            Some(want.clone())
        };
        let Some(id) = id else {
            eprintln!("{prog}: no put run in the trash (items from other tools carry no batch id)");
            return 1;
        };
        entries.retain(|e| e.batch.as_deref() == Some(id.as_str()));
        if entries.is_empty() {
            eprintln!("{prog}: no trashed items from put run '{id}'");
            return 1;
        }
    }
    let mut status = 0i32;
    let mut removed = 0u64;
    let mut bytes = 0u64;
    let mut matched_paths: Vec<String> = Vec::new();
    // Sizes come from each trash's directorysizes cache where it is
    // valid, so a dry run over big trashed trees does not walk them all.
    let sizes: Vec<trashdir::DirSizes> = dirs.iter().map(trashdir::DirSizes::load).collect();
    let size_of = |e: &list::Entry| -> u64 {
        match dirs.iter().position(|d| d.root == e.dir.root) {
            Some(i) => sizes[i].entry_bytes(&e.name),
            None => {
                crate::fastdelete::disk_usage(&crate::util::join_name(&e.dir.files(), &e.name))
                    + crate::fastdelete::disk_usage(&crate::util::join_info(&e.dir.info(), &e.name))
            }
        }
    };

    let wanted = |entry: &list::Entry| -> bool {
        let path_s = crate::util::str_from_os_ref(entry.original.as_os_str());
        let base = entry
            .original
            .file_name()
            .map(crate::util::str_from_os_ref)
            .unwrap_or_default();
        pats.hit(path_s.as_ref(), base.as_str(), entry.name.as_str()) && size.keeps(size_of(entry))
    };

    // A cap counts the matches first, and RTRASH_ALLOW_ROOTS checks where
    // they came from, so a refused rm deletes nothing.
    let scoped_roots = crate::allow_roots::roots().is_some();
    if !dry_run && (crate::permanent_guard::cap().is_some() || scoped_roots) {
        let hits: Vec<&list::Entry> = entries.iter().filter(|e| wanted(e)).collect();
        let origins: Vec<PathBuf> = hits.iter().map(|e| e.original.clone()).collect();
        if let Some(code) = crate::allow_roots::refuse_outside(prog, &origins) {
            return code;
        }
        if let Some(code) = crate::permanent_guard::check_count(prog, hits.len() as u64) {
            return code;
        }
    }

    // On a terminal, or with -i, rm names the matches and asks once.
    if crate::rm_ask::would_ask(dry_run, json) {
        let named: Vec<(String, u64)> = entries
            .iter()
            .filter(|e| wanted(e))
            .map(|e| (crate::util::term_path(&e.original), size_of(e)))
            .collect();
        if !crate::rm_ask::confirm(prog, &named) {
            return 0;
        }
    }

    // The first Ctrl-C stops after the entry in hand and a second quits.
    if !dry_run {
        crate::interrupt::stop_then_quit();
    }
    let mut not_reached: Option<usize> = None;
    for (k, entry) in entries.iter().enumerate() {
        if crate::interrupt::requested() {
            not_reached = Some(entries[k..].iter().filter(|e| wanted(e)).count());
            break;
        }
        let path_s = crate::util::str_from_os_ref(entry.original.as_os_str());
        let base = entry
            .original
            .file_name()
            .map(crate::util::str_from_os_ref)
            .unwrap_or_default();
        let hit = pats.hit(path_s.as_ref(), base.as_str(), entry.name.as_str());
        if !hit {
            continue;
        }
        let payload = crate::util::join_name(&entry.dir.files(), &entry.name);
        let info_path = crate::util::join_info(&entry.dir.info(), &entry.name);
        let sz = size_of(entry);
        if !size.keeps(sz) {
            continue;
        }
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
        if let Err(e) = crate::permanent_guard::remove(&payload) {
            eprintln!(
                "{prog}: cannot remove '{}': {e}",
                crate::util::err_path(&payload)
            );
            status = 1;
            continue;
        }
        // The payload is gone: its record goes too before a Ctrl-C acts.
        let section = crate::interrupt::critical();
        // A total current before this item stays current after it: the
        // item's size and its one count come off. The total leaves out
        // orphans (payloads without a .trashinfo), so removing one leaves
        // it as it is, and info/ does not change either.
        let orphan = !info_path.exists();
        let carry = (!orphan)
            .then(|| crate::trash_total::cached(&entry.dir))
            .flatten()
            .map(|(b, n)| (b.saturating_sub(sz), n.saturating_sub(1)));
        crate::trash_total::touched(&entry.dir);
        if !orphan {
            crate::trash_total::forget(&entry.dir);
        }
        let info_gone = std::fs::remove_file(&info_path);
        if let (Ok(()), Some((b, n))) = (&info_gone, carry) {
            crate::trash_total::store(&entry.dir, b, n);
        }
        if let Err(e) = info_gone {
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
        drop(section);
        crate::interrupt::test_seam("after-item");
    }

    for s in sizes {
        s.write_back();
    }
    if let Some(left) = not_reached {
        eprintln!("{prog}: stopped by Ctrl-C: {removed} removed, {left} not reached");
        status = 130;
    }
    if json {
        let sum = crate::filters::RmSummary {
            dry_run,
            removed,
            bytes,
        };
        print!("{}", sum.json(&matched_paths));
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
