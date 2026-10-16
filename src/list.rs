use std::fs;
use std::path::PathBuf;

use crate::info;
use crate::trashdir::{self, TrashDir};

const HELP: &str = "\
Usage: {prog} [OPTION]... [PATTERN]
List trashed items as 'DELETION-DATE ORIGINAL-PATH', oldest first.
PATTERN keeps items whose file name holds it, or matches it when it has
a glob character (* ? [), like --glob.
Payloads with no .trashinfo come last, dated ????-??-??, under their path
in the trash (\"orphan\":true in JSON); an age filter leaves them out.

      --home-only       only the home trash ($XDG_DATA_HOME/Trash)
      --trash-dir=PATH  list only this trash directory (repeatable);
                     default is the home trash plus every mounted volume
      --all-users       list every user's trash this account can read
                     (root reads all) and add an OWNER column; read-only
                     (as trash-list, no OWNER column, as trash-cli)
      --volumes         print the mount points searched for trash and exit
      --trash-dirs      print the trash directories read and exit
      --older-than=AGE  only items trashed more than AGE ago; AGE is DAYS
                     or a duration such as 2h, 30m or 1d12h
      --newer-than=AGE  only items trashed within the last AGE
      --larger-than=SIZE  only items taking more than SIZE on disk
      --smaller-than=SIZE only items taking less than SIZE on disk
                     (SIZE as in `empty --max-size`: 500M, 1.5G, 64k;
                     sizes as --size shows them; orphans included)
      --glob=PATTERN    only items whose original path (PATTERN with a /)
                     or file name matches a shell glob
      --regex=RE        only items whose original path matches RE
      --name=NAME       only items whose file name is NAME
      --cwd-only        only items that came from the current directory
                     or below it
      --directory=DIR   only items that came from DIR or below it (DIR
                     need not exist any more)
      --size            add each item's disk usage (payload plus its
                     .trashinfo, as status counts it)
      --show-trashpath  under each row, show where the item sits in the
                     trash (\"trash_path\" in --json; not with --format),
                     as gtrash find --show-trashpath
      --sort=KEY        order by date (default), size, name (file name)
                     or path (original path); size implies --size
      --reverse         reverse the order
      --depth=N         under each trashed directory, show what it holds,
                     N levels down, indented (plain rows only)
      --last=N          only the last N rows of the listing (after --sort and
                     --reverse), like tail -n: the N newest by default
      --json            emit a JSON array (scripts; one object per item;
                     \"size\" in bytes with --size; \"missing\" is true
                     when the payload is gone); field names are stable,
                     and new fields only get added
      --format=TEMPLATE print each item through TEMPLATE, unescaped:
                     {date} {epoch} {path} {name} {trash} {batch}
                     {size} (bytes) {owner}; {{ and }} for braces,
                     \\t \\n \\0 for tab, newline, NUL; no newline added
      --help            display this help and exit
      --version         output version information and exit
";

pub struct Entry {
    pub date: Option<String>,
    pub epoch: i64,
    pub original: std::path::PathBuf,
    pub name: String,
    pub dir: TrashDir,
    /// Put batch id from the `.trashinfo`, when rtrash wrote one.
    pub batch: Option<String>,
}

impl Entry {
    /// True when the record's payload under `files/` is gone, so the item
    /// cannot be restored (`rtrash empty --dangling` removes such records).
    pub fn missing(&self) -> bool {
        fs::symlink_metadata(crate::util::join_name(&self.dir.files(), &self.name)).is_err()
    }
}

pub fn collect(dirs: &[TrashDir]) -> Vec<Entry> {
    use rayon::prelude::*;
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir.info()) else {
            continue;
        };
        let paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        // One read per item: spread a big trash over the rayon pool.
        out.par_extend(paths.into_par_iter().filter_map(|path| {
            let fname = crate::util::str_from_os_ref(path.file_name()?);
            let name = crate::util::trash_item_name(&fname)?;
            let content = info::read_bounded(&path, info::RECORD_CAP).ok()?;
            let parsed = info::parse_bytes(&content);
            let epoch = parsed
                .deletion_date
                .as_deref()
                .and_then(info::parse_local_to_epoch)
                .unwrap_or(0);
            // Skip hostile/corrupt relative Path= that escape the volume topdir.
            let original = dir.resolve_original_checked(&parsed.path).ok()?;
            Some(Entry {
                original,
                date: parsed.deletion_date,
                epoch,
                name: name.to_string(),
                dir: dir.clone(),
                batch: parsed.batch,
            })
        }));
    }
    out.sort_by(|a, b| {
        a.epoch
            .cmp(&b.epoch)
            .then_with(|| a.original.cmp(&b.original))
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

/// Payloads in `files/` with no `.trashinfo` (left by a crash or another
/// tool), as entries with no date whose original path is the payload
/// itself, oldest-first order kept by name.
pub fn orphans(dirs: &[TrashDir]) -> Vec<Entry> {
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(rd) = fs::read_dir(dir.files()) else {
            continue;
        };
        let info_dir = dir.info();
        let mut found: Vec<Entry> = rd
            .flatten()
            .filter_map(|ent| {
                let name = crate::util::str_from_os_ref(&ent.file_name());
                let info = crate::util::join_info(&info_dir, &name);
                // symlink_metadata: a dangling .trashinfo symlink still counts.
                if fs::symlink_metadata(&info).is_ok() {
                    return None;
                }
                Some(Entry {
                    date: None,
                    epoch: 0,
                    original: ent.path(),
                    name,
                    dir: dir.clone(),
                    batch: None,
                })
            })
            .collect();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        out.extend(found);
    }
    out
}

/// When `entry` was trashed, from its `.trashinfo` date, if readable.
fn entry_epoch(entry: &Entry) -> Option<i64> {
    entry.date.as_deref().and_then(info::parse_local_to_epoch)
}

/// Keep entries trashed at least `older` seconds ago and less than `newer`
/// seconds ago (both when both are set). A missing or unreadable date
/// never passes an age filter.
pub fn filter_age_secs(entries: Vec<Entry>, older: Option<i64>, newer: Option<i64>) -> Vec<Entry> {
    let age = crate::filters::AgeFilter {
        older_than: older,
        newer_than: newer,
    };
    let now = info::now_epoch();
    entries
        .into_iter()
        .filter(|e| age.keeps_at(entry_epoch(e), now))
        .collect()
}

pub use crate::filters::SizeFilter;

pub use crate::listfmt::{parse_format, render_format, Field, FmtPart, FmtRow};
pub use crate::util::json_escape;

pub fn entry_to_json(e: &Entry) -> String {
    entry_to_json_sized(e, None)
}

/// [`entry_to_json`] with a `size` field when `size` is known.
pub fn entry_to_json_sized(e: &Entry, size: Option<u64>) -> String {
    let size = size.map(|n| format!(",\"size\":{n}")).unwrap_or_default();
    let date = e.date.as_deref().unwrap_or("");
    let batch = match &e.batch {
        Some(b) => format!("\"{}\"", json_escape(b)),
        None => "null".to_string(),
    };
    format!(
        "{{\"deletion_date\":\"{}\",\"original\":\"{}\",\"name\":\"{}\",\"trash_dir\":\"{}\",\"batch\":{},\"missing\":{}{}}}",
        json_escape(date),
        json_escape(&e.original.to_string_lossy()),
        json_escape(&crate::util::show_name(&e.name)),
        json_escape(&e.dir.root.to_string_lossy()),
        batch,
        e.missing(),
        size,
    )
}

pub fn entries_to_json_array(entries: &[Entry]) -> String {
    let refs: Vec<&Entry> = entries.iter().collect();
    sized_json_array(&refs, None)
}

/// JSON array of `entries`, with `sizes[i]` as entry `i`'s size when given.
fn sized_json_array(entries: &[&Entry], sizes: Option<&[u64]>) -> String {
    let mut out = String::from("[\n");
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            out.push_str(",\n");
        }
        out.push_str("  ");
        out.push_str(&entry_to_json_sized(e, sizes.map(|s| s[i])));
    }
    if !entries.is_empty() {
        out.push('\n');
    }
    out.push(']');
    out
}

pub fn run(prog: &str, args: &[String]) -> i32 {
    let mut trash_dirs: Vec<PathBuf> = Vec::new();
    let mut home_only = false;
    let mut all_users = false;
    let mut json = false;
    let mut volumes = false;
    let mut show_dirs = false;
    let mut older_than: Option<i64> = None;
    let mut newer_than: Option<i64> = None;
    // Defaults from the config file; options override them.
    let cfg = crate::config::get();
    let mut with_size = cfg.bool("list", "size").unwrap_or(false);
    let mut show_trash = false;
    let mut sort = SortKey::Date;
    if let Some(s) = cfg.str("list", "sort") {
        match SortKey::parse(s) {
            Some(k) => sort = k,
            None => eprintln!(
                "{prog}: warning: {}",
                cfg.bad_value("list", "sort", "date, size, name or path")
            ),
        }
    }
    let mut reverse = cfg.bool("list", "reverse").unwrap_or(false);
    let mut size_filter = SizeFilter::default();
    let mut matcher: Option<crate::restore::Matcher> = None;
    let mut cwd_only = false;
    let mut directory: Option<PathBuf> = None;
    let mut last: Option<usize> = None;
    let mut depth = 0usize;
    let mut format: Option<Vec<FmtPart>> = None;
    let mut i = 0usize;
    while i < args.len() {
        match size_filter.take(args, &mut i) {
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
            "--home-only" => home_only = true,
            "--all-users" => all_users = true,
            "--json" => json = true,
            a if a.starts_with("--format=") => match parse_format(&a["--format=".len()..]) {
                Ok(f) => format = Some(f),
                Err(e) => {
                    eprintln!("{prog}: {e}");
                    return 2;
                }
            },
            "--volumes" => volumes = true,
            "--trash-dirs" => show_dirs = true,
            "--size" => with_size = true,
            "--show-trashpath" => show_trash = true,
            "--reverse" => reverse = true,
            "--cwd-only" => cwd_only = true,
            a if a.starts_with("--directory=") => {
                let d = &a["--directory=".len()..];
                if d.is_empty() {
                    eprintln!("{prog}: --directory needs a non-empty DIR");
                    return 2;
                }
                let d = crate::util::arg_path(d);
                directory = Some(std::path::absolute(&d).unwrap_or(d));
            }
            a if a.starts_with("--depth=") => match a["--depth=".len()..].parse::<usize>() {
                Ok(n) => depth = n,
                Err(_) => {
                    eprintln!(
                        "{prog}: --depth needs a count of levels, not '{}'",
                        &a["--depth=".len()..]
                    );
                    return 2;
                }
            },
            a if a.starts_with("--last=") => match a["--last=".len()..].parse::<usize>() {
                Ok(n) => last = Some(n),
                Err(_) => {
                    eprintln!(
                        "{prog}: --last needs a count of rows, not '{}'",
                        &a["--last=".len()..]
                    );
                    return 2;
                }
            },
            a if a.starts_with("--glob=")
                || a.starts_with("--regex=")
                || a.starts_with("--name=") =>
            {
                if matcher.is_some() {
                    eprintln!("{prog}: give only one of --glob, --regex and --name");
                    return 2;
                }
                let (key, val) = a.split_once('=').unwrap_or((a, ""));
                if val.is_empty() {
                    eprintln!("{prog}: {key} needs a non-empty value");
                    return 2;
                }
                let val = crate::finder_shared::nfc(val);
                matcher = Some(match key {
                    "--glob" => crate::restore::Matcher::Glob(val),
                    "--name" => crate::restore::Matcher::Name(val),
                    _ => match regex_lite::Regex::new(&val) {
                        Ok(re) => crate::restore::Matcher::Regex(re),
                        Err(e) => {
                            eprintln!("{prog}: invalid --regex '{val}': {e}");
                            return 2;
                        }
                    },
                });
            }
            a if a.starts_with("--sort=") => match SortKey::parse(&a["--sort=".len()..]) {
                Some(k) => sort = k,
                None => {
                    eprintln!(
                        "{prog}: invalid --sort '{}' (date, size, name or path)",
                        &a["--sort=".len()..]
                    );
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
            a if a.starts_with("--trash-dir=") => {
                trash_dirs.push(crate::util::arg_path(&a["--trash-dir=".len()..]));
            }
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
            a if !a.starts_with('-') => {
                // A bare PATTERN: a glob if it has one, else a piece of the
                // file name, as `gtrash find` and `fd` take it.
                if matcher.is_some() {
                    eprintln!("{prog}: give only one of PATTERN, --glob, --regex and --name");
                    return 2;
                }
                let p = crate::finder_shared::nfc(a);
                let p = if p.contains(['*', '?', '[']) {
                    p
                } else {
                    format!("*{p}*")
                };
                matcher = Some(crate::restore::Matcher::Glob(p));
            }
            a => {
                eprintln!("{prog}: unrecognized option '{a}'");
                eprintln!("Try '{prog} --help' for more information.");
                return 2;
            }
        }
        i += 1;
    }
    if let Err(e) = size_filter.check() {
        eprintln!("{prog}: {e}");
        return 2;
    }
    if cwd_only && directory.is_some() {
        eprintln!("{prog}: give --cwd-only or --directory, not both");
        return 2;
    }
    let cwd = if let Some(d) = directory {
        Some(d)
    } else if cwd_only {
        match std::env::current_dir() {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("{prog}: cannot read the current directory: {e}");
                return 2;
            }
        }
    } else {
        None
    };
    if json && format.is_some() {
        eprintln!("{prog}: give --json or --format, not both");
        return 2;
    }
    if show_trash && format.is_some() {
        eprintln!("{prog}: --show-trashpath works on plain rows and --json, not --format");
        return 2;
    }
    if depth > 0 && (json || format.is_some()) {
        eprintln!("{prog}: --depth works on plain rows, not --json or --format");
        return 2;
    }
    // --glob/--regex/--name and --cwd-only, on the original path and the
    // name in the trash.
    let keep = |original: &std::path::Path, name: &str| -> bool {
        matcher
            .as_ref()
            .is_none_or(|m| m.matches_parts(original, name))
            && cwd.as_ref().is_none_or(|d| {
                original.starts_with(d)
                    || std::fs::canonicalize(d).is_ok_and(|r| original.starts_with(r))
            })
    };
    // macOS lists the Finder Trash unless a pin or the setting says otherwise.
    if crate::finder::is_default(!trash_dirs.is_empty() || all_users) {
        let refused = if volumes || show_dirs {
            Some("--volumes or --trash-dirs")
        } else {
            None
        };
        let filter = crate::finder_shared::Filter {
            age: crate::filters::AgeFilter {
                older_than,
                newer_than,
            },
            size: size_filter,
        };
        let shape = crate::finder::ListShape {
            json,
            format: format.as_deref(),
            last,
            depth,
            show_trash,
        };
        return crate::finder::default_list(prog, shape, refused, filter, &keep);
    }
    if all_users && (home_only || !trash_dirs.is_empty()) {
        eprintln!("{prog}: --all-users takes no --home-only or --trash-dir");
        return 2;
    }
    let dirs = if all_users {
        trashdir::all_users()
    } else {
        trashdir::resolve_dirs_opts(&trash_dirs, home_only)
    };
    if dirs.is_empty() && !trash_dirs.is_empty() {
        eprintln!("{prog}: no valid --trash-dir pins");
        return 2;
    }
    // trash-cli's --volumes and --trash-dirs print the places looked at.
    if volumes || show_dirs {
        if volumes {
            for m in trashdir::mount_points() {
                println!("{}", m.display());
            }
        }
        if show_dirs {
            for d in &dirs {
                println!("{}", d.root.display());
            }
        }
        return 0;
    }
    let mut entries = filter_age_secs(collect(&dirs), older_than, newer_than);
    entries.retain(|e| keep(&e.original, &e.name));
    // Orphans have no date, so only an unfiltered listing shows them; they
    // come last, after every recorded item.
    let mut recorded = entries.len();
    if older_than.is_none() && newer_than.is_none() {
        entries.extend(
            orphans(&dirs)
                .into_iter()
                .filter(|e| keep(&e.original, &e.name)),
        );
    }
    let with_size = with_size
        || sort == SortKey::Size
        || format
            .as_ref()
            .is_some_and(|f| f.iter().any(|p| matches!(p, FmtPart::Field(Field::Size))));
    let mut sizes = (with_size || size_filter.is_set()).then(|| entry_sizes(&dirs, &entries));
    if let (true, Some(all)) = (size_filter.is_set(), sizes.as_mut()) {
        let keep: Vec<bool> = all.iter().map(|&n| size_filter.keeps(n)).collect();
        recorded = keep[..recorded].iter().filter(|&&k| k).count();
        let mut k = keep.iter();
        entries.retain(|_| *k.next().unwrap_or(&false));
        let mut k = keep.iter();
        all.retain(|_| *k.next().unwrap_or(&false));
    }
    let sizes = sizes.filter(|_| with_size);
    let mut order = sorted_order(&entries, sizes.as_deref(), sort, reverse);
    if sort == SortKey::Date {
        // Keep orphans after the recorded items whichever way dates run.
        order.sort_by_key(|&i| i >= recorded);
    }
    // --last keeps the tail of the listing, as tail -n would.
    if let Some(n) = last {
        let cut = order.len().saturating_sub(n);
        order.drain(..cut);
    }
    let orphan_rows: Vec<bool> = order.iter().map(|&i| i >= recorded).collect();
    let rows: Vec<&Entry> = order.iter().map(|&i| &entries[i]).collect();
    let row_sizes: Option<Vec<u64>> = sizes.map(|s| order.iter().map(|&i| s[i]).collect());
    // Owner of each row's trash root, looked up once per root.
    // The trash-list name keeps trash-cli's plain rows, without OWNER.
    let owners: Option<Vec<String>> = (all_users && (json || prog != "trash-list")).then(|| {
        let mut by_root: std::collections::HashMap<&std::path::Path, String> =
            std::collections::HashMap::new();
        rows.iter()
            .map(|e| {
                by_root
                    .entry(e.dir.root.as_path())
                    .or_insert_with(|| trashdir::owner_name(&e.dir.root))
                    .clone()
            })
            .collect()
    });
    if let Some(parts) = &format {
        use std::io::Write;
        let mut out = std::io::BufWriter::new(std::io::stdout().lock());
        for (k, e) in rows.iter().enumerate() {
            let row = FmtRow {
                date: e.date.as_deref().unwrap_or(""),
                epoch: e.epoch,
                path: &e.original,
                name: &e.name,
                trash: &e.dir.root,
                batch: e.batch.as_deref().unwrap_or(""),
                size: row_sizes.as_ref().map(|s| s[k]),
                owner: owners.as_ref().map(|o| o[k].as_str()),
            };
            if out.write_all(&render_format(parts, &row)).is_err() {
                return 0;
            }
        }
        let _ = out.flush();
    } else if json {
        let items: Vec<String> = rows
            .iter()
            .enumerate()
            .map(|(k, e)| {
                let obj = entry_to_json_sized(e, row_sizes.as_ref().map(|s| s[k]));
                let mut body = obj.strip_suffix('}').unwrap_or(&obj).to_string();
                if orphan_rows[k] {
                    body.push_str(",\"orphan\":true");
                }
                if let Some(owners) = &owners {
                    body.push_str(&format!(",\"owner\":\"{}\"", json_escape(&owners[k])));
                }
                if show_trash {
                    let payload = crate::util::join_name(&e.dir.files(), &e.name);
                    let payload = json_escape(&payload.to_string_lossy());
                    body.push_str(&format!(",\"trash_path\":\"{payload}\""));
                }
                format!("  {body}}}")
            })
            .collect();
        if items.is_empty() {
            println!("[\n]");
        } else {
            println!("[\n{}\n]", items.join(",\n"));
        }
    } else {
        use std::io::Write;
        let mut out = std::io::BufWriter::new(std::io::stdout().lock());
        for (k, entry) in rows.iter().enumerate() {
            let date = entry.date.as_deref().unwrap_or("????-??-??T??:??:??");
            let mut date = crate::util::term_text(date).replacen('T', " ", 1);
            if let Some(o) = &owners {
                date = format!("{date} {:<8}", crate::util::term_text(&o[k]));
            }
            let res = match &row_sizes {
                Some(s) => writeln!(
                    out,
                    "{date} {:>10} {}",
                    crate::util::format_bytes(s[k]),
                    crate::util::term_path(&entry.original)
                ),
                None => writeln!(out, "{date} {}", crate::util::term_path(&entry.original)),
            };
            let payload = crate::util::join_name(&entry.dir.files(), &entry.name);
            let res = res.and_then(|()| crate::listfmt::trash_line(&mut out, &payload, show_trash));
            let res = res.and_then(|()| write_tree(&mut out, &payload, depth));
            if res.is_err() {
                return 0;
            }
        }
        let _ = out.flush();
    }
    0
}

/// `--depth=N`: what a trashed directory holds, N levels down, one entry
/// per line indented by level, directories with a trailing separator and
/// names in order. Symlinks are not followed; what cannot be read is left
/// out. A plain file, or `depth` 0, prints nothing.
pub(crate) fn write_tree(
    out: &mut impl std::io::Write,
    dir: &std::path::Path,
    depth: usize,
) -> std::io::Result<()> {
    fn walk(
        out: &mut impl std::io::Write,
        dir: &std::path::Path,
        level: usize,
        depth: usize,
    ) -> std::io::Result<()> {
        let Ok(rd) = fs::read_dir(dir) else {
            return Ok(());
        };
        let mut kids: Vec<(String, std::path::PathBuf, bool)> = rd
            .flatten()
            .map(|e| {
                let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
                (
                    e.file_name().to_string_lossy().into_owned(),
                    e.path(),
                    is_dir,
                )
            })
            .collect();
        kids.sort();
        for (name, path, is_dir) in kids {
            let slash = if is_dir { "/" } else { "" };
            let indent = "  ".repeat(level);
            writeln!(out, "    {indent}{}{slash}", crate::util::term_text(&name))?;
            if is_dir && level + 1 < depth {
                walk(out, &path, level + 1, depth)?;
            }
        }
        Ok(())
    }
    let is_dir = fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir());
    if depth == 0 || !is_dir {
        return Ok(());
    }
    walk(out, dir, 0, depth)
}

/// What `list --sort` orders by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Date,
    Size,
    Name,
    Path,
}

impl SortKey {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "date" | "time" => SortKey::Date,
            "size" => SortKey::Size,
            "name" => SortKey::Name,
            "path" => SortKey::Path,
            _ => return None,
        })
    }
}

/// Disk usage of each entry, payload plus `.trashinfo`, through each root's
/// size cache; walked directories go back into the cache.
fn entry_sizes(dirs: &[TrashDir], entries: &[Entry]) -> Vec<u64> {
    use rayon::prelude::*;
    let caches: Vec<trashdir::DirSizes> = dirs.iter().map(trashdir::DirSizes::load).collect();
    let sizes = entries
        .par_iter()
        .map(|e| {
            caches
                .iter()
                .zip(dirs)
                .find(|(_, d)| d.root == e.dir.root)
                .map_or(0, |(c, _)| c.entry_bytes(&e.name))
        })
        .collect();
    for c in caches {
        c.write_back();
    }
    sizes
}

/// Row order for `entries` (already oldest first) under `key`. Ties keep
/// the date order, so equal sizes or names stay oldest first.
pub fn sorted_order(
    entries: &[Entry],
    sizes: Option<&[u64]>,
    key: SortKey,
    reverse: bool,
) -> Vec<usize> {
    let mut order: Vec<usize> = (0..entries.len()).collect();
    let file_name = |i: usize| entries[i].original.file_name().unwrap_or_default();
    match key {
        SortKey::Date => {}
        SortKey::Size => {
            let sizes = sizes.unwrap_or(&[]);
            order.sort_by_key(|&i| std::cmp::Reverse(sizes.get(i).copied().unwrap_or(0)));
        }
        SortKey::Name => order.sort_by(|&a, &b| file_name(a).cmp(file_name(b))),
        SortKey::Path => order.sort_by(|&a, &b| entries[a].original.cmp(&entries[b].original)),
    }
    if reverse {
        order.reverse();
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trashdir::TrashDir;
    use std::path::PathBuf;

    fn entry_with(date: Option<&str>, epoch: i64) -> Entry {
        Entry {
            date: date.map(str::to_string),
            epoch,
            original: PathBuf::from("/tmp/x"),
            name: "x".into(),
            dir: TrashDir {
                root: PathBuf::from("/tmp/Trash"),
                topdir: None,
            },
            batch: None,
        }
    }

    #[test]
    fn json_escape_quotes_and_controls() {
        assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
        assert!(json_escape("ok").contains("ok"));
    }

    #[test]
    fn age_filters_leave_out_a_missing_date() {
        let e = || entry_with(None, 0);
        assert!(filter_age_secs(vec![e()], Some(0), None).is_empty());
        assert!(filter_age_secs(vec![e()], None, Some(365 * 86_400)).is_empty());
    }

    #[test]
    fn age_newer_recent() {
        let now = info::now_epoch();
        let e = || entry_with(Some(&info::now_local_string()), now);
        assert_eq!(filter_age_secs(vec![e()], None, Some(3600)).len(), 1);
        assert!(filter_age_secs(vec![e()], Some(3600), None).is_empty());
    }

    #[test]
    fn filter_age_and() {
        let now = info::now_epoch();
        let recent = entry_with(Some(&info::now_local_string()), now);
        let old = entry_with(Some("2001-01-01T00:00:00"), 978_307_200);
        let out = filter_age_secs(vec![recent, old], Some(30 * 86_400), None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].epoch, 978_307_200);
    }

    #[test]
    fn sort_keys_order_rows_and_reverse() {
        let mk = |path: &str, epoch: i64| Entry {
            original: PathBuf::from(path),
            ..entry_with(None, epoch)
        };
        let es = vec![mk("/b/zeta", 1), mk("/a/beta", 2), mk("/c/alpha", 3)];
        let sizes = [10, 30, 20];
        assert_eq!(sorted_order(&es, None, SortKey::Date, false), [0, 1, 2]);
        assert_eq!(
            sorted_order(&es, Some(&sizes), SortKey::Size, false),
            [1, 2, 0]
        );
        assert_eq!(sorted_order(&es, None, SortKey::Name, false), [2, 1, 0]);
        assert_eq!(sorted_order(&es, None, SortKey::Path, true), [2, 0, 1]);
        let s = entry_to_json_sized(&es[0], Some(42));
        assert!(s.ends_with(",\"size\":42}"), "{s}");
    }

    #[test]
    fn entries_json_array_shape() {
        let e = entry_with(Some("2020-01-02T03:04:05"), 1);
        let s = entries_to_json_array(std::slice::from_ref(&e));
        assert!(s.starts_with('['));
        assert!(s.contains("\"deletion_date\":\"2020-01-02T03:04:05\""));
        assert!(s.contains("\"original\":\"/tmp/x\""));
        assert!(s.ends_with(']'));
    }
}
