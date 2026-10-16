use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use crate::list;
use crate::trashdir;
use crate::util::{scrub, stderr_is_tty, stdin_is_tty, stdout_is_tty};

const HELP: &str = "\
Usage: {prog} [OPTION]... [PATH]
Restore a trashed item to its original location.

With PATH, restore the item originally at PATH (exact original path match).
A PATH with * or ? (quoted, so the shell leaves it) works as --glob.
With --name=NAME, restore the item whose file name, or whose name inside
the trash, is NAME. --glob and --regex match many items at once. When several
items match, --latest restores the most recently trashed one and --all
restores every one, both without asking.
Without PATH on a TTY, open the interactive restore browser (ratatui): filter,
navigate, restore one or more items. Without a TTY (or with a piped index),
print a numbered table and read a selection from stdin: indices, ranges
and commas such as '0-2,5', or 'all'.

By default every trashed item in scope is listed; pass --cwd-only to limit to
items whose original path is under the current directory. A single match
restores immediately.
Run as trash-restore, it follows trash-cli: without PATH it offers only items
from under the current directory, PATH also matches items trashed from
under it, a single match still asks, and the table uses single spaces.

The browser lists the most recently trashed item first, with the cursor on
it. The numbered table lists oldest first, so the newest item sits next to
the index prompt. --newest-first and --oldest-first set the order for both.

  -f, --force     overwrite an existing file (same as --conflict=overwrite)
      --overwrite  same as --force (trash-cli spelling)
      --sort=ORDER  numbered table order: date (oldest first, default),
                    path (original path) or none (as read)
      --conflict=POLICY  when the destination exists: refuse (default),
                    overwrite, rename (restore next to it as NAME.1.EXT)
                    or skip
      --to=DIR    restore into DIR, keeping each item's file name
      --cwd-only  without PATH, only items trashed from under the current directory
      --directory=DIR  without PATH, only items trashed from under DIR
                    (which need not exist any more)
      --plain     force numbered line selection even on a TTY (no TUI)
      --no-space-check  copy across devices even when the copy would leave
                    the target under 5% of its bytes or inodes free, or
                    under RTRASH_MIN_FREE bytes
      --newest-first  list the most recently trashed item first
      --oldest-first  list the oldest trashed item first
      --name=NAME  match items by file name or trash name instead of PATH
      --glob=PATTERN  match items by shell pattern: against the original
                    path when PATTERN has a '/', else against the file name
      --regex=REGEX  match items whose original path contains REGEX
      --latest    of several matches, restore the most recently trashed
      --all       of several matches, restore every one
  -n, --dry-run   show where each chosen item would go and change nothing;
                    a conflict prints as would skip or would fail
      --batch=ID  restore one whole put run, as `undo ID` does: an ID
                    from `undo --list`, or last for the latest run
      --json      print {restored, failed, skipped} as JSON on stdout;
                    implies --plain, and the numbered table goes to stderr
      --home-only  only the home trash (skip volume trash)
      --trash-dir=PATH  only consider this trash directory (repeatable)
      --help      display this help and exit
      --version   output version information and exit
";

/// The `--sort` orders of trash-cli's trash-restore.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sort {
    Date,
    Path,
    None,
}

impl Sort {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "date" => Sort::Date,
            "path" => Sort::Path,
            "none" => Sort::None,
            _ => return None,
        })
    }
}

/// What to do when the restore destination already exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conflict {
    Refuse,
    Overwrite,
    Rename,
    Skip,
}

impl Conflict {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "refuse" => Conflict::Refuse,
            "overwrite" => Conflict::Overwrite,
            "rename" => Conflict::Rename,
            "skip" => Conflict::Skip,
            _ => return None,
        })
    }
}

/// How the CLI picks entries besides PATH.
pub(crate) enum Matcher {
    Name(String),
    Glob(String),
    Regex(regex_lite::Regex),
}

impl Matcher {
    /// The same test for an item known by its original path and its name
    /// in the trash (a Finder Trash item).
    pub(crate) fn matches_parts(&self, original: &Path, name: &str) -> bool {
        // Patterns were made composed (NFC) when parsed; compare the same
        // form of the path and name.
        let original = &crate::finder_shared::nfc_path(original);
        let name = &crate::finder_shared::nfc(name);
        match self {
            Matcher::Name(n) => name_matches_parts(original, name, n),
            Matcher::Glob(p) => glob_matches_parts(original, name, p),
            Matcher::Regex(re) => re.is_match(&crate::util::str_from_os_ref(original.as_os_str())),
        }
    }

    fn matches(&self, e: &list::Entry) -> bool {
        self.matches_parts(&e.original, &e.name)
    }
}

pub fn run(prog: &str, args: &[String]) -> i32 {
    let snap = crate::finder_total::snapshot();
    let code = run_inner(prog, args);
    crate::trash_total::log_touched("restore");
    crate::finder_total::log("restore", &snap);
    code
}

/// `restore --batch=ID|last`: the put run as a unit, which `undo` does.
/// Only the options undo shares pass through.
fn by_batch(prog: &str, args: &[String]) -> Option<i32> {
    let id = args.iter().find_map(|a| a.strip_prefix("--batch="))?;
    if id.is_empty() {
        eprintln!("{prog}: --batch needs an ID (or last)");
        return Some(2);
    }
    let mut pass: Vec<String> = Vec::new();
    for a in args {
        match a.as_str() {
            a if a.starts_with("--batch=") => {}
            "-f" | "--force" | "--overwrite" => pass.push("--force".into()),
            "--json" | "--home-only" => pass.push(a.to_string()),
            "-n" | "--dry-run" => pass.push("--dry-run".into()),
            a if a.starts_with("--conflict=") || a.starts_with("--trash-dir=") => {
                pass.push(a.to_string())
            }
            a => {
                eprintln!(
                    "{prog}: --batch takes only --force, --conflict, --dry-run, --json, --home-only and --trash-dir, not '{a}'"
                );
                return Some(2);
            }
        }
    }
    if id != "last" {
        pass.push(id.to_string());
    }
    Some(crate::undo::run(prog, &pass))
}

fn run_inner(prog: &str, args: &[String]) -> i32 {
    if let Some(code) = by_batch(prog, args) {
        return code;
    }
    let mut conflict = Conflict::Refuse;
    let mut home_only = false;
    let mut cwd_only = false;
    let mut directory: Option<PathBuf> = None;
    let mut plain = false;
    let mut newest_first: Option<bool> = None;
    let mut target: Option<PathBuf> = None;
    let mut matcher: Option<Matcher> = None;
    let mut to: Option<PathBuf> = None;
    let mut latest = false;
    let mut all = false;
    let mut json = false;
    let mut dry_run = false;
    let mut trash_dirs: Vec<PathBuf> = Vec::new();
    let mut sort: Option<Sort> = None;
    // Run as trash-restore, behave as trash-cli's command does.
    let trash_cli = prog == "trash-restore";
    for arg in args {
        let mut set_matcher = |m: Matcher| {
            if matcher.is_some() {
                eprintln!("{prog}: give only one of --name, --glob and --regex");
                return false;
            }
            matcher = Some(m);
            true
        };
        match arg.as_str() {
            "-f" | "--force" | "--overwrite" => conflict = Conflict::Overwrite,
            a if a.starts_with("--sort=") => match Sort::parse(&a["--sort=".len()..]) {
                Some(k) => sort = Some(k),
                None => {
                    eprintln!(
                        "{prog}: invalid --sort '{}' (date, path or none)",
                        &a["--sort=".len()..]
                    );
                    return 2;
                }
            },
            "--latest" => latest = true,
            "-n" | "--dry-run" => dry_run = true,
            "--no-space-check" => crate::space::skip_check(true),
            "--all" => all = true,
            "--json" => {
                json = true;
                plain = true;
            }
            a if a.starts_with("--conflict=") => match Conflict::parse(&a["--conflict=".len()..]) {
                Some(c) => conflict = c,
                None => {
                    eprintln!(
                        "{prog}: invalid --conflict '{}' (refuse, overwrite, rename or skip)",
                        &a["--conflict=".len()..]
                    );
                    return 2;
                }
            },
            a if a.starts_with("--to=") => {
                let d = &a["--to=".len()..];
                if d.is_empty() {
                    eprintln!("{prog}: --to needs a non-empty DIR");
                    return 2;
                }
                to = Some(crate::util::arg_path(d));
            }
            a if a.starts_with("--name=") => {
                let n = &a["--name=".len()..];
                if n.is_empty() {
                    eprintln!("{prog}: --name needs a non-empty NAME");
                    return 2;
                }
                if !set_matcher(Matcher::Name(crate::finder_shared::nfc(n))) {
                    return 2;
                }
            }
            a if a.starts_with("--glob=") => {
                let g = &a["--glob=".len()..];
                if g.is_empty() {
                    eprintln!("{prog}: --glob needs a non-empty PATTERN");
                    return 2;
                }
                if !set_matcher(Matcher::Glob(crate::finder_shared::nfc(g))) {
                    return 2;
                }
            }
            a if a.starts_with("--regex=") => {
                let r = &a["--regex=".len()..];
                match regex_lite::Regex::new(&crate::finder_shared::nfc(r)) {
                    Ok(re) => {
                        if !set_matcher(Matcher::Regex(re)) {
                            return 2;
                        }
                    }
                    Err(e) => {
                        eprintln!("{prog}: invalid --regex '{r}': {e}");
                        return 2;
                    }
                }
            }
            "--home-only" => home_only = true,
            "--cwd-only" => cwd_only = true,
            a if a.starts_with("--directory=") => {
                let d = &a["--directory=".len()..];
                if d.is_empty() {
                    eprintln!("{prog}: --directory needs a non-empty DIR");
                    return 2;
                }
                directory = Some(absolutize(&crate::util::arg_path(d)));
            }
            "--plain" => plain = true,
            "--newest-first" => newest_first = Some(true),
            "--oldest-first" => newest_first = Some(false),
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
            a if a.starts_with('-') && a.len() > 1 => {
                eprintln!("{prog}: unrecognized option '{a}'");
                return 2;
            }
            a => {
                if target.is_some() {
                    eprintln!("{prog}: only one PATH may be given");
                    return 2;
                }
                // A quoted '*.log' the shell left alone is a glob; a literal
                // name with * or ? still goes through --name.
                if a.contains(['*', '?']) && matcher.is_none() {
                    matcher = Some(Matcher::Glob(crate::finder_shared::nfc(a)));
                    continue;
                }
                target = Some(crate::util::arg_path(a));
            }
        }
    }

    if target.is_some() && matcher.is_some() {
        eprintln!("{prog}: give PATH or a --name, --glob or --regex match, not both");
        return 2;
    }
    if cwd_only && directory.is_some() {
        eprintln!("{prog}: give --cwd-only or --directory, not both");
        return 2;
    }
    if latest && all {
        eprintln!("{prog}: give --latest or --all, not both");
        return 2;
    }
    if let Some(d) = &to {
        if !d.is_dir() {
            eprintln!("{prog}: --to: '{}' is not a directory", d.display());
            return 2;
        }
    }

    // macOS restores from the Finder Trash unless a pin or the setting
    // says otherwise.
    if crate::finder::is_default(!trash_dirs.is_empty()) {
        // --cwd-only is --directory on the current directory, and so is
        // trash-restore without PATH or a match, as trash-cli scopes it.
        let cwd_scope = trash_cli && target.is_none() && matcher.is_none();
        let scope = match (&directory, cwd_only || cwd_scope) {
            (None, true) => match std::env::current_dir() {
                Ok(d) => Some(d),
                Err(e) => {
                    eprintln!("{prog}: cannot read the current directory: {e}");
                    return 2;
                }
            },
            _ => directory.clone(),
        };
        return crate::finder::default_restore(
            prog,
            &crate::finder::RestoreOpts {
                target: target.as_deref(),
                matcher: matcher.as_ref(),
                directory: scope.as_deref(),
                cwd_note: cwd_scope && directory.is_none(),
                all,
                latest,
                to: to.as_deref(),
                conflict,
                json,
                plain,
                newest_first,
                dry_run,
            },
        );
    }
    let dirs = trashdir::resolve_dirs_opts(&trash_dirs, home_only);
    if dirs.is_empty() && !trash_dirs.is_empty() {
        eprintln!("{prog}: no valid --trash-dir pins (need files/ and info/ directories)");
        return 2;
    }
    let entries = list::collect(&dirs);
    let mut matches: Vec<&list::Entry> = match (&target, &matcher) {
        // trash-cli takes PATH as a scope: the item there and everything
        // trashed from under it.
        (Some(t), _) if trash_cli => {
            let abs = absolutize(t);
            entries
                .iter()
                .filter(|e| e.original.starts_with(&abs))
                .collect()
        }
        (Some(t), _) => {
            let abs = absolutize(t);
            entries.iter().filter(|e| e.original == abs).collect()
        }
        (None, Some(m)) => entries.iter().filter(|e| m.matches(e)).collect(),
        // --directory scopes like --cwd-only, to a directory that may be gone.
        (None, None) if directory.is_some() => {
            let dir = directory.as_deref().unwrap_or(Path::new("/"));
            let real = std::fs::canonicalize(dir).ok();
            entries
                .iter()
                .filter(|e| {
                    e.original.starts_with(dir)
                        || real.as_ref().is_some_and(|r| e.original.starts_with(r))
                })
                .collect()
        }
        (None, None) if cwd_only || trash_cli => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            entries
                .iter()
                .filter(|e| e.original.starts_with(&cwd))
                .collect()
        }
        // Bare `rtrash restore` offers every item in scope; trash-restore
        // keeps trash-cli's current directory scope above.
        (None, None) => entries.iter().collect(),
    };

    let plan = Plan {
        to: to.as_deref(),
        conflict,
        quiet: json,
        dry_run,
    };
    if matches.is_empty() && trash_cli && target.is_none() && matcher.is_none() {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        println!("No files trashed from current dir ('{}')", cwd.display());
        return 1;
    }
    if matches.is_empty() {
        eprintln!("{prog}: no trashed files match");
        if json {
            println!("{}", restored_json(&[], 0, 0));
        }
        return 1;
    }
    if latest {
        order_entries(&mut matches, true);
        matches.truncate(1);
    }

    // TTY + multi-item: prefer ratatui browser when built with `tui` (unless --plain).
    #[cfg(feature = "tui")]
    if !plain
        && !dry_run
        && !all
        && target.is_none()
        && matcher.is_none()
        && to.is_none()
        && matches!(conflict, Conflict::Refuse | Conflict::Overwrite)
        && matches.len() > 1
        && stdin_is_tty()
    {
        let newest = newest_first.unwrap_or(true);
        order_entries(&mut matches, newest);
        return crate::restore_tui::run(prog, matches, conflict == Conflict::Overwrite, newest);
    }
    #[cfg(not(feature = "tui"))]
    let _ = plain;
    match sort {
        Some(Sort::Path) => matches.sort_by(|a, b| a.original.cmp(&b.original)),
        Some(Sort::None) => {}
        Some(Sort::Date) | None => order_entries(&mut matches, newest_first.unwrap_or(false)),
    }

    // trash-cli asks even for one match; rtrash restores it right away.
    let chosen: Vec<&list::Entry> = if (matches.len() == 1 && !trash_cli) || all || latest {
        matches
    } else {
        for (i, e) in matches.iter().enumerate() {
            let date = e.date.as_deref().unwrap_or("????-??-??T??:??:??");
            let gone = if e.missing() { "  [payload gone]" } else { "" };
            // trash-cli separates the columns with one space.
            let gap = if trash_cli { " " } else { "  " };
            let row = format!(
                "{i:4}{gap}{}{gap}{}{gone}",
                date.replacen('T', " ", 1),
                e.original.display()
            );
            // The row goes to stderr with --json, so scrub for that stream.
            let tty = if json {
                stderr_is_tty()
            } else {
                stdout_is_tty()
            };
            let row = if tty { scrub(&row).into_owned() } else { row };
            // Keep stdout for the JSON summary.
            if json {
                eprintln!("{row}");
            } else {
                println!("{row}");
            }
        }
        // Piped index for scripts / tests: `printf '0\n' | rtrash restore`.
        if stdin_is_tty() {
            eprint!("What file to restore [0..{}]: ", matches.len() - 1);
            io::stderr().flush().ok();
        }
        let mut line = String::new();
        if io::stdin().lock().read_line(&mut line).is_err() {
            eprintln!("{prog}: failed to read selection from stdin");
            return 1;
        }
        if line.trim().is_empty() && trash_cli {
            println!("No files were restored");
            return 0;
        }
        if line.trim().is_empty() {
            eprintln!(
                "{prog}: {} matches; provide an index 0..{} on stdin",
                matches.len(),
                matches.len() - 1
            );
            return 1;
        }
        match parse_selection(&line, matches.len()) {
            Ok(idx) => idx.into_iter().map(|i| matches[i]).collect(),
            Err(e) => {
                eprintln!("{prog}: invalid selection: {e}");
                return 1;
            }
        }
    };

    let mut done: Vec<(&list::Entry, PathBuf)> = Vec::new();
    let mut failed = 0usize;
    let mut skipped = 0usize;
    for e in chosen {
        match plan.apply(prog, e) {
            Outcome::Restored(dest) => done.push((e, dest)),
            Outcome::Skipped => skipped += 1,
            Outcome::Failed => failed += 1,
        }
    }
    if json {
        let body = restored_json(&done, failed, skipped);
        if dry_run {
            println!("{{\"dry_run\":true,{}", &body[1..]);
        } else {
            println!("{body}");
        }
    }
    i32::from(failed > 0)
}

/// Where and how the CLI restores each chosen entry.
pub(crate) struct Plan<'a> {
    pub to: Option<&'a Path>,
    pub conflict: Conflict,
    pub quiet: bool,
    /// Report each outcome as would restore, would skip or would fail,
    /// and move nothing.
    pub dry_run: bool,
}

pub(crate) enum Outcome {
    Restored(PathBuf),
    Skipped,
    Failed,
}

impl Plan<'_> {
    pub(crate) fn apply(&self, prog: &str, e: &list::Entry) -> Outcome {
        let dest = match (self.to, e.original.file_name()) {
            // A relative --to counts from here, as the shell would.
            (Some(dir), Some(name)) if dir.is_absolute() => dir.join(name),
            (Some(dir), Some(name)) => std::env::current_dir()
                .map(|cwd| cwd.join(dir).join(name))
                .unwrap_or_default(),
            (Some(_), None) => {
                eprintln!(
                    "{prog}: cannot restore '{}' by name: no file name",
                    crate::util::err_path(&e.original)
                );
                return Outcome::Failed;
            }
            (None, _) => e.original.clone(),
        };
        let exists = dest.symlink_metadata().is_ok();
        let (dest, force) = match (exists, self.conflict) {
            (true, Conflict::Skip) => {
                if self.dry_run {
                    self.say(format!(
                        "would skip '{}': it exists",
                        crate::util::term_path(&dest)
                    ));
                } else {
                    eprintln!(
                        "{prog}: skipping '{}': it exists",
                        crate::util::err_path(&dest)
                    );
                }
                return Outcome::Skipped;
            }
            (true, Conflict::Rename) => (free_name(&dest), false),
            (_, c) => (dest, c == Conflict::Overwrite),
        };
        if self.dry_run {
            return self.dry(e, dest, force);
        }
        match restore_one_to(e, &dest, force) {
            Ok(done) => {
                for w in &done.warnings {
                    eprintln!("{prog}: warning: {}", crate::util::err_text(w));
                }
                if !self.quiet {
                    println!("restored '{}'", crate::util::term_path(&done.dest));
                }
                Outcome::Restored(done.dest)
            }
            Err(err) => {
                // The error quotes paths from the record verbatim.
                eprintln!("{prog}: {}", crate::util::err_text(&err));
                Outcome::Failed
            }
        }
    }
}

impl Plan<'_> {
    /// One dry-run line: on stdout, or on stderr when stdout carries JSON.
    fn say(&self, line: String) {
        if self.quiet {
            let line = if stderr_is_tty() {
                scrub(&line).into_owned()
            } else {
                line
            };
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }

    /// What restoring `e` to `dest` would do, checked without moving it:
    /// a relative record, a target inside the trash, a gone payload and a
    /// blocked destination fail as the real restore would.
    fn dry(&self, e: &list::Entry, dest: PathBuf, force: bool) -> Outcome {
        let src = crate::util::join_name(&e.dir.files(), &e.name);
        let why = if !dest.is_absolute() {
            Some(format!(
                "'{}' names no absolute location",
                crate::util::term_path(&dest)
            ))
        } else if let Some(err) = trash_conflict(e, &dest) {
            Some(err)
        } else if src.symlink_metadata().is_err() {
            Some(format!(
                "the payload of '{}' is gone",
                crate::util::term_path(&e.original)
            ))
        } else if !force && dest.symlink_metadata().is_ok() {
            Some(format!("'{}' exists", crate::util::term_path(&dest)))
        } else {
            None
        };
        if let Some(why) = why {
            self.say(format!("would fail: {why}"));
            return Outcome::Failed;
        }
        if dest == e.original {
            self.say(format!(
                "would restore '{}'",
                crate::util::term_path(&e.original)
            ));
        } else {
            self.say(format!(
                "would restore '{}' as '{}'",
                crate::util::term_path(&e.original),
                crate::util::term_path(&dest)
            ));
        }
        Outcome::Restored(dest)
    }
}

/// The first `STEM.N.EXT` (or `NAME.N` without an extension) next to
/// `dest` that does not exist yet.
pub(crate) fn free_name(dest: &Path) -> PathBuf {
    let parent = dest.parent().unwrap_or(Path::new(""));
    let stem = dest.file_stem().unwrap_or_default().to_string_lossy();
    let ext = dest.extension().map(|e| e.to_string_lossy());
    (1u64..)
        .map(|n| {
            let name = match &ext {
                Some(x) => format!("{stem}.{n}.{x}"),
                None => format!("{stem}.{n}"),
            };
            parent.join(name)
        })
        .find(|p| p.symlink_metadata().is_err())
        .unwrap_or_else(|| dest.to_path_buf())
}

pub use crate::util::parse_selection;

/// Shell-pattern match: against the original path when the pattern names a
/// directory part, else against the file name or the trash name.
fn glob_matches_parts(original: &Path, name: &str, pat: &str) -> bool {
    if pat.contains('/') {
        return crate::util::fnmatch(pat, &crate::util::str_from_os_ref(original.as_os_str()));
    }
    crate::util::fnmatch(pat, name)
        || original
            .file_name()
            .is_some_and(|f| crate::util::fnmatch(pat, &crate::util::str_from_os_ref(f)))
}

/// Whether `e` goes by `name`: its original file name or its name under
/// `files/` in the trash.
fn name_matches_parts(original: &Path, trash_name: &str, name: &str) -> bool {
    trash_name == name
        || original
            .file_name()
            .is_some_and(|f| f == crate::util::os_from_str(name))
}

/// `{"restored": [...], "failed": N, "skipped": N}` for `restore --json`.
/// Each item carries `restored_to`, which differs from `original` under
/// `--to` or a rename.
pub(crate) fn restored_json(
    restored: &[(&list::Entry, PathBuf)],
    failed: usize,
    skipped: usize,
) -> String {
    use crate::util::json_escape;
    let items: Vec<String> = restored
        .iter()
        .map(|(e, dest)| {
            format!(
                "{{\"original\":\"{}\",\"name\":\"{}\",\"trash_dir\":\"{}\",\"restored_to\":\"{}\"}}",
                json_escape(&e.original.to_string_lossy()),
                json_escape(&crate::util::show_name(&e.name)),
                json_escape(&e.dir.root.to_string_lossy()),
                json_escape(&dest.to_string_lossy()),
            )
        })
        .collect();
    format!(
        "{{\"restored\":[{}],\"failed\":{failed},\"skipped\":{skipped}}}",
        items.join(",")
    )
}

/// Order entries by deletion time. `list::collect` yields oldest first, with
/// ties broken by original path; newest first reverses that whole order, so
/// equal timestamps keep a stable, path-sorted position in both directions.
pub fn order_entries(entries: &mut [&list::Entry], newest_first: bool) {
    entries.sort_by(|a, b| {
        a.epoch
            .cmp(&b.epoch)
            .then_with(|| a.original.cmp(&b.original))
    });
    if newest_first {
        entries.reverse();
    }
}

/// A typed PATH in the form put records originals: absolute, cleaned, and
/// with symlinks in its parent resolved. `/var/x/a.txt` then matches an
/// entry recorded as `/private/var/x/a.txt` on macOS, or a path typed
/// through any symlinked directory on Linux.
fn absolutize(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        lexical_clean(p)
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        lexical_clean(&cwd.join(p))
    };
    match (abs.parent(), abs.file_name()) {
        (Some(parent), Some(name)) => trashdir::resolve_existing(parent).join(name),
        _ => abs,
    }
}

/// Lexically drop `.` and resolve `..` so user-typed paths match recorded ones.
fn lexical_clean(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// A finished restore: where the item went, and problems that did not stop it.
#[derive(Debug)]
pub(crate) struct Restored {
    pub(crate) dest: PathBuf,
    pub(crate) warnings: Vec<String>,
}

/// Restore one entry to its original path without printing anything, so the
/// TUI can call it while it owns the terminal. Errors come back as text
/// without the program name.
#[cfg(feature = "tui")]
pub(crate) fn restore_one(entry: &list::Entry, force: bool) -> Result<Restored, String> {
    restore_one_to(entry, &entry.original, force)
}

/// A free hidden name in `parent` to stage item `name` under. Whatever
/// already has such a name stays: it may be an item a crashed restore left
/// there, or anyone's file.
fn staging_path(parent: &Path, name: &str) -> PathBuf {
    let pid = std::process::id();
    (0u32..)
        .map(|n| {
            let tag = match n {
                0 => format!(".rtrash-restore-{pid}-{name}"),
                n => format!(".rtrash-restore-{pid}-{n}-{name}"),
            };
            crate::util::join_name(parent, &tag)
        })
        .find(|p| p.symlink_metadata().is_err())
        .unwrap_or_else(|| crate::util::join_name(parent, name))
}

/// After a failed restore, move the payload from `staged` back to `src`,
/// its name in the trash, where its record still points; `err` says what
/// failed, and gains where the payload is if it cannot go back.
fn unstage(err: String, staged: &Path, src: &Path) -> String {
    match trashdir::relocate_back(staged, src) {
        Ok(()) => format!("{err}; the item stays in the trash"),
        Err(e) => format!(
            "{err}; the item is now at '{}', since it could not go back to '{}': {e}",
            staged.display(),
            src.display()
        ),
    }
}

/// Why `dest` must not receive a restored item: it is, contains or sits
/// inside the entry's trash or the home trash. put refuses such paths, so
/// only a forged or corrupt record names one, and restoring there with
/// `-f` would replace the whole trash, or a real item in it.
fn trash_conflict(entry: &list::Entry, dest: &Path) -> Option<String> {
    let abs = match (dest.parent(), dest.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            trashdir::resolve_existing(parent).join(name)
        }
        _ => dest.to_path_buf(),
    };
    let home = trashdir::home_trash();
    for root in [entry.dir.root.as_path(), home.as_path()] {
        let why = match trashdir::trash_overlap(&abs, root) {
            Some(trashdir::Overlap::Contains) => "it contains the trash directory",
            Some(trashdir::Overlap::Inside) => "it is inside the trash directory",
            None => continue,
        };
        return Some(format!(
            "refusing to restore to '{}': {why} '{}'",
            dest.display(),
            root.display()
        ));
    }
    None
}

/// Restore one entry to `dest` instead of its original path, printing
/// nothing.
pub(crate) fn restore_one_to(
    entry: &list::Entry,
    dest: &Path,
    force: bool,
) -> Result<Restored, String> {
    // The home trash records absolute paths only; a relative or empty
    // Path= would restore below whatever directory restore runs in.
    if !dest.is_absolute() {
        return Err(format!(
            "cannot restore '{}': its record names no absolute location ('{}'); `--to=DIR` restores it into DIR",
            crate::util::show_name(&entry.name),
            dest.display()
        ));
    }
    if let Some(err) = trash_conflict(entry, dest) {
        return Err(err);
    }
    let mut warnings = Vec::new();
    let src = crate::util::join_name(&entry.dir.files(), &entry.name);

    // Never destroy the live destination until the trash payload is known good.
    if src.symlink_metadata().is_err() {
        return Err(format!(
            "trash payload missing for '{}' (stale .trashinfo?)",
            crate::util::show_name(&entry.name)
        ));
    }

    let dest_exists = dest.symlink_metadata().is_ok();
    if dest_exists && !force {
        return Err(format!(
            "refusing to overwrite existing '{}' (use -f to force)",
            dest.display()
        ));
    }

    let parent = match dest.parent() {
        Some(p) => p,
        None => {
            return Err(format!("cannot restore to '{}': no parent", dest.display()));
        }
    };
    if let Err(e) = fs::create_dir_all(parent) {
        return Err(format!("cannot create '{}': {e}", parent.display()));
    }

    // What leaving the trash takes off a running total that is current.
    let size = crate::trash_total::cached(&entry.dir)
        .map(|_| trashdir::DirSizes::load(&entry.dir).entry_bytes(&entry.name));

    // Stage into the destination directory first, then swap over any blocker.
    // On failure the live dest (if any) is left intact.
    let staged = staging_path(parent, &entry.name);
    if let Err(e) = trashdir::relocate(&src, &staged) {
        let err = format!("cannot restore '{}' to staging area: {e}", src.display());
        let _ = trashdir::remove_any_path(&staged);
        return Err(err);
    }

    if dest_exists {
        // Payload is durable under `staged`; only now remove the blocker.
        if let Err(e) = trashdir::remove_any_path(dest) {
            let err = format!("cannot remove existing '{}': {e}", dest.display());
            return Err(unstage(err, &staged, &src));
        }
    }

    // A rename in one directory; it still fails on a name the kernel
    // refuses, such as one with a NUL byte from a `%00` in Path=.
    if let Err(e) = fs::rename(&staged, dest) {
        let err = format!("cannot place restored file at '{}': {e}", dest.display());
        return Err(unstage(err, &staged, &src));
    }

    let info_path = crate::util::join_info(&entry.dir.info(), &entry.name);
    // Test seam, debug builds only: store a total here as if another
    // process had measured the trash while this restore ran.
    if cfg!(debug_assertions) {
        if let Some((b, n)) = std::env::var("RTRASH_TEST_TOTAL_MIDWAY")
            .ok()
            .and_then(|v| {
                let (b, n) = v.split_once(':')?;
                Some((b.parse().ok()?, n.parse().ok()?))
            })
        {
            crate::trash_total::store(&entry.dir, b, n);
        }
    }
    {
        // Under the lock, so a put cannot add to the total in between.
        let _lock = trashdir::TrashLock::acquire(&entry.dir.root).ok();
        let total = crate::trash_total::cached(&entry.dir);
        // A total stored by another process while this restore ran was
        // not sized up front; size the item where it now lives, plus its
        // record, as the trash counted them.
        let size = match (size, total) {
            (None, Some(_)) => {
                let info_sz = fs::symlink_metadata(&info_path).map_or(0, |m| {
                    use std::os::unix::fs::MetadataExt;
                    m.blocks().saturating_mul(512)
                });
                Some(crate::fastdelete::disk_usage(dest).saturating_add(info_sz))
            }
            (s, _) => s,
        };
        match fs::remove_file(&info_path) {
            Ok(()) => {
                if let (Some((b, n)), Some(sz), true) = (total, size, _lock.is_some()) {
                    crate::trash_total::store(
                        &entry.dir,
                        b.saturating_sub(sz),
                        n.saturating_sub(1),
                    );
                } else {
                    crate::trash_total::forget(&entry.dir);
                }
            }
            Err(e) => warnings.push(format!("cannot remove '{}': {e}", info_path.display())),
        }
        crate::trash_total::touched(&entry.dir);
    }
    trashdir::directorysizes_remove(&entry.dir, &entry.name);
    Ok(Restored {
        dest: dest.to_path_buf(),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trashdir::TrashDir;

    #[test]
    fn restore_stages_beside_what_has_its_staging_name() {
        let root = std::env::temp_dir().join(format!("rtrash-stage-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let trash = TrashDir {
            root: root.join("Trash"),
            topdir: None,
        };
        trash.ensure().unwrap();
        fs::write(trash.files().join("item"), b"payload").unwrap();
        let out = root.join("out");
        fs::create_dir(&out).unwrap();
        // What a crashed restore by a process with this pid would leave.
        let left = out.join(format!(".rtrash-restore-{}-item", std::process::id()));
        fs::write(&left, b"left behind").unwrap();
        let mut e = entry(out.join("item").to_str().unwrap(), 0);
        e.name = "item".into();
        e.dir = trash;
        restore_one_to(&e, &out.join("item"), false).unwrap();
        assert_eq!(fs::read(out.join("item")).unwrap(), b"payload");
        assert_eq!(fs::read(&left).unwrap(), b"left behind");
        fs::remove_dir_all(&root).unwrap();
    }

    fn entry(path: &str, epoch: i64) -> list::Entry {
        list::Entry {
            date: None,
            epoch,
            original: PathBuf::from(path),
            name: "n".into(),
            dir: TrashDir {
                root: PathBuf::from("/tmp/trash"),
                topdir: None,
            },
            batch: None,
        }
    }

    fn paths(entries: &[&list::Entry]) -> Vec<String> {
        entries
            .iter()
            .map(|e| e.original.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn order_newest_first_puts_latest_on_top() {
        let old = entry("/a/old", 10);
        let mid = entry("/a/mid", 20);
        let new = entry("/a/new", 30);
        let mut v = vec![&mid, &old, &new];
        order_entries(&mut v, true);
        assert_eq!(paths(&v), ["/a/new", "/a/mid", "/a/old"]);
        order_entries(&mut v, false);
        assert_eq!(paths(&v), ["/a/old", "/a/mid", "/a/new"]);
    }

    #[test]
    fn name_matches_file_name_or_trash_name() {
        let mut e = entry("/home/u/report.pdf", 1);
        e.name = "report.2.pdf".into();
        assert!(Matcher::Name(crate::finder_shared::nfc("report.pdf")).matches(&e));
        assert!(Matcher::Name(crate::finder_shared::nfc("report.2.pdf")).matches(&e));
        assert!(!Matcher::Name(crate::finder_shared::nfc("report")).matches(&e));
        assert!(!Matcher::Name(crate::finder_shared::nfc("u/report.pdf")).matches(&e));
    }

    #[test]
    fn restored_json_lists_items() {
        let e = entry("/a/\"q\".txt", 1);
        assert_eq!(
            restored_json(&[(&e, PathBuf::from("/b/q.txt"))], 0, 2),
            r#"{"restored":[{"original":"/a/\"q\".txt","name":"n","trash_dir":"/tmp/trash","restored_to":"/b/q.txt"}],"failed":0,"skipped":2}"#
        );
        assert_eq!(
            restored_json(&[], 1, 0),
            r#"{"restored":[],"failed":1,"skipped":0}"#
        );
    }

    #[test]
    fn selection_takes_indices_ranges_and_all() {
        assert_eq!(parse_selection("3\n", 5), Ok(vec![3]));
        assert_eq!(parse_selection("0-2,4", 5), Ok(vec![0, 1, 2, 4]));
        assert_eq!(parse_selection(" 4 1-2, 2 ", 5), Ok(vec![4, 1, 2]));
        assert_eq!(parse_selection("all", 3), Ok(vec![0, 1, 2]));
        assert!(parse_selection("5", 5).is_err());
        assert!(parse_selection("3-1", 5).is_err());
        assert!(parse_selection("x", 5).is_err());
        assert!(parse_selection(",", 5).is_err());
    }

    #[test]
    fn glob_matches_path_with_a_slash_else_the_name() {
        let e = entry("/home/u/src/main.rs", 1);
        assert!(Matcher::Glob(crate::finder_shared::nfc("*.rs")).matches(&e));
        assert!(Matcher::Glob(crate::finder_shared::nfc("/home/*/src/*")).matches(&e));
        assert!(!Matcher::Glob(crate::finder_shared::nfc("src/*")).matches(&e));
        assert!(!Matcher::Glob(crate::finder_shared::nfc("*.txt")).matches(&e));
    }

    #[test]
    fn matchers_ignore_unicode_normalization() {
        let nfd = entry("/home/u/ne\u{301}e.txt", 1);
        let nfc = entry("/home/u/n\u{e9}e.txt", 1);
        for pat in ["n\u{e9}*", "ne\u{301}*"] {
            let g = Matcher::Glob(crate::finder_shared::nfc(pat));
            assert!(g.matches(&nfd) && g.matches(&nfc), "{pat}");
            let re =
                regex_lite::Regex::new(&crate::finder_shared::nfc(&format!("/{pat}"))).unwrap();
            let r = Matcher::Regex(re);
            assert!(r.matches(&nfd) && r.matches(&nfc), "{pat}");
        }
        let n = Matcher::Name(crate::finder_shared::nfc("ne\u{301}e.txt"));
        assert!(n.matches(&nfc));
    }

    #[test]
    fn free_name_counts_up_before_the_extension() {
        let dir = std::env::temp_dir().join(format!("rtrash-freename-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.txt"), b"").unwrap();
        fs::write(dir.join("a.1.txt"), b"").unwrap();
        assert_eq!(free_name(&dir.join("a.txt")), dir.join("a.2.txt"));
        assert_eq!(free_name(&dir.join(".rc")), dir.join(".rc.1"));
        assert_eq!(free_name(&dir.join("d")), dir.join("d.1"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn order_ties_reverse_as_a_block() {
        let x = entry("/t/x", 5);
        let y = entry("/t/y", 5);
        let mut v = vec![&y, &x];
        order_entries(&mut v, false);
        assert_eq!(paths(&v), ["/t/x", "/t/y"]);
        order_entries(&mut v, true);
        assert_eq!(paths(&v), ["/t/y", "/t/x"]);
    }
}
