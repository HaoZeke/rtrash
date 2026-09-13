use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::trashdir;
use crate::util::confirm;

/// One exclusive lock per trash root for this `put` invocation.
struct PutBatch {
    held: HashMap<PathBuf, (trashdir::TrashDir, trashdir::TrashLock)>,
    /// One DeletionDate for every payload in this invocation.
    deletion_date: String,
    /// Canonical parents for `abs_nofollow` (same-dir batch).
    parents: HashMap<PathBuf, PathBuf>,
    /// Catalog rows to merge once at [`PutBatch::finish`].
    pending: HashMap<PathBuf, Vec<crate::list::Entry>>,
    /// Resolved trash root per device (same-FS batch).
    resolved: HashMap<u64, trashdir::TrashDir>,
    /// `--trash-dir` / `--home` pin: `ensure()` once, not once per name.
    sticky: Option<trashdir::TrashDir>,
    /// Roots whose `info/` had no `.trashinfo` when the lock was taken.
    /// Pending rows are then the whole catalog; do not rayon-scan them back.
    fresh: HashSet<PathBuf>,
}

impl PutBatch {
    fn new() -> Self {
        Self {
            held: HashMap::new(),
            deletion_date: crate::info::now_local_string(),
            parents: HashMap::new(),
            pending: HashMap::new(),
            resolved: HashMap::new(),
            sticky: None,
            fresh: HashSet::new(),
        }
    }

    fn abs_nofollow(&mut self, path: &Path) -> io::Result<PathBuf> {
        let file_name = path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new(""),
        };
        let key = parent.to_path_buf();
        if let Some(canon) = self.parents.get(&key) {
            return Ok(canon.join(file_name));
        }
        let canon = if parent.as_os_str().is_empty() {
            std::env::current_dir()?
        } else {
            parent.canonicalize()?
        };
        self.parents.insert(key, canon.clone());
        Ok(canon.join(file_name))
    }

    fn move_into(
        &mut self,
        abs: &Path,
        meta: &fs::Metadata,
        trash: trashdir::TrashDir,
    ) -> io::Result<String> {
        let key = trash.root.clone();
        if !self.held.contains_key(&key) {
            let lock = trashdir::TrashLock::acquire(&trash.root)?;
            if info_dir_has_no_trashinfo(&trash) {
                self.fresh.insert(key.clone());
            }
            self.held.insert(key.clone(), (trash, lock));
        }
        let (td, lock) = self.held.get(&key).expect("just inserted");
        // Per-name fsync of .trashinfo is the 401-entry put cost. The batch
        // fsyncs info/ once at finish (same durability window as a crash
        // mid-argv).
        trashdir::trash_move_locked_opts(abs, meta, td, lock, false, &self.deletion_date)
    }

    fn note(&mut self, trash: &trashdir::TrashDir, stored: String, original: PathBuf) {
        let epoch = crate::info::parse_local_to_epoch(&self.deletion_date).unwrap_or(0);
        self.pending
            .entry(trash.root.clone())
            .or_default()
            .push(crate::list::Entry {
                date: Some(self.deletion_date.clone()),
                epoch,
                original,
                name: stored,
                dir: trash.clone(),
                orphan: false,
            });
    }

    fn finish(&mut self) {
        for (td, lock) in self.held.values() {
            let _ = lock.sync_info();
            let extra = self.pending.remove(&td.root).unwrap_or_default();
            if extra.is_empty() {
                continue;
            }
            match crate::catalog_cache::load_unverified(td) {
                Some(mut rows) => {
                    let names: HashSet<&str> = extra.iter().map(|e| e.name.as_str()).collect();
                    rows.retain(|e| !names.contains(e.name.as_str()));
                    rows.extend(extra);
                    crate::catalog_cache::store(td, &rows);
                }
                None if self.fresh.contains(&td.root) => {
                    crate::catalog_cache::store(td, &extra);
                }
                None => {
                    // Existing can, no cache: do not spawn rayon from put.
                    // list collect rebuilds the blob.
                }
            }
        }
    }
}

fn info_dir_has_no_trashinfo(td: &trashdir::TrashDir) -> bool {
    let Ok(rd) = fs::read_dir(td.info()) else {
        return true;
    };
    !rd.filter_map(|e| e.ok())
        .any(|e| e.file_name().as_encoded_bytes().ends_with(b".trashinfo"))
}

/// Prompt policy for one put (GNU rm `-f`/`-i`/`-I` last-wins).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Interactive {
    /// Never prompt (`-f` / `--interactive=never`).
    Never,
    /// Prompt once before a large or recursive batch (`-I`).
    Once,
    /// Prompt before every removal (`-i`).
    Always,
}

/// Options for [`put_one`]. The CLI, TUI, and HUD share this struct.
#[derive(Clone, Debug)]
pub struct Opts {
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
    /// Refuse copy-into-home when volume trash cannot be used.
    pub no_home_fallback: bool,
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
            no_home_fallback: false,
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
  -v, --verbose         explain what is being done
      --one-file-system accepted for rm compatibility (no-op: entries are
                          moved whole, never traversed)
      --preserve-root   do not remove '/' (default)
      --no-preserve-root  do not treat '/' specially
      --trash-dir PATH  put into this trash root (creates files/ and info/ if
                          needed); `--trash-dir=PATH` also accepted; scripts
                          pin instead of XDG discovery
      --home-only       always use the home trash (skip volume trash)
      --no-home-fallback  refuse when volume trash cannot be used (no copy to $HOME)
      --json            emit a JSON summary {ok, failed, names, dests}; implies --plain
      --files-from=PATH read additional operands from PATH (`-` = stdin)
      -0, --zero        --files-from records are NUL-separated (default: newline)
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
    let mut opts = Opts::default();
    // trash-cli / gio / gtrash: `trash-put dir` trashes a tree without -r.
    // `rtrash put` and multi-call `rm` stay rm-shaped.
    if prog == "trash-put" || prog == "trash" {
        opts.recursive = true;
    }
    let mut files: Vec<PathBuf> = Vec::new();
    let mut no_more_opts = false;
    let mut plain = false;
    let mut json = false;
    let mut zero = false;
    let mut files_from: Option<PathBuf> = None;

    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        if no_more_opts || arg == "-" || !arg.starts_with('-') {
            files.push(PathBuf::from(arg));
            i += 1;
            continue;
        }
        match arg.as_str() {
            "--" => no_more_opts = true,
            "--plain" => plain = true,
            "--json" => {
                json = true;
                plain = true;
            }
            "-0" | "--zero" => zero = true,
            // GNU rm: -f / -i / --interactive last flag wins for prompt vs force.
            "--force" => {
                opts.force = true;
                opts.interactive = Interactive::Never;
            }
            "--recursive" => opts.recursive = true,
            "--dir" => opts.empty_dirs = true,
            "--verbose" => opts.verbose = true,
            "--one-file-system" => {}
            "--preserve-root" => opts.preserve_root = true,
            "--no-preserve-root" => opts.preserve_root = false,
            "--home-only" => opts.home_only = true,
            "--no-home-fallback" => opts.no_home_fallback = true,
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
            a if a == "--files-from" || a.starts_with("--files-from=") => {
                if files_from.is_some() {
                    return usage_err(prog, "put accepts only one --files-from");
                }
                if let Some(rest) = a.strip_prefix("--files-from=") {
                    files_from = Some(PathBuf::from(rest));
                } else {
                    i += 1;
                    let Some(p) = args.get(i) else {
                        return usage_err(prog, "--files-from requires PATH");
                    };
                    files_from = Some(PathBuf::from(p));
                }
            }
            a if crate::util::is_trash_dir_flag(a) => {
                if opts.trash_pin.is_some() {
                    return usage_err(prog, "put accepts only one --trash-dir PATH");
                }
                match crate::util::take_trash_dir_value(args, &mut i) {
                    Ok(p) => opts.trash_pin = Some(p),
                    Err(e) => return usage_err(prog, e),
                }
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
        i += 1;
    }

    if let Some(src) = &files_from {
        match read_files_from(src, zero) {
            Ok(extra) => files.extend(extra),
            Err(e) => {
                eprintln!("{prog}: cannot read --files-from '{}': {e}", src.display());
                return 1;
            }
        }
    }

    if files.is_empty() {
        #[cfg(feature = "tui")]
        if !plain && crate::util::stdin_is_tty() {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            return crate::put_tui::run(prog, &cwd);
        }
        let _ = plain;
        if opts.force {
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

    let mut status = 0;
    let mut ok = 0u64;
    let mut failed = 0u64;
    let mut names: Vec<String> = Vec::new();
    let mut dests: Vec<String> = Vec::new();
    let mut batch = PutBatch::new();
    for file in &files {
        match put_one_in(prog, file, &opts, &mut batch) {
            Ok(dest) => {
                ok += 1;
                names.push(file.display().to_string());
                dests.push(dest);
            }
            Err(code) => {
                failed += 1;
                status = status.max(code);
            }
        }
    }
    batch.finish();
    if json {
        let names_j: Vec<String> = names
            .iter()
            .map(|n| format!("\"{}\"", crate::list::json_escape(n)))
            .collect();
        let dests_j: Vec<String> = dests
            .iter()
            .map(|n| format!("\"{}\"", crate::list::json_escape(n)))
            .collect();
        println!(
            "{{\"ok\":{ok},\"failed\":{failed},\"names\":[{}],\"dests\":[{}]}}",
            names_j.join(","),
            dests_j.join(",")
        );
    }
    status
}

fn read_files_from(path: &Path, nul: bool) -> io::Result<Vec<PathBuf>> {
    use std::io::Read;
    let data = if path.as_os_str() == "-" {
        let mut buf = Vec::new();
        io::stdin().read_to_end(&mut buf)?;
        buf
    } else {
        fs::read(path)?
    };
    let sep = if nul { 0u8 } else { b'\n' };
    let mut out = Vec::new();
    for rec in data.split(|b| *b == sep) {
        let rec = if !nul && rec.last() == Some(&b'\r') {
            &rec[..rec.len() - 1]
        } else {
            rec
        };
        if rec.is_empty() {
            continue;
        }
        out.push(PathBuf::from(OsStr::from_bytes(rec)));
    }
    Ok(out)
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

/// Move one path into the trash. Shared by the CLI, TUI, and HUD.
pub fn put_one(prog: &str, path: &Path, opts: &Opts) -> Result<(), i32> {
    let p = path.to_path_buf();
    put_paths(prog, std::slice::from_ref(&p), opts)
        .into_iter()
        .find(Result::is_err)
        .unwrap_or(Ok(()))
}

/// Put many paths under one [`PutBatch`] (one lock and one DeletionDate per root).
pub fn put_paths(prog: &str, paths: &[PathBuf], opts: &Opts) -> Vec<Result<(), i32>> {
    let mut batch = PutBatch::new();
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        out.push(put_one_in(prog, p, opts, &mut batch).map(|_| ()));
    }
    batch.finish();
    out
}

fn put_one_in(prog: &str, path: &Path, opts: &Opts, batch: &mut PutBatch) -> Result<String, i32> {
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
                return Ok(String::new());
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
            return Ok(String::new());
        }
    }

    let abs = match batch.abs_nofollow(path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{prog}: cannot remove '{name}': {e}");
            return Err(1);
        }
    };
    let trash = match resolve_put_trash_cached(batch, opts, &abs, &meta) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{prog}: cannot trash '{name}': {e}");
            return Err(1);
        }
    };
    let trash_for_cache = trash.clone();
    match batch.move_into(&abs, &meta, trash) {
        Ok(stored) => {
            batch.note(&trash_for_cache, stored, abs.clone());
            if opts.verbose {
                if meta.is_dir() {
                    println!(
                        "removed directory '{name}' -> {}",
                        trash_for_cache.root.display()
                    );
                } else {
                    println!("removed '{name}' -> {}", trash_for_cache.root.display());
                }
            }
            Ok(trash_for_cache.root.display().to_string())
        }
        Err(e) => {
            eprintln!("{prog}: cannot trash '{name}': {e}");
            Err(1)
        }
    }
}

fn resolve_put_trash_cached(
    batch: &mut PutBatch,
    opts: &Opts,
    abs: &Path,
    meta: &fs::Metadata,
) -> io::Result<trashdir::TrashDir> {
    if opts.trash_pin.is_some() || opts.home_only {
        if let Some(td) = &batch.sticky {
            return Ok(td.clone());
        }
        let td = resolve_put_trash(opts, abs, meta)?;
        batch.sticky = Some(td.clone());
        return Ok(td);
    }
    if let Some(td) = batch.resolved.get(&meta.dev()) {
        return Ok(td.clone());
    }
    let td = resolve_put_trash(opts, abs, meta)?;
    batch.resolved.insert(meta.dev(), td.clone());
    Ok(td)
}

/// Pick destination trash: explicit pin (created if needed), home-only, or auto-select.
fn resolve_put_trash(
    opts: &Opts,
    abs: &Path,
    meta: &fs::Metadata,
) -> io::Result<trashdir::TrashDir> {
    if let Some(pin) = &opts.trash_pin {
        let root = if pin.is_absolute() {
            pin.clone()
        } else {
            std::env::current_dir()?.join(pin)
        };
        let td = trashdir::TrashDir {
            root: root.clone(),
            topdir: trashdir::infer_topdir(&root),
        };
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
    trashdir::select_opts(abs, meta.dev(), !opts.no_home_fallback)
}

#[cfg(test)]
mod tests {
    #[test]
    fn trash_pin_is_cached_for_the_batch() {
        let src = include_str!("put.rs");
        assert!(
            src.contains("sticky: Option<trashdir::TrashDir>"),
            "--trash-dir must not mkdir files/info once per name"
        );
        assert!(
            src.contains("if let Some(td) = &batch.sticky"),
            "pin/home trash is reused after the first ensure"
        );
        assert!(
            !src.contains(concat!("list::", "collect_one")),
            "put finish must not rayon-scan info/ after writing it"
        );
    }
}
