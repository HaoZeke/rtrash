//! Selective permanent delete of trash entries (trash-cli `trash-rm` role).

use std::path::PathBuf;

use crate::list;
use crate::trashdir::{self, TrashDir};

const HELP: &str = "\
Usage: {prog} [OPTION]... PATTERN...
Permanently remove trashed items whose original path is PATTERN.
A bare PATTERN is that path exactly: the path put records, resolved
from the current directory, with a trailing slash ignored. `rm ~/a`
does not remove `~/ab` or `~/a/b`. When nothing matches, those longer
paths are named and left in place. --match=glob (a shell glob of the
original path, its basename or the trash name), --regex and
--match=substring are the broader matches; --match=exact is the same
comparison as a bare PATTERN. Matching entries are deleted from the
trash (files/ + .trashinfo), not restored.

Note: multi-call name `rm` means *put into trash* (safe). This command
(subcommand `rtrash rm` or multi-call `trash-rm`) permanently deletes
matching *trash* entries — not a synonym for put.

On a terminal, rm names the matches (the first 10, with sizes) and asks
once before it deletes them, as rm -I does. A script whose stdin or
stderr is not a terminal is not asked. The last of -i, -f and -y wins.
Without -f, rm refuses an item whose payload holds dirty, unpushed or
unknown git work; -n reports the same refusal without deleting anything.

      --trash-dir=PATH  only consider this trash directory (repeatable)
      --home-only  only the home trash (skip volume trash)
      --older-than=AGE  only match items trashed more than AGE ago: DAYS,
                     or a duration such as 2h, 30m or 1d12h
      --newer-than=AGE  only match items trashed within the last AGE
      --larger-than=SIZE  only match items taking more than SIZE on disk
      --smaller-than=SIZE only match items taking less than SIZE on disk
                     (SIZE as in `empty --max-size`: 500M, 1.5G, 64k)
      --glob       PATHs are shell-style globs
      --prefix     PATHs match themselves and paths below them
      --substring  PATHs are text searched in the original path
      --regex      PATHs are regular expressions searched in the
                     original path (the same as --match=regex)
  -m, --match=KIND  how to read PATTERNs (trashy's -m): exact (the
                     default; the whole original path), glob, regex,
                     substring (text in the original path), prefix (the
                     path and paths below it) or id (the {id} from
                     `list --format`, the trash name, date and root)
      --id=NAME    select one item by its name under the trash's files/
      --all-matches allow more than one item with the same original path
      --batch=ID   only items from one put run: an ID from `undo --list`,
                     or `last` for the latest; with no PATTERN, all of it
  -n, --dry-run    list matches and reclaimable size; do not delete
  -i, --interactive  ask even when stdin is not a terminal (the reply is
                     read from stdin; no reply removes nothing)
  -y, --yes        do not ask
  -f, --force      do not ask, and allow broad patterns that cover every name
                     below a matched directory or take every entry in a
                     known scope, including its last entry (e.g. '*')
                   When the scope is unknown, regex and substring matching
                     and globs containing a path separator require --force;
                     use an exact path, --id, or a basename glob instead.
      --max-count=N  refuse, deleting nothing, when more than N items
                     would go (also RTRASH_MAX_PERMANENT)
  -v, --verbose    print each permanently removed original path
      --json       emit a JSON summary (and matched originals array)
      --help       display this help and exit
      --version    output version information and exit

Examples:
  {prog} /home/you/old-project
  {prog} -n foo
  {prog} --match=glob '*.o'
  {prog} -n --match=glob '*.o'
  {prog} -i --max-count=1 --match=id ID
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
    let mut kind = crate::filters::Match::Exact;
    let mut older_than: Option<i64> = None;
    let mut newer_than: Option<i64> = None;
    let mut trash_dirs: Vec<PathBuf> = Vec::new();
    let mut patterns: Vec<String> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    let mut all_matches = false;
    let mut batch: Option<String> = None;
    let mut size = list::SizeFilter::default();

    let mut no_more_opts = false;
    let mut i = 0usize;
    while i < args.len() {
        if !no_more_opts {
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
        }
        let arg = &args[i];
        if no_more_opts {
            if arg.is_empty() {
                eprintln!("{prog}: '' is not a path");
                eprintln!("Try '{prog} --help' for more information.");
                return 2;
            }
            patterns.push(arg.to_string());
            i += 1;
            continue;
        }
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
            "--glob" => kind = crate::filters::Match::Glob,
            "--prefix" => kind = crate::filters::Match::Prefix,
            "--substring" => kind = crate::filters::Match::Substring,
            "--regex" => kind = crate::filters::Match::Regex,
            "--all-matches" => all_matches = true,
            a if a.starts_with("--id=") => {
                let id = &a["--id=".len()..];
                if id.is_empty() {
                    eprintln!("{prog}: --id needs a trash item name");
                    return 2;
                }
                ids.push(id.to_string());
            }
            "-m" | "--match" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    eprintln!(
                        "{prog}: {arg} requires a kind \
                         (exact, glob, regex, substring, prefix or id)"
                    );
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
            "" => {
                eprintln!("{prog}: '' is not a path");
                eprintln!("Try '{prog} --help' for more information.");
                return 2;
            }
            "--" => no_more_opts = true,
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
    // A put run is a scope of its own: with no PATTERN and no --id, take
    // all of it. An id is a selection, so the run is not then taken as `*`.
    let scoped = crate::rm_match::batch_takes_the_whole_run(
        batch.is_some(),
        patterns.is_empty(),
        ids.is_empty(),
    );
    if scoped {
        kind = crate::filters::Match::Glob;
        patterns.push("*".into());
    }
    if patterns.is_empty() && ids.is_empty() {
        eprintln!("{prog}: missing operand");
        eprintln!("Try '{prog} --help' for more information.");
        return 2;
    }
    if kind == crate::filters::Match::Glob {
        // put records originals with their parent directory resolved, so
        // a path typed through a symlinked directory matches that way too.
        let resolved: Vec<String> = patterns.iter().filter_map(|p| resolve_pattern(p)).collect();
        patterns.extend(resolved);
    }
    let mut pats = match crate::filters::Patterns::new(patterns.clone(), kind) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{prog}: {e}");
            return 2;
        }
    };
    if kind == crate::filters::Match::Exact {
        // The path put would record, including a relative one and one
        // typed with a trailing slash. `operands` stays as typed, so a
        // hint is once per operand rather than once per spelling.
        pats.texts = crate::orig_path::expand(&pats.operands);
    }

    // Refuse accidental host-wide permanent purge of the whole trash.
    if crate::rm_match::refuse_pattern_selection(prog, &pats, &patterns, force, scoped, &[], &[]) {
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
        let keep = match crate::keep::Keep::load() {
            Ok(keep) => keep,
            Err(e) => {
                eprintln!("{prog}: cannot read keep file: {e}; refusing permanent deletion");
                return 1;
            }
        };
        return crate::finder_shared::rm(
            prog,
            &pats,
            crate::finder_shared::RmOpts {
                ids: &ids,
                all_matches,
                keep: &keep,
                batch: batch.as_deref(),
                dry_run,
                force,
                verbose,
                json,
                filter,
                refuse_covered: !force && !scoped,
            },
        );
    }
    let collected = list::collect(&dirs);
    let keep = match crate::keep::Keep::load() {
        Ok(keep) => keep,
        Err(e) => {
            eprintln!("{prog}: cannot read keep file: {e}; refusing permanent deletion");
            return 1;
        }
    };
    // Originals before the age filter, with their batch id, so a path that
    // the window leaves out is still visible to the hint.
    let mut pre: Vec<(PathBuf, Option<String>)> = collected
        .iter()
        .map(|e| (e.original.clone(), e.batch.clone()))
        .collect();
    let mut entries = collected;
    // Payloads with no .trashinfo match by their name or their path under
    // files/; having no date, they stay out of an age-filtered rm.
    let orphans = list::orphans(&dirs);
    for orphan in &orphans {
        pre.push((orphan.original.clone(), orphan.batch.clone()));
    }
    if older_than.is_none() && newer_than.is_none() {
        entries.extend(orphans);
    }
    let census: Vec<PathBuf> = entries.iter().map(|e| e.original.clone()).collect();
    let mut entries = list::filter_age_secs(entries, older_than, newer_than);
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
        pre.retain(|(_, batch_id)| batch_id.as_deref() == Some(id.as_str()));
        if entries.is_empty() {
            eprintln!("{prog}: no trashed items from put run '{id}'");
            return 1;
        }
    }
    let have: Vec<PathBuf> = pre.into_iter().map(|(path, _)| path).collect();
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
    let glob_pats = crate::filters::Patterns {
        texts: pats
            .texts
            .iter()
            .map(|text| crate::util::nfc(text))
            .collect(),
        ..pats.clone()
    };

    let matched = |entry: &list::Entry| -> bool {
        let path_s = crate::util::str_from_os_ref(entry.original.as_os_str());
        let base = entry
            .original
            .file_name()
            .map(crate::util::str_from_os_ref)
            .unwrap_or_default();
        let pattern_hit = if kind == crate::filters::Match::Exact {
            crate::orig_path::selects(&pats.texts, &entry.original)
        } else if kind == crate::filters::Match::Glob {
            glob_pats.selects(
                &crate::util::nfc(&path_s),
                &crate::util::nfc(&base),
                &crate::util::nfc(&entry.name),
                entry.date.as_deref().unwrap_or(""),
                &entry.dir.root,
            )
        } else {
            pats.selects(
                path_s.as_ref(),
                base.as_str(),
                entry.name.as_str(),
                entry.date.as_deref().unwrap_or(""),
                &entry.dir.root,
            )
        };
        crate::rm_match::chosen(
            pattern_hit,
            crate::rm_match::id_hit(&ids, &entry.name),
            !ids.is_empty(),
        ) && size.keeps(size_of(entry))
    };
    if kind == crate::filters::Match::Exact {
        let kept: Vec<PathBuf> = entries
            .iter()
            .filter(|entry| !size.is_set() || size.keeps(size_of(entry)))
            .map(|entry| entry.original.clone())
            .collect();
        let window =
            crate::orig_path::window(older_than.is_some() || newer_than.is_some(), size.is_set());
        crate::orig_path::hint(prog, &pats.operands, &have, &kept, window);
    }

    let hits: Vec<&list::Entry> = entries.iter().filter(|e| matched(e)).collect();
    if kind == crate::filters::Match::Exact
        && ids.is_empty()
        && crate::rm_match::refuse_ambiguous(
            prog,
            hits.iter().map(|e| e.original.as_path()),
            all_matches,
        )
    {
        return 2;
    }
    if hits.is_empty() {
        return crate::rm_match::no_matches(prog);
    }
    // After the match: a selection that holds every entry, or every entry
    // under a real scope root, is the same mass delete the probe check
    // refuses. A prefix or glob that leaves an entry stays.
    let originals: Vec<PathBuf> = hits.iter().map(|e| e.original.clone()).collect();
    if crate::rm_match::refuse_pattern_selection(
        prog, &pats, &patterns, force, scoped, &originals, &census,
    ) {
        return 2;
    }
    for entry in hits.iter().filter(|e| keep.protects(&e.original)) {
        eprintln!(
            "{prog}: kept '{}' (listed in the keep file)",
            crate::util::err_path(&entry.original)
        );
    }
    let selected: Vec<&list::Entry> = hits
        .into_iter()
        .filter(|e| !keep.protects(&e.original))
        .collect();
    if selected.is_empty() {
        if json {
            let sum = crate::filters::RmSummary {
                dry_run,
                removed: 0,
                bytes: 0,
            };
            print!("{}", sum.json(&[]));
        }
        return 0;
    }
    let deletable = |entry: &list::Entry| {
        selected
            .iter()
            .any(|s| s.name == entry.name && s.dir.root == entry.dir.root)
    };
    let seen: Vec<trashdir::Seen> = dirs.iter().map(trashdir::Seen::take).collect();

    // A cap counts the matches first, and RTRASH_ALLOW_ROOTS checks where
    // they came from, so a refused rm deletes nothing.
    let scoped_roots = crate::allow_roots::roots().is_some();
    if !dry_run && (crate::permanent_guard::cap().is_some() || scoped_roots) {
        let origins: Vec<PathBuf> = selected.iter().map(|e| e.original.clone()).collect();
        if let Some(code) = crate::allow_roots::refuse_outside(prog, &origins) {
            return code;
        }
        if let Some(code) = crate::permanent_guard::check_count(prog, selected.len() as u64) {
            return code;
        }
    }

    // On a terminal, or with -i, rm names the matches and asks once.
    if crate::rm_ask::would_ask(dry_run, json) {
        let named: Vec<(String, u64)> = entries
            .iter()
            .filter(|e| deletable(e))
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
            not_reached = Some(entries[k..].iter().filter(|e| deletable(e)).count());
            break;
        }
        if !deletable(entry) {
            continue;
        }
        let sz = size_of(entry);
        if dry_run {
            let payload_scanned = crate::util::join_name(&entry.dir.files(), &entry.name);
            if !force && !crate::util::is_gone(&payload_scanned) {
                if let Some(work) = crate::du_git::risky_tree(&payload_scanned) {
                    eprintln!(
                        "{prog}: refusing to remove '{}': it holds {} git work; \
                         restore it, or pass -f to remove it anyway",
                        crate::util::err_path(&entry.original),
                        work.label()
                    );
                    status = 1;
                    continue;
                }
            }
            bytes = bytes.saturating_add(sz);
            let line = if verbose {
                format!(
                    "would permanently remove {} ({})",
                    entry.original.display(),
                    crate::fastdelete::format_bytes(sz)
                )
            } else {
                entry.original.display().to_string()
            };
            crate::util::HumanOutput::new(json, !json || verbose).line(&line);
            matched_paths.push(entry.original.to_string_lossy().into_owned());
            removed += 1;
            continue;
        }
        // Under the trash lock, and only if it is still the item read
        // above: one that left the trash while rm asked, and whose name a
        // put gave to another item, stays. One already gone is skipped.
        let held = match entry.relock() {
            Ok(held) => held,
            Err(list::Relock::Gone) => continue,
            Err(why) => {
                eprintln!("{prog}: {}", crate::util::err_text(&why.reason(entry)));
                status = 1;
                continue;
            }
        };
        let same_payload = dirs
            .iter()
            .position(|d| d.root == entry.dir.root)
            .and_then(|i| seen.get(i))
            .is_some_and(|s| s.saw_item(&crate::util::os_from_str(&entry.name)));
        if !same_payload {
            eprintln!(
                "{prog}: '{}' changed after it was listed; left as it is",
                crate::util::err_path(&entry.original)
            );
            status = 1;
            continue;
        }
        let (payload, info_path) = (held.payload(), held.record());
        if !force && !held.gone() {
            if let Some(work) = crate::du_git::risky_tree(&payload) {
                eprintln!(
                    "{prog}: refusing to remove '{}': it holds {} git work; \
                     restore it, or pass -f to remove it anyway",
                    crate::util::err_path(&entry.original),
                    work.label()
                );
                status = 1;
                continue;
            }
        }
        // The scan above can take long. The payload must still be the file
        // the lock was taken on, or whatever took its name stays.
        crate::util::replace_payload_seam(&payload);
        if held.swapped() {
            eprintln!(
                "{prog}: {}",
                crate::util::err_text(&list::Relock::Changed.reason(entry))
            );
            status = 1;
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
        crate::util::HumanOutput::new(json, verbose)
            .line(&format!("removed '{}'", entry.original.display()));
        matched_paths.push(entry.original.to_string_lossy().into_owned());
        removed += 1;
        drop(section);
        drop(held);
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
