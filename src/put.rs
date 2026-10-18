use std::cell::RefCell;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::answer::ask as confirm;
use crate::trashdir::{self, trash_overlap, Overlap};

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Interactive {
    Never,
    Once,
    Always,
}

/// What put does with an item a live process holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum InUse {
    Ignore,
    Warn,
    Refuse,
}

impl InUse {
    fn parse(v: &str) -> Option<InUse> {
        match v {
            "ignore" => Some(InUse::Ignore),
            "warn" => Some(InUse::Warn),
            "refuse" => Some(InUse::Refuse),
            _ => None,
        }
    }

    /// `RTRASH_IN_USE`, else `policy.in_use`, else warn.
    fn from_env(prog: &str) -> InUse {
        if let Ok(v) = std::env::var("RTRASH_IN_USE") {
            if let Some(m) = InUse::parse(&v) {
                return m;
            }
            eprintln!(
                "{prog}: warning: ignoring RTRASH_IN_USE='{v}' (want warn, refuse or ignore)"
            );
        }
        let cfg = crate::config::get();
        if let Some(v) = cfg.str("policy", "in_use") {
            if let Some(m) = InUse::parse(v) {
                return m;
            }
            eprintln!(
                "{prog}: warning: {}",
                cfg.bad_value("policy", "in_use", "warn, refuse or ignore")
            );
        }
        InUse::Warn
    }
}

#[derive(Clone)]
pub(crate) struct Opts {
    pub force: bool,
    pub recursive: bool,
    pub empty_dirs: bool,
    pub verbose: bool,
    pub interactive: Interactive,
    /// `--inspect`: show each item's size (and a directory's file count)
    /// and ask before trashing it, as rip2 does.
    pub inspect: bool,
    pub preserve_root: bool,
    /// Explicit FreeDesktop trash root (`files/` + `info/`); created if needed.
    pub trash_pin: Option<PathBuf>,
    /// Prefer home trash only (skip volume selection). Ignored when `trash_pin` is set.
    pub home_only: bool,
    /// Suppress `-v` lines (stdout carries the `--json` summary instead).
    pub quiet: bool,
    /// Check for live processes holding the item. The browsers leave it off.
    pub in_use: InUse,
    /// `--dry-run`: run every check and say what would move, moving nothing.
    pub dry_run: bool,
}

/// One item moved into a trash directory by `put_one`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trashed {
    /// Absolute original path (as recorded, before the move).
    pub original: PathBuf,
    /// Name under `files/` (and `info/NAME.trashinfo`).
    pub name: String,
    /// Trash root the item went to.
    pub trash_dir: PathBuf,
}

/// JSON summary for `put --json`: every trashed item plus the failure count.
pub fn summary_json(trashed: &[Trashed], failed: usize) -> String {
    let items: Vec<(&Path, &str, &Path)> = trashed
        .iter()
        .map(|t| (t.original.as_path(), t.name.as_str(), t.trash_dir.as_path()))
        .collect();
    crate::util::put_summary_json(&items, failed)
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            force: false,
            recursive: false,
            empty_dirs: false,
            verbose: false,
            interactive: Interactive::Never,
            inspect: false,
            preserve_root: true,
            trash_pin: None,
            home_only: false,
            quiet: false,
            in_use: InUse::Ignore,
            dry_run: false,
        }
    }
}

const HELP: &str = "\
Usage: {prog} [OPTION]... [FILE]...
Move FILE(s) to the freedesktop.org trash. Accepts rm(1) flags.
On a TTY with no FILE operands, opens the interactive put browser. Use --plain
to require CLI operands even on a TTY.

  -f, --force           ignore nonexistent files and arguments, never prompt
  -i                    prompt before every removal
      --inspect         prompt before every removal, showing its size and,
                          for a directory, how many files it holds
  -I                    prompt once before removing more than three files,
                          or when removing recursively
      --interactive[=WHEN]  prompt according to WHEN: never, once (-I), or
                          always (-i); without WHEN, prompt always
  -r, -R, --recursive   remove directories and their contents
  -d, --dir             remove empty directories
      --dry-run         run every check (protected paths, the home and
                          mount-point failsafes, git work, holders) and
                          print 'would trash PATH' for each item that would
                          move, moving nothing
  -v, --verbose         explain what is being done, naming the trash
                          directory each item went to
      --one-file-system accepted for rm compatibility (no-op: entries are
                          moved whole, never traversed)
      --preserve-root[=all]  do not remove '/', your home directory or a
                        mount point (default)
      --no-preserve-root  do not treat those specially
      --trash-dir=PATH  put into this trash root (creates files/ and info/ if
                          needed); scripts/tests pin instead of XDG discovery
      --home-only       always use the home trash (skip volume trash)
      --plain           never open the put browser; require FILE operands
      --no-space-check  copy across devices even when the copy would leave
                          the trash's file system under 5% of its bytes or
                          inodes free, or under RTRASH_MIN_FREE bytes
      --in-use=WHEN     when a live process has an item open, mapped or as
                          its working directory: warn (the default), refuse,
                          or ignore (skips the process scan)
      --json            print a JSON summary on stdout ({trashed, failed});
                          implies --plain and replaces -v output
      --help            display this help and exit
      --version         output version information and exit

Unlike rm, nothing is unlinked: files move to the XDG trash and are
recoverable with `{prog} restore` until `{prog} empty` runs.
";

fn usage_err(prog: &str, msg: &str) -> i32 {
    eprintln!("{prog}: {msg}");
    eprintln!("Try '{prog} --help' for more information.");
    2
}

pub fn run(prog: &str, args: &[String]) -> i32 {
    let snap = crate::finder_total::snapshot();
    let code = run_inner(prog, args);
    crate::trash_total::log_touched("put");
    crate::finder_total::log("put", &snap);
    code
}

fn run_inner(prog: &str, args: &[String]) -> i32 {
    let mut opts = Opts::default();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut no_more_opts = false;
    let mut plain = false;
    let mut json = false;
    let mut in_use: Option<InUse> = None;

    for arg in args {
        if no_more_opts || arg == "-" || !arg.starts_with('-') {
            files.push(crate::util::arg_path(arg));
            continue;
        }
        match arg.as_str() {
            "--" => no_more_opts = true,
            "--plain" => plain = true,
            "--no-space-check" => crate::space::skip_check(true),
            "--json" => {
                json = true;
                opts.quiet = true;
            }
            // GNU rm: -f / -i / --interactive last flag wins for prompt vs force.
            "--force" => {
                opts.force = true;
                opts.interactive = Interactive::Never;
                opts.inspect = false;
            }
            "--recursive" => opts.recursive = true,
            "--dir" => opts.empty_dirs = true,
            "--verbose" => opts.verbose = true,
            "--dry-run" => opts.dry_run = true,
            "--inspect" => {
                opts.force = false;
                opts.inspect = true;
            }
            "--one-file-system" => {}
            // GNU's =all also refuses arguments on another device than
            // their parent; rtrash refuses mount roots under plain
            // --preserve-root already.
            "--preserve-root" | "--preserve-root=all" => opts.preserve_root = true,
            "--no-preserve-root" => opts.preserve_root = false,
            "--home-only" => opts.home_only = true,
            "--interactive" => {
                opts.force = false;
                opts.interactive = Interactive::Always;
            }
            "--interactive=never" | "--interactive=no" | "--interactive=none" => {
                opts.interactive = Interactive::Never;
            }
            "--interactive=once" => {
                opts.force = false;
                opts.interactive = Interactive::Once;
            }
            "--interactive=always" | "--interactive=yes" => {
                opts.force = false;
                opts.interactive = Interactive::Always;
            }
            "--help" => {
                print!("{}", HELP.replace("{prog}", prog));
                return 0;
            }
            "--version" => {
                println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                return 0;
            }
            a if a.starts_with("--in-use=") => match InUse::parse(&a["--in-use=".len()..]) {
                Some(m) => in_use = Some(m),
                None => {
                    return usage_err(
                        prog,
                        &format!(
                            "invalid argument '{}' for '--in-use' (want warn, refuse or ignore)",
                            &a["--in-use=".len()..]
                        ),
                    )
                }
            },
            a if a.starts_with("--trash-dir=") => {
                if opts.trash_pin.is_some() {
                    return usage_err(prog, "put accepts only one --trash-dir=PATH");
                }
                opts.trash_pin = Some(crate::util::arg_path(&a["--trash-dir=".len()..]));
            }
            long if long.starts_with("--") => {
                return usage_err(prog, &format!("unrecognized option '{long}'"));
            }
            short => {
                for c in short[1..].chars() {
                    match c {
                        'f' => {
                            opts.force = true;
                            opts.interactive = Interactive::Never;
                            opts.inspect = false;
                        }
                        'r' | 'R' => opts.recursive = true,
                        'd' => opts.empty_dirs = true,
                        'v' => opts.verbose = true,
                        'i' => {
                            opts.force = false;
                            opts.interactive = Interactive::Always;
                        }
                        'I' => {
                            opts.force = false;
                            opts.interactive = Interactive::Once;
                        }
                        other => {
                            return usage_err(prog, &format!("invalid option -- '{other}'"));
                        }
                    }
                }
            }
        }
    }

    opts.in_use = in_use.unwrap_or_else(|| InUse::from_env(prog));

    if files.is_empty() {
        #[cfg(feature = "tui")]
        if !plain && !json && crate::util::stdin_is_tty() {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            return crate::put_tui::run(prog, &cwd);
        }
        let _ = plain;
        if opts.force {
            if json {
                println!("{}", summary_json(&[], 0));
            }
            return 0;
        }
        return usage_err(prog, "missing operand");
    }

    if opts.interactive == Interactive::Once && (files.len() > 3 || opts.recursive) {
        let noun = if opts.recursive { " recursively" } else { "" };
        if !confirm(&format!(
            "{prog}: remove {} argument{}{noun}? ",
            files.len(),
            if files.len() == 1 { "" } else { "s" },
        )) {
            return 0;
        }
    }

    // RTRASH_ALLOW_ROOTS: every operand in scope, or nothing moves.
    if let Some(code) = crate::allow_roots::refuse_outside(prog, &files) {
        return code;
    }

    let _ = SCAN_ROOTS.set(files.clone());
    // The first Ctrl-C stops after the item in hand and a second quits.
    // Not with prompts: a press at a question would wait unseen.
    if opts.interactive == Interactive::Never {
        crate::interrupt::stop_then_quit();
    }
    let mut status = 0;
    let mut trashed = Vec::new();
    let mut failed = 0usize;
    let mut reached = 0usize;
    for file in &files {
        if crate::interrupt::requested() {
            break;
        }
        reached += 1;
        match put_one(prog, file, &opts) {
            Ok(Some(t)) => trashed.push(t),
            Ok(None) => {}
            Err(code) => {
                failed += 1;
                status = status.max(code);
            }
        }
        crate::interrupt::test_seam("after-item");
    }
    let stopped = crate::interrupt::requested();
    if stopped {
        eprintln!(
            "{prog}: stopped by Ctrl-C: {} trashed, {} not reached",
            trashed.len(),
            files.len() - reached
        );
        status = 130;
    }
    if json {
        println!("{}", summary_json(&trashed, failed));
    }
    if !trashed.is_empty() && crate::finder::is_default(opts.trash_pin.is_some()) {
        let items: Vec<(String, PathBuf, PathBuf)> = trashed
            .iter()
            .map(|t| (t.name.clone(), t.original.clone(), t.trash_dir.clone()))
            .collect();
        crate::finder_shared::log_batch(crate::info::batch_id(), &items);
        let moved: Vec<(PathBuf, String)> = trashed
            .iter()
            .map(|t| (t.trash_dir.clone(), t.name.clone()))
            .collect();
        if !stopped {
            crate::finder::prune_after_put(prog, &moved, opts.quiet || json);
        }
        // A put into a Finder trash frees nothing on that disk either.
        if !(opts.quiet || json) {
            warn_big_finder_trash(prog, &trashed);
            warn_low_space(prog, &trashed);
        }
    }
    // The Finder Trash keeps no .trashinfo; its own branch above warns
    // from the Finder total.
    // A run stopped by Ctrl-C prunes nothing: it was asked to end.
    if !trashed.is_empty() && !stopped && !crate::finder::is_default(opts.trash_pin.is_some()) {
        prune_after_put(prog, &trashed, opts.quiet || json);
        if !(opts.quiet || json) {
            warn_big_trash(prog, &trashed);
            warn_low_space(prog, &trashed);
        }
    }
    status
}

/// With a trash budget (`RTRASH_MAX_SIZE` or `policy.max_size` in the
/// config), bring each trash root this run used back under it, never
/// touching what this run trashed.
fn prune_after_put(prog: &str, trashed: &[Trashed], quiet: bool) {
    let Some((max, policy, source)) = crate::retention::budget(prog) else {
        return;
    };
    let mut roots: Vec<PathBuf> = trashed.iter().map(|t| t.trash_dir.clone()).collect();
    roots.sort();
    roots.dedup();
    let dirs: Vec<trashdir::TrashDir> = roots
        .iter()
        .filter_map(|r| trashdir::TrashDir::from_root(r).ok())
        .collect();
    // One budget per root: each volume's trash frees space on that volume.
    for dir in &dirs {
        // The running total answers most puts without a pass.
        if crate::trash_total::bytes(prog, dir) <= max {
            continue;
        }
        let done = crate::retention::prune(
            prog,
            std::slice::from_ref(dir),
            &crate::retention::Prune {
                max,
                policy,
                dry_run: false,
                verbose: false,
                protect_batch: Some(crate::info::batch_id()),
                gone_before: None,
                free_inodes: 0,
                only: None,
            },
        );
        if done.removed > 0 && !quiet {
            eprintln!(
                "{prog}: removed {} from '{}' to keep it under {} ({source})",
                crate::util::plural(done.removed, "item", "items"),
                dir.root.display(),
                crate::fastdelete::format_bytes(max)
            );
        }
    }
}

/// A trash size to warn at: a size, or a percent of its disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarnSize {
    Bytes(u64),
    Percent(u64),
}

impl WarnSize {
    pub fn parse(v: &str) -> Option<WarnSize> {
        let v = v.trim();
        match v.strip_suffix('%') {
            Some(p) => p
                .trim()
                .parse()
                .ok()
                .filter(|&p| p > 0 && p <= 100)
                .map(WarnSize::Percent),
            None => crate::filters::parse_size(v)
                .filter(|&b| b > 0)
                .map(WarnSize::Bytes),
        }
    }

    /// Whether `bytes` of trash on a disk of `disk` bytes is past it.
    pub fn exceeded(&self, bytes: u64, disk: u64) -> bool {
        match *self {
            WarnSize::Bytes(b) => bytes > b,
            WarnSize::Percent(p) => {
                disk > 0 && u128::from(bytes) * 100 > u128::from(disk) * u128::from(p)
            }
        }
    }
}

/// Share of its disk past which a put warns about a trash by default.
pub const DEFAULT_WARN_PERCENT: u64 = 20;

/// `RTRASH_WARN_SIZE`, else `policy.warn_size`, else 20% of the disk,
/// with where it came from.
fn warn_size(prog: &str) -> Option<(WarnSize, &'static str)> {
    let (raw, src) = match std::env::var("RTRASH_WARN_SIZE") {
        Ok(v) => (v, "RTRASH_WARN_SIZE"),
        Err(_) => match crate::config::get().str("policy", "warn_size") {
            Some(v) => (v.to_string(), "policy.warn_size"),
            None => {
                return Some((
                    WarnSize::Percent(DEFAULT_WARN_PERCENT),
                    "the default 20% of its disk",
                ))
            }
        },
    };
    if raw.trim().is_empty() {
        return None;
    }
    let w = WarnSize::parse(&raw);
    if w.is_none() {
        eprintln!("{prog}: warning: ignoring {src}='{raw}' (want a size such as 20G or a percent such as 10%)");
    }
    w.map(|w| (w, src))
}

/// Warn when a trash this put used holds more than the warn size.
fn warn_big_trash(prog: &str, trashed: &[Trashed]) {
    let Some((limit, src)) = warn_size(prog) else {
        return;
    };
    let mut roots: Vec<PathBuf> = trashed.iter().map(|t| t.trash_dir.clone()).collect();
    roots.sort();
    roots.dedup();
    for root in roots {
        let Ok(dir) = trashdir::TrashDir::from_root(&root) else {
            continue;
        };
        let bytes = crate::trash_total::bytes(prog, &dir);
        let disk = crate::space::query(&root).map_or(0, |s| s.total);
        if limit.exceeded(bytes, disk) {
            let share = if disk > 0 {
                format!(
                    " ({}% of its disk)",
                    u128::from(bytes) * 100 / u128::from(disk)
                )
            } else {
                String::new()
            };
            eprintln!(
                "{prog}: warning: trash '{}' holds {}{share}, past {src}; free it with `rtrash empty`",
                root.display(),
                crate::fastdelete::format_bytes(bytes)
            );
        }
    }
}

/// Warn when a trash this run used sits on a file system under
/// [`crate::space::WARN_FREE_PERCENT`] free, in bytes or inodes: a put
/// there freed nothing, since the payload only moved.
/// The size warning for the Finder trashes a put used: the stored total
/// from before the put plus what this put moved. A trash without a stored
/// total is skipped rather than walked, so a put stays cheap.
fn warn_big_finder_trash(prog: &str, trashed: &[Trashed]) {
    let Some((limit, src)) = warn_size(prog) else {
        return;
    };
    let mut roots: Vec<PathBuf> = trashed.iter().map(|t| t.trash_dir.clone()).collect();
    roots.sort();
    roots.dedup();
    for root in roots {
        let Some((before, _)) = crate::finder_total::stored_total(&root) else {
            continue;
        };
        let added: u64 = trashed
            .iter()
            .filter(|t| t.trash_dir == root)
            .map(|t| crate::fastdelete::disk_usage(&root.join(&t.name)))
            .sum();
        let bytes = before.bytes.saturating_add(added);
        let disk = crate::space::query(&root).map_or(0, |s| s.total);
        if limit.exceeded(bytes, disk) {
            eprintln!(
                "{prog}: warning: trash '{}' holds about {}, past {src}; free it with `rtrash empty --until-free=10%` or `rtrash empty`",
                root.display(),
                crate::fastdelete::format_bytes(bytes)
            );
        }
    }
}

fn warn_low_space(prog: &str, trashed: &[Trashed]) {
    let mut roots: Vec<PathBuf> = trashed.iter().map(|t| t.trash_dir.clone()).collect();
    roots.sort();
    roots.dedup();
    for root in roots {
        if let Some(w) = crate::space::low_warning(&format!("trash '{}'", root.display()), &root) {
            eprintln!("{prog}: warning: {w}");
        }
    }
}

/// The safe-rm protected list, read once per process.
fn protected() -> &'static [String] {
    static LIST: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    LIST.get_or_init(crate::safety::protected_patterns)
}

/// `protect.paths` from the config file, in the same form.
fn config_protected() -> &'static [String] {
    static LIST: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    LIST.get_or_init(|| {
        crate::safety::parse_config(&crate::config::get().list("protect", "paths").join("\n"))
    })
}

/// Files below `dir` (anything that is not a directory), without
/// following symlinks; what cannot be read counts as nothing.
fn count_files(dir: &Path) -> u64 {
    let mut n = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(_) => n += 1,
                Err(_) => {}
            }
        }
    }
    n
}

fn describe(meta: &fs::Metadata, path: &Path) -> &'static str {
    let ft = meta.file_type();
    if ft.is_symlink() {
        "symbolic link"
    } else if ft.is_dir() {
        "directory"
    } else if meta.len() == 0 && ft.is_file() {
        "regular empty file"
    } else if ft.is_file() {
        "regular file"
    } else {
        let _ = path;
        "file"
    }
}

thread_local! {
    /// What [`put_one`] has said on this thread while [`heard`] listens.
    static HEARD: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

/// A message of [`put_one`]: on stderr as always, and kept for [`heard`]
/// when it listens on this thread.
fn say(args: std::fmt::Arguments) {
    let text = args.to_string();
    HEARD.with(|h| {
        if let Some(said) = h.borrow_mut().as_mut() {
            said.push(text.clone());
        }
    });
    eprintln!("{text}");
}

/// `eprintln!` for [`put_one`], through [`say`].
macro_rules! say {
    ($($arg:tt)*) => {
        say(format_args!($($arg)*))
    };
}

/// Run `f` and return, with its result, every message `put_one` wrote
/// on this thread meanwhile. The du explorer shows them in its status
/// line, where stderr cannot reach while it owns the terminal.
pub fn heard<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    let outer = HEARD.with(|h| h.replace(Some(Vec::new())));
    let out = f();
    let said = HEARD.with(|h| h.replace(outer)).unwrap_or_default();
    (out, said)
}

/// The reason among what [`heard`] collected for an item put did not
/// trash: the first line that is no warning or note, without the
/// program name.
pub fn why(prog: &str, said: &[String]) -> String {
    let head = format!("{prog}: ");
    said.iter()
        .map(|l| l.strip_prefix(head.as_str()).unwrap_or(l.as_str()))
        .find(|l| !l.starts_with("warning: ") && !l.starts_with("note: "))
        .map_or_else(|| "not trashed".to_string(), str::to_string)
}

/// Trash one path. `Ok(None)` means nothing moved without an error (missing
/// file under `-f`, or a declined prompt).
pub(crate) fn put_one(prog: &str, path: &Path, opts: &Opts) -> Result<Option<Trashed>, i32> {
    let name = path.as_os_str().to_string_lossy();
    if let Err(e) = crate::allow_roots::check(path) {
        say!("{prog}: cannot trash: {e}");
        return Err(2);
    }

    if opts.preserve_root && path == Path::new("/") {
        say!("{prog}: it is dangerous to operate recursively on '/'");
        say!("{prog}: use --no-preserve-root to override this failsafe");
        return Err(1);
    }
    // As typed: `Path::file_name` reads `d/.` as `d`, which would trash d.
    if crate::safety::is_dot_operand(path) {
        say!("{prog}: refusing to remove '.' or '..' directory: skipping '{name}'");
        return Err(1);
    }

    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if opts.force {
                return Ok(None);
            }
            say!("{prog}: cannot remove '{name}': No such file or directory");
            return Err(1);
        }
        Err(e) => {
            say!("{prog}: cannot remove '{name}': {e}");
            return Err(1);
        }
    };

    // `link/` names the directory the link points to, while the move
    // below takes the link: refuse rather than pick one of the two.
    if meta.is_dir() && crate::safety::typed_last(path).1 {
        let bare: PathBuf = path.components().collect();
        if fs::symlink_metadata(bare).is_ok_and(|m| m.file_type().is_symlink()) {
            say!(
                "{prog}: cannot remove '{name}': it is a symlink to a directory; \
                 give it without the trailing slash to trash the link"
            );
            return Err(1);
        }
    }

    if meta.is_dir() && !opts.recursive {
        if !opts.empty_dirs {
            say!("{prog}: cannot remove '{name}': Is a directory");
            return Err(1);
        }
        match fs::read_dir(path).map(|mut d| d.next().is_some()) {
            Ok(true) => {
                say!("{prog}: cannot remove '{name}': Directory not empty");
                return Err(1);
            }
            Ok(false) => {}
            Err(e) => {
                say!("{prog}: cannot remove '{name}': {e}");
                return Err(1);
            }
        }
    }

    if opts.inspect {
        let kind = describe(&meta, path);
        let size = crate::util::format_bytes(crate::fastdelete::disk_usage(path));
        let what = if meta.is_dir() {
            let n = count_files(path);
            format!("{size}, {n} file{}", if n == 1 { "" } else { "s" })
        } else {
            size
        };
        if !confirm(&format!("{prog}: remove {kind} '{name}' ({what})? ")) {
            return Ok(None);
        }
    } else if opts.interactive == Interactive::Always {
        let kind = describe(&meta, path);
        if !confirm(&format!("{prog}: remove {kind} '{name}'? ")) {
            return Ok(None);
        }
    }

    let abs = match trashdir::abs_nofollow(path) {
        Ok(p) => p,
        Err(e) => {
            say!("{prog}: cannot remove '{name}': {e}");
            return Err(1);
        }
    };
    if let Some(pat) = crate::safety::protected_by(&abs, protected()) {
        say!("{prog}: skipping protected path '{name}' (safe-rm list entry '{pat}')");
        return Err(1);
    }
    if let Some(pat) = crate::safety::protected_by(&abs, config_protected()) {
        say!("{prog}: skipping protected path '{name}' (config protect.paths entry '{pat}')");
        return Err(1);
    }
    if opts.preserve_root && meta.is_dir() {
        if crate::safety::is_home(&abs) {
            say!("{prog}: refusing to remove your home directory '{name}'");
            say!("{prog}: use --no-preserve-root to override this failsafe");
            return Err(1);
        }
        if crate::safety::is_mount_root(&abs, &meta) {
            say!("{prog}: skipping '{name}', since it is a mount point");
            say!("{prog}: use --no-preserve-root to override this failsafe");
            return Err(1);
        }
    }
    if meta.is_dir() {
        if let Some(state) = crate::safety::risky_repo_state(&abs) {
            say!(
                "{prog}: warning: '{name}' contains a git repository with {} work; `rtrash undo` brings it back",
                state.as_str()
            );
        }
    }
    if opts.in_use != InUse::Ignore {
        let holds = holders().under(&abs);
        if !holds.is_empty() {
            let who = crate::holders::describe(&holds);
            if opts.in_use == InUse::Refuse {
                say!("{prog}: cannot trash '{name}': in use by {who}");
                say!("{prog}: use --in-use=warn to trash it anyway");
                return Err(1);
            }
            say!("{prog}: warning: '{name}' is in use by {who}");
        }
    }
    if opts.dry_run {
        println!("would trash '{}'", crate::util::term_path(&abs));
        return Ok(None);
    }
    // WSL, opted in: a file on a Windows drive goes to the Recycle Bin,
    // where Explorer restores it; rtrash list and undo do not see it.
    if opts.trash_pin.is_none() && crate::wsl::applies(&abs) {
        return match crate::wsl::recycle(&abs, meta.is_dir()) {
            Ok(()) => {
                if opts.verbose && !opts.quiet {
                    println!(
                        "removed '{}' (Windows Recycle Bin)",
                        crate::util::term_text(&name)
                    );
                }
                Ok(None)
            }
            Err(e) => {
                say!("{prog}: cannot trash '{name}' to the Windows Recycle Bin: {e}");
                Err(1)
            }
        };
    }
    if opts.trash_pin.is_none() && !opts.quiet && crate::wsl::hint_due(&abs) {
        say!(
            "{prog}: note: '{name}' is on a Windows drive; RTRASH_WSL_RECYCLE=1 sends such files to the Windows Recycle Bin (=0 hides this note)"
        );
    }
    if crate::finder::is_default(opts.trash_pin.is_some()) {
        let size = crate::finder_total::measure_item(&abs);
        return match crate::finder::put_item(&abs, &meta) {
            Ok((trash_name, trash_dir)) => {
                crate::finder_total::carry(&trash_dir, size, true);
                if opts.verbose && !opts.quiet {
                    let to = crate::util::term_path(&trash_dir);
                    println!("removed '{}' (trash {to})", crate::util::term_text(&name));
                }
                Ok(Some(Trashed {
                    original: abs,
                    name: trash_name,
                    trash_dir,
                }))
            }
            Err(e) => {
                say!("{prog}: cannot trash '{name}': {e}");
                Err(1)
            }
        };
    }
    let trash = match resolve_put_trash(opts, &abs, &meta) {
        Ok(t) => t,
        Err(e) => {
            say!("{prog}: cannot trash '{name}': {e}");
            return Err(1);
        }
    };
    // Moving a directory into itself fails with a bare EINVAL, and an
    // item already in the trash would be trashed again, its info lost.
    match trash_overlap(&abs, &trash.root) {
        Some(Overlap::Contains) => {
            say!(
                "{prog}: cannot trash '{name}': it contains the trash directory '{}' it would move into",
                trash.root.display()
            );
            return Err(1);
        }
        Some(Overlap::Inside) => {
            say!(
                "{prog}: cannot trash '{name}': it is inside the trash directory '{}'; `rtrash rm` or `rtrash empty` deletes trashed items",
                trash.root.display()
            );
            return Err(1);
        }
        None => {}
    }
    match trashdir::trash_move(&abs, &meta, &trash) {
        Ok(trash_name) => {
            if opts.verbose && !opts.quiet {
                // Name the trash the item went to: a file on another
                // file system goes to that file system's trash.
                let to = crate::util::term_path(&trash.root);
                if meta.is_dir() {
                    println!(
                        "removed directory '{}' (trash {to})",
                        crate::util::term_text(&name)
                    );
                } else {
                    println!("removed '{}' (trash {to})", crate::util::term_text(&name));
                }
            }
            Ok(Some(Trashed {
                original: abs,
                name: trash_name,
                trash_dir: trash.root,
            }))
        }
        Err(e) => {
            say!("{prog}: cannot trash '{name}': {e}");
            Err(1)
        }
    }
}

/// The live processes and what they hold, read once per run.
/// Only holds on the operands matter, so the scan keeps just those.
fn holders() -> &'static crate::holders::Snapshot {
    static SNAP: std::sync::OnceLock<crate::holders::Snapshot> = std::sync::OnceLock::new();
    SNAP.get_or_init(|| {
        let roots = SCAN_ROOTS.get().map_or(&[][..], Vec::as_slice);
        crate::holders::Snapshot::scan_under(roots, roots.iter().any(|r| mappable(r)))
    })
}

/// Reading every process's maps costs as much as the rest of the scan, so
/// put reads them only for what is commonly mapped without staying open:
/// directories, executables and shared libraries.
fn mappable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Ok(m) = fs::symlink_metadata(path) else {
        return false;
    };
    let name = path
        .file_name()
        .map(|f| f.to_string_lossy())
        .unwrap_or_default();
    m.is_dir()
        || m.permissions().mode() & 0o111 != 0
        || name.contains(".so")
        || name.ends_with(".dylib")
}

/// The put operands, set before the first item so one scan covers them all.
static SCAN_ROOTS: std::sync::OnceLock<Vec<PathBuf>> = std::sync::OnceLock::new();

/// Pick destination trash: explicit pin (created if needed), home-only, or auto-select.
fn resolve_put_trash(
    opts: &Opts,
    abs: &Path,
    meta: &fs::Metadata,
) -> io::Result<trashdir::TrashDir> {
    if let Some(pin) = &opts.trash_pin {
        let td = trashdir::TrashDir::from_root(pin)?;
        // Its Path= would have to be absolute, and restore refuses an
        // absolute Path= outside a volume trash's volume.
        if let Some(top) = td.topdir.as_deref().filter(|t| !abs.starts_with(t)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "the volume trash '{}' keeps only items from '{}'",
                    pin.display(),
                    top.display()
                ),
            ));
        }
        td.ensure()?;
        return Ok(td);
    }
    if opts.home_only {
        let td = trashdir::TrashDir {
            root: trashdir::home_trash(),
            topdir: None,
        };
        trashdir::ensure_home(&td)?;
        return Ok(td);
    }
    trashdir::select(abs, meta.dev())
}

#[cfg(test)]
mod json_tests {
    use super::*;

    #[test]
    fn paths_containing_or_inside_the_trash_are_told_apart() {
        let t = std::env::temp_dir().join(format!("rtrash-overlap-{}", std::process::id()));
        let root = t.join("data/Trash");
        fs::create_dir_all(root.join("files")).unwrap();
        let t = t.canonicalize().unwrap();
        assert_eq!(trash_overlap(&t, &root), Some(Overlap::Contains));
        assert_eq!(
            trash_overlap(&t.join("data/Trash"), &root),
            Some(Overlap::Contains)
        );
        assert_eq!(
            trash_overlap(&t.join("data/Trash/files/x"), &root),
            Some(Overlap::Inside)
        );
        assert_eq!(trash_overlap(&t.join("data/Trashy"), &root), None);
        assert_eq!(trash_overlap(&t.join("other"), &root), None);
        // A trash reached through a symlink is the same trash.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(t.join("data"), t.join("link")).unwrap();
            assert_eq!(
                trash_overlap(&t.join("data"), &t.join("link/Trash")),
                Some(Overlap::Contains)
            );
        }
        let _ = fs::remove_dir_all(&t);
    }

    /// glib #1640: a symlinked `Trash` is the trash under its own name too.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_trash_is_the_trash_by_its_link_name() {
        let t = std::env::temp_dir().join(format!("rtrash-overlap-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&t);
        fs::create_dir_all(t.join("disk/Trash/files")).unwrap();
        fs::create_dir_all(t.join("xdg")).unwrap();
        let t = t.canonicalize().unwrap();
        let link = t.join("xdg/Trash");
        std::os::unix::fs::symlink(t.join("disk/Trash"), &link).unwrap();
        assert_eq!(trash_overlap(&link, &link), Some(Overlap::Contains));
        assert_eq!(
            trash_overlap(&t.join("xdg"), &link),
            Some(Overlap::Contains)
        );
        let _ = fs::remove_dir_all(&t);
    }

    #[test]
    fn summary_json_lists_items_and_failures() {
        let t = Trashed {
            original: PathBuf::from("/w/a \"q\".txt"),
            name: "a \"q\".txt".into(),
            trash_dir: PathBuf::from("/x/Trash"),
        };
        let s = summary_json(&[t], 2);
        assert_eq!(
            s,
            r#"{"trashed":[{"original":"/w/a \"q\".txt","name":"a \"q\".txt","trash_dir":"/x/Trash"}],"failed":2}"#
        );
        assert_eq!(summary_json(&[], 0), r#"{"trashed":[],"failed":0}"#);
    }
}

#[cfg(test)]
mod warn_size_tests {
    use super::WarnSize;

    #[test]
    fn warn_sizes_parse_and_compare() {
        assert_eq!(WarnSize::parse("20G"), Some(WarnSize::Bytes(20 << 30)));
        assert_eq!(WarnSize::parse("10%"), Some(WarnSize::Percent(10)));
        for bad in ["", "0", "0%", "101%", "lots"] {
            assert_eq!(WarnSize::parse(bad), None, "{bad}");
        }
        assert!(WarnSize::Bytes(10).exceeded(11, 0));
        assert!(!WarnSize::Bytes(10).exceeded(10, 0));
        assert!(WarnSize::Percent(10).exceeded(11, 100));
        assert!(!WarnSize::Percent(10).exceeded(10, 100));
        assert!(!WarnSize::Percent(10).exceeded(10, 0));
    }
}
