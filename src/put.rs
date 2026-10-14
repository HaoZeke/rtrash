use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::trashdir::{self, trash_overlap, Overlap};
use crate::util::confirm;

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
    pub preserve_root: bool,
    /// Explicit FreeDesktop trash root (`files/` + `info/`); created if needed.
    pub trash_pin: Option<PathBuf>,
    /// Prefer home trash only (skip volume selection). Ignored when `trash_pin` is set.
    pub home_only: bool,
    /// Suppress `-v` lines (stdout carries the `--json` summary instead).
    pub quiet: bool,
    /// Check for live processes holding the item. The browsers leave it off.
    pub in_use: InUse,
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
            preserve_root: true,
            trash_pin: None,
            home_only: false,
            quiet: false,
            in_use: InUse::Ignore,
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
  -I                    prompt once before removing more than three files,
                          or when removing recursively
      --interactive[=WHEN]  prompt according to WHEN: never, once (-I), or
                          always (-i); without WHEN, prompt always
  -r, -R, --recursive   remove directories and their contents
  -d, --dir             remove empty directories
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
            }
            "--recursive" => opts.recursive = true,
            "--dir" => opts.empty_dirs = true,
            "--verbose" => opts.verbose = true,
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

    let _ = SCAN_ROOTS.set(files.clone());
    let mut status = 0;
    let mut trashed = Vec::new();
    let mut failed = 0usize;
    for file in &files {
        match put_one(prog, file, &opts) {
            Ok(Some(t)) => trashed.push(t),
            Ok(None) => {}
            Err(code) => {
                failed += 1;
                status = status.max(code);
            }
        }
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
    }
    // The Finder Trash keeps no .trashinfo, so budgets and warnings that
    // read the FreeDesktop layout skip it.
    if !trashed.is_empty() && !crate::finder::is_default(opts.trash_pin.is_some()) {
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

/// Trash one path. `Ok(None)` means nothing moved without an error (missing
/// file under `-f`, or a declined prompt).
pub(crate) fn put_one(prog: &str, path: &Path, opts: &Opts) -> Result<Option<Trashed>, i32> {
    let name = path.as_os_str().to_string_lossy();

    if opts.preserve_root && path == Path::new("/") {
        eprintln!("{prog}: it is dangerous to operate recursively on '/'");
        eprintln!("{prog}: use --no-preserve-root to override this failsafe");
        return Err(1);
    }
    let base = path.file_name().map(|f| f.to_string_lossy().into_owned());
    if matches!(base.as_deref(), Some(".") | Some("..")) || name == "." || name == ".." {
        eprintln!("{prog}: refusing to remove '.' or '..' directory: skipping '{name}'");
        return Err(1);
    }

    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if opts.force {
                return Ok(None);
            }
            eprintln!("{prog}: cannot remove '{name}': No such file or directory");
            return Err(1);
        }
        Err(e) => {
            eprintln!("{prog}: cannot remove '{name}': {e}");
            return Err(1);
        }
    };

    if meta.is_dir() && !opts.recursive {
        if !opts.empty_dirs {
            eprintln!("{prog}: cannot remove '{name}': Is a directory");
            return Err(1);
        }
        match fs::read_dir(path).map(|mut d| d.next().is_some()) {
            Ok(true) => {
                eprintln!("{prog}: cannot remove '{name}': Directory not empty");
                return Err(1);
            }
            Ok(false) => {}
            Err(e) => {
                eprintln!("{prog}: cannot remove '{name}': {e}");
                return Err(1);
            }
        }
    }

    if opts.interactive == Interactive::Always {
        let kind = describe(&meta, path);
        if !confirm(&format!("{prog}: remove {kind} '{name}'? ")) {
            return Ok(None);
        }
    }

    let abs = match trashdir::abs_nofollow(path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{prog}: cannot remove '{name}': {e}");
            return Err(1);
        }
    };
    if let Some(pat) = crate::safety::protected_by(&abs, protected()) {
        eprintln!("{prog}: skipping protected path '{name}' (safe-rm list entry '{pat}')");
        return Err(1);
    }
    if let Some(pat) = crate::safety::protected_by(&abs, config_protected()) {
        eprintln!("{prog}: skipping protected path '{name}' (config protect.paths entry '{pat}')");
        return Err(1);
    }
    if opts.preserve_root && meta.is_dir() {
        if crate::safety::is_home(&abs) {
            eprintln!("{prog}: refusing to remove your home directory '{name}'");
            eprintln!("{prog}: use --no-preserve-root to override this failsafe");
            return Err(1);
        }
        if crate::safety::is_mount_root(&abs, &meta) {
            eprintln!("{prog}: skipping '{name}', since it is a mount point");
            eprintln!("{prog}: use --no-preserve-root to override this failsafe");
            return Err(1);
        }
    }
    if meta.is_dir() {
        if let Some(state) = crate::safety::risky_repo_state(&abs) {
            eprintln!(
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
                eprintln!("{prog}: cannot trash '{name}': in use by {who}");
                eprintln!("{prog}: use --in-use=warn to trash it anyway");
                return Err(1);
            }
            eprintln!("{prog}: warning: '{name}' is in use by {who}");
        }
    }
    if crate::finder::is_default(opts.trash_pin.is_some()) {
        return match crate::finder::put_item(&abs, &meta) {
            Ok((trash_name, trash_dir)) => {
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
                eprintln!("{prog}: cannot trash '{name}': {e}");
                Err(1)
            }
        };
    }
    let trash = match resolve_put_trash(opts, &abs, &meta) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{prog}: cannot trash '{name}': {e}");
            return Err(1);
        }
    };
    // Moving a directory into itself fails with a bare EINVAL, and an
    // item already in the trash would be trashed again, its info lost.
    match trash_overlap(&abs, &trash.root) {
        Some(Overlap::Contains) => {
            eprintln!(
                "{prog}: cannot trash '{name}': it contains the trash directory '{}' it would move into",
                trash.root.display()
            );
            return Err(1);
        }
        Some(Overlap::Inside) => {
            eprintln!(
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
            eprintln!("{prog}: cannot trash '{name}': {e}");
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
        td.ensure()?;
        return Ok(td);
    }
    if opts.home_only {
        let td = trashdir::TrashDir {
            root: trashdir::home_trash(),
            topdir: None,
        };
        td.ensure()?;
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
