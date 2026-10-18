use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;

use crate::info;
use crate::list;
use crate::trashdir::{self, TrashDir};
#[cfg(feature = "tui")]
use crate::util::stdin_is_tty;

const HELP: &str = "\
Usage: {prog} [OPTION]... [DAYS]
Purge trashed items. With DAYS (or an age such as 2h), only items trashed
more than that long ago.
An item that cannot be removed stays; each one is listed at the end, with
the reason, and the exit status is 1.
On a TTY without DAYS, opens the interactive empty browser (multi-select
permanent delete). Use --plain to skip the TUI.

  -n, --dry-run    report what would be removed without removing anything;
                     also prints approximate reclaimable disk space (fast
                     in-process walk, like du of the victims)
  -v, --verbose    print each removed item by its original path
  -i, --interactive  list the trash directories and ask before emptying
                     them (as trash-cli); skips the TUI
  -f, --force      do not ask (cancels an earlier -i)
      --max-count=N  refuse, deleting nothing, when more than N items
                     would go (also RTRASH_MAX_PERMANENT)
      --plain      skip TUI; non-interactive empty (also used when stdin is not a TTY)
      --all        allow an empty with no DAYS, --max-size, --until-free or
                     --trash-dir to remove items other puts left; without
                     it such an empty asks on a terminal and refuses
                     elsewhere unless every item came from the latest put
      --older-than=AGE  same as positional DAYS; AGE may also be a duration
                     such as 2h, 30m or 1d12h
      --max-size=SIZE  remove items until the trash (all roots in scope
                     together) fits in SIZE, such as 500M or 10G; with
                     DAYS, the age cut runs first
      --prune=oldest|largest  which items --max-size removes first
                     (default oldest)
      --until-free=SPEC  on each file system under SPEC free (a percent
                     such as 10%, a size such as 20G, and ,inodes=PCT),
                     remove the oldest items of every trash there (with
                     --home-only or --trash-dir, only file systems those
                     hold) until
                     it is back at SPEC; exits 1 when one stays short;
                     --json lists each short file system
      --dangling   remove only the .trashinfo records whose payload is gone
                     (they list as items that cannot be restored); takes
                     no DAYS or --max-size; the limits of status --fix
                     apply (RTRASH_NO_PERMANENT, RTRASH_MAX_PERMANENT
                     or --max-count, RTRASH_ALLOW_ROOTS)
      --json       emit a JSON summary on stdout ({dry_run, removed, bytes, days,
                     older_than_secs, max_size})
      --trash-dir=PATH  empty only this trash directory (repeatable);
                     default is the home trash plus every mounted volume
      --home-only  only the home trash (skip volume trash discovery)
      --all-users  every user's trash this account can write (root: all)
      --help       display this help and exit
      --version    output version information and exit
";

struct Opts {
    /// The age cut in seconds (`--older-than`, positional DAYS).
    days: Option<i64>,
    dry_run: bool,
    verbose: bool,
    home_only: bool,
    trash_dirs: Vec<PathBuf>,
    plain: bool,
    json: bool,
    max_size: Option<u64>,
    until_free: Option<(String, crate::reap::Floor)>,
    prune: crate::retention::Policy,
    dangling: bool,
    /// `-i`: ask once before emptying, as trash-cli does.
    interactive: bool,
    /// `--all`: an unscoped empty may take items other puts left.
    all: bool,
    all_users: bool,
}

/// The `.trashinfo` records in `dir` whose payload in `files/` is gone,
/// sorted. Only regular files are records: a folder or link by such a
/// name is left alone, as `status --check` reports it stray.
fn dangling_records(dir: &TrashDir) -> Vec<std::ffi::OsString> {
    let files = dir.files();
    let mut out: Vec<std::ffi::OsString> = fs::read_dir(dir.info())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            // Raw bytes, so a name that is not UTF-8 still finds its
            // payload and is never taken for dangling.
            let fname = e.file_name();
            if !e.file_type().is_ok_and(|t| t.is_file()) {
                return None;
            }
            let stem = fname.as_bytes().strip_suffix(b".trashinfo")?;
            if stem.is_empty() || stem.contains(&b'/') {
                return None;
            }
            let payload = files.join(std::ffi::OsStr::from_bytes(stem));
            // A payload that cannot be reached is not missing.
            crate::util::is_gone(&payload).then_some(fname)
        })
        .collect();
    out.sort();
    out
}

/// `empty --dangling`: remove the records that name no payload, under
/// each trash's lock, and nothing else. The limits are those of
/// `status --check --fix`, checked before the first removal so a refused
/// run removes nothing (exit 2): with `RTRASH_ALLOW_ROOTS` set each
/// record must name a path under a root (one whose origin cannot be read
/// refuses the run), then `RTRASH_MAX_PERMANENT` or `--max-count` caps
/// the count. Each removal goes through
/// [`crate::permanent_guard::remove_record`], so `RTRASH_NO_PERMANENT`
/// refuses it there too. A dry run checks none of them, as `-n` there.
fn empty_dangling(prog: &str, dirs: &[TrashDir], opts: &Opts) -> i32 {
    let found: Vec<Vec<std::ffi::OsString>> = dirs.iter().map(dangling_records).collect();
    let count = found.iter().map(|f| f.len() as u64).sum::<u64>();
    #[cfg(test)]
    {
        if let Some(f) = tests::AFTER_COUNT.take() {
            f();
        }
    }
    if !opts.dry_run && count > 0 {
        if crate::allow_roots::roots().is_some() {
            let scoped: Vec<(PathBuf, Option<PathBuf>)> = dirs
                .iter()
                .zip(&found)
                .flat_map(|(dir, names)| {
                    names.iter().map(move |fname| {
                        let record = dir.info().join(fname);
                        let origin = crate::trash_check::record_origin(dir, &record).ok();
                        (record, origin.map(|(o, _)| o))
                    })
                })
                .collect();
            if let Some(code) = crate::allow_roots::refuse_outside_or_unplaced(prog, &scoped) {
                return code;
            }
        }
        if let Some(code) = crate::permanent_guard::check_count(prog, count) {
            return code;
        }
    }
    let mut removed = 0u64;
    let mut status = 0;
    for (dir, checked) in dirs.iter().zip(&found) {
        let _lock = match trashdir::TrashLock::acquire(&dir.root) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("{prog}: cannot lock trash '{}': {e}", dir.root.display());
                status = 1;
                continue;
            }
        };
        // A total current before keeps current: each record's size and
        // its one count come off.
        let mut total = (!opts.dry_run)
            .then(|| crate::trash_total::cached(dir))
            .flatten();
        // Read again under the lock: a record whose payload came back
        // since stays, and only records counted above can go.
        let now = dangling_records(dir);
        for fname in now.iter().filter(|f| checked.contains(*f)) {
            let path = dir.info().join(fname);
            if opts.dry_run {
                if !opts.json {
                    println!("would remove {}", crate::util::term_path(&path));
                }
                removed += 1;
                continue;
            }
            let sz = fs::symlink_metadata(&path).map_or(0, |m| {
                use std::os::unix::fs::MetadataExt;
                m.blocks().saturating_mul(512)
            });
            match crate::permanent_guard::remove_record(&path) {
                Ok(()) => {
                    if opts.verbose && !opts.json {
                        println!("removed {}", crate::util::term_path(&path));
                    }
                    crate::trash_total::forget(dir);
                    crate::trash_total::touched(dir);
                    if let Some((b, n)) = total {
                        let next = (b.saturating_sub(sz), n.saturating_sub(1));
                        crate::trash_total::store(dir, next.0, next.1);
                        total = Some(next);
                    }
                    removed += 1;
                }
                Err(e) => {
                    eprintln!("{prog}: cannot remove '{}': {e}", path.display());
                    status = 1;
                }
            }
        }
    }
    if opts.json {
        println!(
            "{{\"dry_run\":{},\"removed\":{removed},\"dangling\":true}}",
            opts.dry_run
        );
    } else {
        let verb = if opts.dry_run {
            "Would remove"
        } else {
            "Removed"
        };
        eprintln!(
            "{verb} {}",
            crate::util::plural(
                removed,
                "record without a payload",
                "records without a payload"
            )
        );
    }
    status
}

/// trash-cli's `-i` prompt: name the trash directories, read a y or n.
/// Whether this empty would take everything: no age, size or free-space
/// cut, no `--trash-dir` pin, no `--all`, and not a dry run.
fn unscoped(opts: &Opts) -> bool {
    opts.days.is_none()
        && opts.max_size.is_none()
        && opts.trash_dirs.is_empty()
        && !opts.all
        && !opts.dry_run
}

/// Count every trash in `dirs`, for an empty to act on later.
fn count(dirs: &[TrashDir]) -> Vec<trashdir::Seen> {
    dirs.iter().map(trashdir::Seen::take).collect()
}

fn confirm(seen: &[trashdir::Seen]) -> bool {
    eprintln!("Would empty the following trash directories:");
    for s in seen {
        let (size, n) = holds(s);
        eprintln!("    - {} ({size}, {n})", s.dir().root.display());
    }
    crate::answer::ask("Proceed? (y/N) ")
}

/// What a trash's payload directory takes on disk and how many items the
/// count found at its top, as `12.0 KiB` and `3 items`, for the `-i`
/// prompt.
fn holds(seen: &trashdir::Seen) -> (String, String) {
    let size = crate::util::format_bytes(crate::fastdelete::disk_usage(&seen.dir().files()));
    (size, crate::util::plural(seen.items(), "item", "items"))
}

fn usage_err(prog: &str, msg: &str) -> i32 {
    eprintln!("{prog}: {msg}");
    eprintln!("Try '{prog} --help' for more information.");
    2
}

pub fn run(prog: &str, args: &[String]) -> i32 {
    let snap = crate::finder_total::snapshot();
    let code = run_inner(prog, args);
    crate::trash_total::log_touched("empty");
    crate::finder_total::log("empty", &snap);
    code
}

fn run_inner(prog: &str, args: &[String]) -> i32 {
    let mut opts = Opts {
        days: None,
        dry_run: false,
        verbose: false,
        home_only: false,
        trash_dirs: Vec::new(),
        plain: false,
        json: false,
        max_size: None,
        until_free: None,
        prune: crate::retention::Policy::Oldest,
        dangling: false,
        interactive: false,
        all_users: false,
        all: false,
    };
    for arg in args {
        match arg.as_str() {
            "-n" | "--dry-run" => opts.dry_run = true,
            "-v" | "--verbose" => opts.verbose = true,
            "-f" | "--force" => opts.interactive = false,
            "-i" | "--interactive" => opts.interactive = true,
            "--all-users" => opts.all_users = true,
            "--plain" => opts.plain = true,
            "--all" => opts.all = true,
            "--json" => opts.json = true,
            "--dangling" => opts.dangling = true,
            "--home-only" => opts.home_only = true,
            "--help" => {
                print!("{}", HELP.replace("{prog}", prog));
                return 0;
            }
            "--version" => {
                println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                return 0;
            }
            a if a.starts_with("--trash-dir=") => {
                opts.trash_dirs
                    .push(crate::util::arg_path(&a["--trash-dir=".len()..]));
            }
            a if a.starts_with("--max-count=") => match crate::permanent_guard::max_count_arg(a) {
                Some(Ok(n)) => crate::permanent_guard::set_max_count(n),
                _ => {
                    return usage_err(
                        prog,
                        &format!("--max-count takes a whole number of items, not '{a}'"),
                    )
                }
            },
            a if a.starts_with("--max-size=") => {
                match crate::retention::parse_size(&a["--max-size=".len()..]) {
                    Some(n) => opts.max_size = Some(n),
                    None => return usage_err(prog, &format!("invalid SIZE in '{a}'")),
                }
            }
            a if a.starts_with("--prune=") => {
                match crate::retention::Policy::parse(&a["--prune=".len()..]) {
                    Some(p) => opts.prune = p,
                    None => return usage_err(prog, "--prune takes oldest or largest"),
                }
            }
            a if a.starts_with("--until-free=") => {
                let v = &a["--until-free=".len()..];
                match crate::reap::Floor::parse(v) {
                    Some(f) => opts.until_free = Some((v.to_string(), f)),
                    None => {
                        return usage_err(
                            prog,
                            &format!("invalid SPEC in '{a}' (a percent such as 10%, a size such as 20G, and ,inodes=PCT)"),
                        )
                    }
                }
            }
            "--max-size" | "--prune" | "--until-free" => {
                return usage_err(prog, &format!("{arg} needs a value ({arg}=VALUE)"));
            }
            a if a.starts_with("--older-than=") => {
                match crate::filters::parse_age_secs(&a["--older-than=".len()..]) {
                    Ok(secs) => opts.days = Some(secs),
                    Err(e) => return usage_err(prog, &e),
                }
            }
            "--older-than" => {
                return usage_err(prog, "--older-than requires an AGE (use --older-than=AGE)");
            }
            a if !a.starts_with('-') => match crate::filters::parse_age_secs(a) {
                Ok(secs) => opts.days = Some(secs),
                Err(_) => return usage_err(prog, &format!("invalid DAYS argument '{a}'")),
            },
            a => return usage_err(prog, &format!("unrecognized option '{a}'")),
        }
    }

    if let Some((spec, floor)) = &opts.until_free {
        if opts.days.is_some() || opts.max_size.is_some() || opts.dangling || opts.all_users {
            return usage_err(
                prog,
                "--until-free takes no DAYS, --max-size, --dangling or --all-users",
            );
        }
        return until_free(prog, spec, *floor, &opts);
    }

    // macOS empties the Finder Trash unless a pin or the setting says
    // otherwise.
    if crate::finder::is_default(!opts.trash_dirs.is_empty() || opts.all_users) {
        if opts.dangling {
            return usage_err(
                prog,
                "--dangling does not apply to the Finder Trash; pin --trash-dir or set RTRASH_MACOS_TRASH=freedesktop",
            );
        }
        if !opts.interactive && unscoped(&opts) {
            let (total, foreign) = crate::finder_shared::outside_latest_put();
            if !crate::budget::guard_ok(prog, total, foreign) {
                return 2;
            }
        }
        if opts.interactive && !opts.dry_run {
            let (mut bytes, mut n) = (0u64, 0u64);
            for t in crate::finder::trashes() {
                bytes += crate::fastdelete::disk_usage(&t);
                n += fs::read_dir(&t).map_or(0, |d| d.flatten().count()) as u64;
            }
            let size = crate::util::format_bytes(bytes);
            let items = crate::util::plural(n, "item", "items");
            let ask = format!("Would empty the Finder Trash ({size}, {items}). Proceed? (y/N) ");
            if !crate::answer::ask(&ask) {
                return 0;
            }
        }
        let cutoff = opts.days.map(|secs| info::now_epoch() - secs);
        return crate::finder::default_empty(
            prog,
            &crate::finder::EmptyOpts {
                cutoff,
                max_size: opts.max_size,
                prune: opts.prune,
                dry_run: opts.dry_run,
                verbose: opts.verbose,
                json: opts.json,
            },
        );
    }
    if opts.all_users && (opts.home_only || !opts.trash_dirs.is_empty()) {
        return usage_err(prog, "--all-users takes no --home-only or --trash-dir");
    }
    let dirs: Vec<TrashDir> = if opts.all_users {
        trashdir::all_users()
    } else {
        trashdir::resolve_dirs_opts(&opts.trash_dirs, opts.home_only)
    };
    if dirs.is_empty() && !opts.trash_dirs.is_empty() {
        eprintln!(
            "{prog}: no valid --trash-dir pins (need non-symlink files/ and info/ directories)"
        );
        return 2;
    }

    if opts.dangling {
        if opts.days.is_some() || opts.max_size.is_some() {
            return usage_err(prog, "--dangling takes no DAYS or --max-size");
        }
        return empty_dangling(prog, &dirs, &opts);
    }

    // What the run counts is all it may remove: an item put after the
    // count, or after the yes to it, stays. A question counts first.
    let asked = (opts.interactive && !opts.dry_run).then(|| count(&dirs));
    if let Some(seen) = &asked {
        if !confirm(seen) {
            return 0;
        }
        opts.plain = true;
    }

    // TTY empty browser when no age filter (full interactive review).
    // --json implies non-interactive (scripts).
    #[cfg(feature = "tui")]
    if !opts.plain && !opts.json && opts.days.is_none() && opts.max_size.is_none() && stdin_is_tty()
    {
        let mut entries = list::collect(&dirs);
        // Orphans too, so the browser can clear everything a plain empty would.
        entries.extend(list::orphans(&dirs));
        let refs: Vec<&list::Entry> = entries.iter().collect();
        return crate::empty_tui::run(prog, refs, opts.dry_run);
    }

    // Counted before the guard and the cap count, so what they allow is
    // what can go.
    let seen = asked.unwrap_or_else(|| count(&dirs));
    #[cfg(test)]
    {
        if let Some(f) = tests::AFTER_COUNT.take() {
            f();
        }
    }

    if !opts.interactive && unscoped(&opts) {
        let entries = list::collect(&dirs);
        let latest = crate::undo::batches(&entries).first().map(|b| b.id.clone());
        let foreign = entries
            .iter()
            .filter(|e| latest.is_none() || e.batch != latest)
            .count();
        if !crate::budget::guard_ok(prog, entries.len(), foreign) {
            return 2;
        }
    }

    let cutoff = opts.days.map(|secs| info::now_epoch() - secs);
    let removed = AtomicU64::new(0);
    let bytes = AtomicU64::new(0);
    let errors = AtomicU64::new(0);
    let failures: Failures = Default::default();

    // A size budget alone removes nothing by age.
    let age_pass = !(opts.max_size.is_some() && cutoff.is_none());

    // A cap counts what would go first, so a refused empty deletes nothing.
    if !opts.dry_run && crate::permanent_guard::cap().is_some() {
        let mut n = if age_pass {
            would_remove(&dirs, cutoff)
        } else {
            0
        };
        if let Some(max) = opts.max_size {
            // The age cut runs first; gone_before leaves out what it takes.
            n += crate::retention::prune(
                prog,
                &dirs,
                &crate::retention::Prune {
                    max,
                    policy: opts.prune,
                    dry_run: true,
                    verbose: false,
                    protect_batch: None,
                    gone_before: cutoff,
                    free_inodes: 0,
                    only: Some(seen.as_slice()),
                },
            )
            .removed;
        }
        if let Some(code) = crate::permanent_guard::check_count(prog, n) {
            return code;
        }
    }
    // The first Ctrl-C stops after the items in hand and a second quits.
    // Only now: a press at a question above keeps its usual meaning.
    if !opts.dry_run {
        crate::interrupt::stop_then_quit();
    }
    for seen in seen.iter().filter(|_| age_pass) {
        if crate::interrupt::requested() {
            break;
        }
        let dir = seen.dir();
        // Exclusive lock for this trash root so put cannot interleave a half pair.
        let _lock = match trashdir::TrashLock::acquire(&dir.root) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("{prog}: cannot lock trash '{}': {e}", dir.root.display());
                errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        empty_one(
            prog, seen, cutoff, &opts, &removed, &bytes, &errors, &failures,
        );
    }

    if let Some(max) = opts.max_size {
        let done = crate::retention::prune(
            prog,
            &dirs,
            &crate::retention::Prune {
                max,
                policy: opts.prune,
                dry_run: opts.dry_run,
                verbose: opts.verbose,
                protect_batch: None,
                gone_before: cutoff,
                free_inodes: 0,
                only: Some(seen.as_slice()),
            },
        );
        removed.fetch_add(done.removed, Ordering::Relaxed);
        bytes.fetch_add(done.bytes, Ordering::Relaxed);
        errors.fetch_add(done.errors, Ordering::Relaxed);
    }

    let n = removed.load(Ordering::Relaxed);
    let b = bytes.load(Ordering::Relaxed);
    if opts.json {
        // Whole days as before; an age in hours or minutes gives days as a
        // fraction, and older_than_secs gives it exactly.
        let days = match opts.days {
            Some(s) if s % 86_400 == 0 => (s / 86_400).to_string(),
            Some(s) => format!("{}", s as f64 / 86_400.0),
            None => "null".into(),
        };
        let older_secs = opts.days.map_or("null".into(), |s| s.to_string());
        let max_size = match opts.max_size {
            Some(m) => m.to_string(),
            None => "null".into(),
        };
        println!(
            "{{\"dry_run\":{},\"removed\":{},\"bytes\":{},\"days\":{},\"older_than_secs\":{},\"max_size\":{}}}",
            if opts.dry_run { "true" } else { "false" },
            n,
            b,
            days,
            older_secs,
            max_size
        );
    } else {
        let verb = if opts.dry_run {
            "Would remove"
        } else {
            "Removed"
        };
        let noun = if n == 1 { "item" } else { "items" };
        if opts.verbose || opts.dry_run || n > 0 {
            if opts.dry_run {
                // Always show reclaim estimate on dry-run (the main reason to use -n).
                eprintln!(
                    "{verb} {n} {noun} ({}, approximately reclaimable)",
                    crate::fastdelete::format_bytes(b)
                );
            } else if opts.max_size.is_some() {
                eprintln!("{verb} {n} {noun} ({})", crate::fastdelete::format_bytes(b));
            } else {
                eprintln!("{verb} {n} {noun}");
            }
        }
    }
    let failures = failures.into_inner().unwrap_or_else(|p| p.into_inner());
    eprint!("{}", failure_report(prog, &failures));
    if !opts.dry_run && crate::interrupt::requested() {
        eprintln!("{prog}: stopped by Ctrl-C: {n} removed; the rest stay in the trash");
        return 130;
    }
    if errors.load(Ordering::Relaxed) > 0 {
        1
    } else {
        0
    }
}

/// `--until-free`: on each file system that holds a trash in scope and has less
/// free than `floor`, remove the oldest trash items there (the Finder
/// Trash first where it is the default) until it is back at `floor`.
fn until_free(prog: &str, spec: &str, floor: crate::reap::Floor, opts: &Opts) -> i32 {
    // One path per file system, keyed as reap keys them.
    let mut roots: Vec<PathBuf> = trashdir::resolve_dirs_opts(&opts.trash_dirs, opts.home_only)
        .into_iter()
        .map(|d| d.root)
        .collect();
    #[cfg(unix)]
    if crate::finder::is_default(!opts.trash_dirs.is_empty()) {
        roots.extend(crate::finder::trashes());
    }
    crate::reap::free_until(
        prog,
        &roots,
        spec,
        floor,
        opts.dry_run,
        opts.verbose,
        opts.json,
    )
}

/// How many items a plain empty of `dirs` would delete: every child of
/// files/ for a full empty, else the records older than `cutoff`.
fn would_remove(dirs: &[TrashDir], cutoff: Option<i64>) -> u64 {
    let old_record = |e: &fs::DirEntry, cut: i64| {
        let fname = e.file_name();
        let fname = crate::util::str_from_os_ref(&fname);
        if crate::util::trash_item_name(&fname).is_none() {
            return false;
        }
        let parsed = info::read_bounded(&e.path(), info::RECORD_CAP)
            .map(|c| info::parse_bytes(&c))
            .ok();
        is_old(
            parsed.as_ref().and_then(|i| i.deletion_date.as_deref()),
            cut,
        )
    };
    dirs.iter()
        .map(|dir| match cutoff {
            None => fs::read_dir(dir.files()).map_or(0, |r| r.count() as u64),
            Some(cut) => fs::read_dir(dir.info()).map_or(0, |r| {
                r.flatten().filter(|e| old_record(e, cut)).count() as u64
            }),
        })
        .sum()
}

/// Older-than-cutoff check. An entry with a missing or unreadable
/// DeletionDate is no older than the cutoff as far as anyone can tell, so
/// an age cut keeps it; a full empty still removes it.
fn is_old(date: Option<&str>, cutoff: i64) -> bool {
    date.and_then(info::parse_local_to_epoch)
        .is_some_and(|t| t <= cutoff)
}

/// The end-of-run list of what stayed in the trash, one line per item;
/// empty when everything went.
fn failure_report(prog: &str, failures: &[(PathBuf, String)]) -> String {
    if failures.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "{prog}: could not remove {}:\n",
        crate::util::plural(failures.len() as u64, "item", "items")
    );
    for (path, why) in failures {
        out.push_str(&format!("  {}: {why}\n", crate::util::err_path(path)));
    }
    out
}

/// Items that could not be removed: where, and why.
type Failures = std::sync::Mutex<Vec<(PathBuf, String)>>;

fn fail(failures: &Failures, errors: &AtomicU64, path: PathBuf, e: &io::Error) {
    errors.fetch_add(1, Ordering::Relaxed);
    failures
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push((path, e.to_string()));
}

/// Empty the trash `seen` counted, under its lock, taking only the items
/// it counted.
#[allow(clippy::too_many_arguments)]
fn empty_one(
    prog: &str,
    seen: &trashdir::Seen,
    cutoff: Option<i64>,
    opts: &Opts,
    removed: &AtomicU64,
    bytes: &AtomicU64,
    errors: &AtomicU64,
    failures: &Failures,
) {
    let dir = seen.dir();
    let info_dir = dir.info();
    let files_dir = dir.files();
    if !opts.dry_run {
        crate::trash_total::touched(dir);
    }

    // Full empty (no age filter): real wipe, or dry-run that inventories the
    // same two roots (every child of files/ and info/), not only .trashinfo rows.
    if cutoff.is_none() {
        if opts.dry_run {
            full_empty_dry_run(dir, opts, removed, bytes);
        } else {
            let before = errors.load(Ordering::Relaxed);
            let left = full_empty_wipe(prog, seen, opts, removed, errors, failures);
            // An empty that took everything leaves a known total; one that
            // failed somewhere, stopped or left new items leaves it stale
            // for the next reader.
            if errors.load(Ordering::Relaxed) == before
                && !crate::interrupt::requested()
                && left == 0
            {
                crate::trash_total::store(dir, 0, 0);
            } else {
                crate::trash_total::forget(dir);
            }
        }
        return;
    }

    let mut kept_names: Vec<String> = Vec::new();
    // Items that came after the count, or took the name of one it counted.
    let mut late = 0u64;
    let mut victims: Vec<(PathBuf, Option<PathBuf>, PathBuf)> = Vec::new(); // (target, info, display)

    if let Ok(entries) = fs::read_dir(&info_dir) {
        for entry in entries.flatten() {
            let info_path = entry.path();
            let fname = entry.file_name();
            let fname = crate::util::str_from_os_ref(&fname);
            let Some(name) = crate::util::trash_item_name(&fname) else {
                continue;
            };
            let take = match cutoff {
                None => true,
                Some(cut) => {
                    let parsed = info::read_bounded(&info_path, info::RECORD_CAP)
                        .map(|c| info::parse_bytes(&c))
                        .ok();
                    is_old(
                        parsed.as_ref().and_then(|i| i.deletion_date.as_deref()),
                        cut,
                    )
                }
            };
            // Under the lock, only an item that the count saw.
            if take && !seen.saw_item(&crate::util::os_from_str(name)) {
                late += 1;
                kept_names.push(name.to_string());
                continue;
            }
            if take {
                // -v names the original path, as trash-cli users asked.
                let shown = if opts.verbose {
                    original_of(dir, &info_path)
                } else {
                    None
                };
                victims.push((
                    crate::util::join_name(&files_dir, name),
                    Some(info_path),
                    shown.unwrap_or_else(|| crate::util::join_name(&dir.root.join("files"), name)),
                ));
            } else {
                kept_names.push(name.to_string());
            }
        }
    }

    // Orphans in files/ with no .trashinfo are stale debris; purge them on a
    // full empty only (a days filter has no date to compare against).
    if cutoff.is_none() {
        if let Ok(entries) = fs::read_dir(&files_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let mut info_name = name;
                info_name.push(".trashinfo");
                let has_info = info_dir.join(info_name).exists();
                if !has_info {
                    let p = entry.path();
                    victims.push((p.clone(), None, p));
                }
            }
        }
    }

    report_late(prog, dir, late);

    let sizes = trashdir::DirSizes::load(dir);
    // A total current before this pass loses what the pass takes.
    let total = (!opts.dry_run)
        .then(|| crate::trash_total::cached(dir))
        .flatten();
    let gone = AtomicU64::new(0);
    let gone_items = AtomicU64::new(0);
    victims.par_iter().for_each(|(target, info_path, display)| {
        if opts.dry_run {
            // Prefer FreeDesktop directorysizes for directory payloads; fall back
            // to a disk_usage walk. Always add the .trashinfo size when present.
            let name = target
                .file_name()
                .map(crate::util::str_from_os_ref)
                .unwrap_or_default();
            let sz = if info_path.is_some() && !name.is_empty() {
                sizes.entry_bytes(&name)
            } else {
                crate::fastdelete::disk_usage(target)
                    + info_path
                        .as_ref()
                        .map(|p| crate::fastdelete::disk_usage(p))
                        .unwrap_or(0)
            };
            bytes.fetch_add(sz, Ordering::Relaxed);
            if opts.verbose {
                println!(
                    "would remove {} ({})",
                    display.display(),
                    crate::fastdelete::format_bytes(sz)
                );
            }
            removed.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // After a Ctrl-C, the items not started stay with their records.
        if crate::interrupt::requested() {
            return;
        }
        let sz = match (&total, info_path) {
            (Some(_), Some(_)) => target
                .file_name()
                .map_or(0, |n| sizes.entry_bytes(&crate::util::str_from_os_ref(n))),
            _ => 0,
        };
        match crate::permanent_guard::remove(target) {
            Ok(()) => {
                if info_path.is_some() {
                    gone.fetch_add(sz, Ordering::Relaxed);
                    gone_items.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(ip) = info_path {
                    // The payload is gone: its record goes too before a
                    // Ctrl-C acts.
                    let section = crate::interrupt::critical();
                    match fs::remove_file(ip) {
                        Ok(()) => {}
                        // A concurrent empty may have won the race.
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        Err(e) => fail(failures, errors, ip.clone(), &e),
                    }
                    drop(section);
                    crate::interrupt::test_seam("after-item");
                }
                if opts.verbose {
                    println!("removed {}", crate::util::term_path(display));
                }
                removed.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => fail(failures, errors, display.clone(), &e),
        }
    });

    if opts.dry_run {
        sizes.write_back_locked();
    } else {
        if crate::interrupt::requested() {
            // Stopped: the victims not reached are not in kept_names, so
            // keep every line whose payload is still there instead.
            trashdir::DirSizes::load(dir).write_back_locked();
        } else {
            prune_directorysizes(dir, &kept_names);
        }
        if let Some((b, n)) = total {
            crate::trash_total::store(
                dir,
                b.saturating_sub(gone.into_inner()),
                n.saturating_sub(gone_items.into_inner()),
            );
        } else {
            crate::trash_total::forget(dir);
        }
    }
}

/// Say on stderr that `late` items in `dir`, put after the count, stayed.
fn report_late(prog: &str, dir: &TrashDir, late: u64) {
    if late > 0 {
        eprintln!(
            "{prog}: left {} that came into '{}' after the count; empty again to take them",
            crate::util::plural(late, "item", "items"),
            dir.root.display()
        );
    }
}

/// The original path a `.trashinfo` records, resolved against its trash.
fn original_of(dir: &TrashDir, info_path: &std::path::Path) -> Option<PathBuf> {
    let raw = info::read_bounded(info_path, info::RECORD_CAP).ok()?;
    let rec = info::parse_bytes(&raw);
    (!rec.path.as_os_str().is_empty()).then(|| dir.resolve_original(&rec.path))
}

/// Dry-run inventory matching full wipe: every top-level child of `files/` and
/// non-counted-as-item names under `info/` still contribute reclaim size; item
/// count is top-level `files/` children (same as live wipe `n_files`).
fn full_empty_dry_run(dir: &TrashDir, opts: &Opts, removed: &AtomicU64, bytes: &AtomicU64) {
    let files_dir = dir.files();
    let info_dir = dir.info();
    let mut n_files = 0u64;
    let mut total = 0u64;
    let sizes = trashdir::DirSizes::load(dir);
    if let Ok(entries) = fs::read_dir(&files_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            // Directory payloads: use validated directorysizes when present.
            let sz = sizes.payload_bytes(&crate::util::str_from_os_ref(&entry.file_name()));
            total = total.saturating_add(sz);
            n_files += 1;
            if opts.verbose {
                let mut info = entry.file_name();
                info.push(".trashinfo");
                let shown = original_of(dir, &info_dir.join(info)).unwrap_or(p);
                println!(
                    "would remove {} ({})",
                    crate::util::term_path(&shown),
                    crate::fastdelete::format_bytes(sz)
                );
            }
        }
    }
    if let Ok(entries) = fs::read_dir(&info_dir) {
        for entry in entries.flatten() {
            total = total.saturating_add(crate::fastdelete::disk_usage(&entry.path()));
        }
    }
    sizes.write_back_locked();
    removed.fetch_add(n_files, Ordering::Relaxed);
    bytes.fetch_add(total, Ordering::Relaxed);
}

/// Full empty: wipe `files/` and `info/`. Skip expensive pre-scans unless
/// verbose (count comes from the wipe itself). Only what `seen` counted
/// goes; the return is how many items came after the count and stayed.
fn full_empty_wipe(
    prog: &str,
    seen: &trashdir::Seen,
    opts: &Opts,
    removed: &AtomicU64,
    errors: &AtomicU64,
    failures: &Failures,
) -> u64 {
    let dir = seen.dir();
    let info_dir = dir.info();
    let files_dir = dir.files();
    // A payload put after the count, or in the place of one it counted,
    // stays with its record; so does a record put after the count. The
    // trash is locked, so the names looked up once here stay true.
    let names: Vec<std::ffi::OsString> = fs::read_dir(&files_dir)
        .map(|r| r.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    let late_names: std::collections::HashSet<&std::ffi::OsStr> = names
        .iter()
        .map(|n| n.as_os_str())
        .filter(|n| !seen.saw_payload(n))
        .collect();
    let late = |name: &std::ffi::OsStr| late_names.contains(name);
    let late_record = |rec: &std::ffi::OsStr| !seen.saw_record(rec);

    // Verbose still lists intended removals before delete (stable UX),
    // by original path where a record names one.
    if opts.verbose {
        if let Ok(entries) = fs::read_dir(&files_dir) {
            for entry in entries
                .flatten()
                .filter(|e| !late(e.file_name().as_os_str()))
            {
                let mut info = entry.file_name();
                info.push(".trashinfo");
                let shown = original_of(dir, &info_dir.join(info)).unwrap_or_else(|| entry.path());
                println!("removed {}", crate::util::term_path(&shown));
            }
        }
    }

    // A payload that cannot go stays, with its .trashinfo; the rest of the
    // trash still empties, and the failures are reported at the end.
    // The whole trash goes at once, so the cap takes every item or none.
    let mut kept: Vec<String> = late_names
        .iter()
        .map(|n| crate::util::str_from_os_ref(n))
        .collect();
    let left = kept.len() as u64;
    if let Err(e) = crate::permanent_guard::gate_many(names.len() as u64 - left) {
        eprintln!("{prog}: {e}");
        errors.fetch_add(1, Ordering::Relaxed);
        return left;
    }
    let (wf, wi) = crate::fastdelete::wipe_two_parallel(&files_dir, &info_dir, late, late_record);
    match wf {
        Ok(w) => {
            // Top-level `files/` children ≈ trashed items (incl. orphans).
            removed.fetch_add(w.removed, Ordering::Relaxed);
            // A Ctrl-C left these with their records.
            for name in &w.not_reached {
                kept.push(crate::util::str_from_os_ref(name));
            }
            for (name, e) in w.failed {
                kept.push(crate::util::str_from_os_ref(&name));
                fail(failures, errors, files_dir.join(&name), &e);
            }
        }
        Err(e) => {
            eprintln!(
                "{prog}: cannot wipe trash under '{}': {e}",
                dir.root.display()
            );
            errors.fetch_add(1, Ordering::Relaxed);
            return left;
        }
    }
    match wi {
        Ok(w) => {
            for (name, e) in w.failed {
                fail(failures, errors, info_dir.join(&name), &e);
            }
        }
        Err(e) => fail(failures, errors, info_dir.clone(), &e),
    }

    prune_directorysizes(dir, &kept);
    report_late(prog, dir, left);
    left
}

/// Rewrite the spec's `directorysizes` cache keeping only surviving entries;
/// the file goes when nothing survives.
fn prune_directorysizes(dir: &TrashDir, kept: &[String]) {
    trashdir::directorysizes_keep(dir, kept);
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// Set by a test: run once on this thread after an empty has
        /// counted the trash, and asked about it, and before it locks it;
        /// for `--dangling`, after it has found the dangling records.
        pub(super) static AFTER_COUNT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// A trash under the temp dir holding one item per name, all trashed
    /// in 2001 so that any age cut takes them.
    fn old_trash(tag: &str, names: &[&str]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rtrash-empty-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("files")).unwrap();
        fs::create_dir_all(root.join("info")).unwrap();
        for name in names {
            put_old(&root, name);
        }
        root
    }

    /// Put an item `name` into the trash at `root`, trashed in 2001. A
    /// payload already there gives way to a new file, which never gets
    /// its inode: the new file exists before the old one goes.
    fn put_old(root: &std::path::Path, name: &str) {
        let new = root.join(format!("{name}.new"));
        fs::write(&new, name).unwrap();
        fs::rename(&new, root.join("files").join(name)).unwrap();
        let record = format!("[Trash Info]\nPath=/tmp/{name}\nDeletionDate=2001-01-01T00:00:00\n");
        fs::write(root.join("info").join(format!("{name}.trashinfo")), record).unwrap();
    }

    /// Empty the trash at `root` with `args`, while the hook, between the
    /// count and the lock, puts a new item `c` and a new payload under the
    /// counted name `b`. Both must stay with their records; `a` goes.
    fn empty_with_late_items(tag: &str, args: &[&str]) {
        let root = old_trash(tag, &["a", "b"]);
        let r = root.clone();
        AFTER_COUNT.set(Some(Box::new(move || {
            put_old(&r, "c");
            put_old(&r, "b");
        })));
        let mut argv = vec![
            "--plain".to_string(),
            format!("--trash-dir={}", root.display()),
        ];
        argv.extend(args.iter().map(|a| a.to_string()));
        assert_eq!(run_inner("rtrash", &argv), 0);
        assert!(AFTER_COUNT.take().is_none(), "the hook never ran");
        assert!(!root.join("files/a").exists());
        assert!(!root.join("info/a.trashinfo").exists());
        for kept in ["files/b", "info/b.trashinfo", "files/c", "info/c.trashinfo"] {
            assert!(root.join(kept).exists(), "{kept} went");
        }
        let _ = fs::remove_dir_all(&root);
    }

    /// `--dangling` reads the records again under the lock and takes only
    /// those it counted: one whose payload came back since the count stays,
    /// and so does one that dangles only since then (round 246).
    #[test]
    fn dangling_takes_only_the_records_it_counted() {
        let root = old_trash("dangling-late", &["gone", "back", "kept"]);
        for name in ["gone", "back"] {
            fs::remove_file(root.join("files").join(name)).unwrap();
        }
        let r = root.clone();
        AFTER_COUNT.set(Some(Box::new(move || {
            fs::write(r.join("files/back"), b"back").unwrap();
            fs::remove_file(r.join("files/kept")).unwrap();
        })));
        let argv = [
            "--dangling".to_string(),
            format!("--trash-dir={}", root.display()),
        ];
        assert_eq!(run_inner("rtrash", &argv), 0);
        assert!(AFTER_COUNT.take().is_none(), "the hook never ran");
        assert!(!root.join("info/gone.trashinfo").exists());
        assert!(root.join("info/back.trashinfo").exists());
        assert!(root.join("info/kept.trashinfo").exists());
        let _ = fs::remove_dir_all(&root);
    }

    /// A full empty takes only what it counted before the lock (round 245).
    #[test]
    fn a_full_empty_spares_what_came_after_its_count() {
        empty_with_late_items("late-full", &[]);
    }

    /// So does an age cut: the late items are as old as the cut asks.
    #[test]
    fn an_age_cut_keeps_late_items_as_old_as_the_cut() {
        empty_with_late_items("late-age", &["1"]);
    }

    /// And a size budget: the late items count toward it but stay.
    #[test]
    fn a_size_budget_counts_late_items_but_never_picks_them() {
        empty_with_late_items("late-size", &["--max-size=0"]);
    }

    #[test]
    fn failure_report_lists_each_item_with_its_reason() {
        assert_eq!(failure_report("rtrash", &[]), "");
        let f = vec![
            (
                PathBuf::from("/t/files/a"),
                "/t/files/a/x: Permission denied".to_string(),
            ),
            (
                PathBuf::from("/t/files/b"),
                "Operation not permitted".to_string(),
            ),
        ];
        assert_eq!(
            failure_report("rtrash", &f),
            "rtrash: could not remove 2 items:\n  /t/files/a: /t/files/a/x: Permission denied\n  /t/files/b: Operation not permitted\n"
        );
    }

    #[test]
    fn missing_date_is_never_old() {
        // An unknown age never passes an age cut.
        assert!(!is_old(None, i64::MAX));
        assert!(!is_old(Some("garbage"), i64::MAX));
    }

    #[test]
    fn cutoff_comparison() {
        let now = info::now_local_string();
        assert!(!is_old(Some(&now), info::now_epoch() - 3600));
        assert!(is_old(
            Some("2001-01-01T00:00:00"),
            info::now_epoch() - 3600
        ));
    }
}
