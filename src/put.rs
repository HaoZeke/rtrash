use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::trashdir;
use crate::util::confirm;

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Interactive {
    Never,
    Once,
    Always,
}

#[derive(Clone)]
pub(crate) struct Opts {
    pub force: bool,
    pub recursive: bool,
    pub empty_dirs: bool,
    pub verbose: bool,
    pub interactive: Interactive,
    pub preserve_root: bool,
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
    let mut files: Vec<PathBuf> = Vec::new();
    let mut no_more_opts = false;
    let mut plain = false;

    for arg in args {
        if no_more_opts || arg == "-" || !arg.starts_with('-') {
            files.push(PathBuf::from(arg));
            continue;
        }
        match arg.as_str() {
            "--" => no_more_opts = true,
            "--plain" => plain = true,
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
    for file in &files {
        if let Err(code) = put_one(prog, file, &opts) {
            status = status.max(code);
        }
    }
    status
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

pub(crate) fn put_one(prog: &str, path: &Path, opts: &Opts) -> Result<(), i32> {
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
                return Ok(());
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
            return Ok(());
        }
    }

    let abs = match trashdir::abs_nofollow(path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{prog}: cannot remove '{name}': {e}");
            return Err(1);
        }
    };
    let trash = match trashdir::select(&abs, meta.dev()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{prog}: cannot trash '{name}': {e}");
            return Err(1);
        }
    };
    match trashdir::trash_move(&abs, &meta, &trash) {
        Ok(_) => {
            if opts.verbose {
                if meta.is_dir() {
                    println!("removed directory '{name}'");
                } else {
                    println!("removed '{name}'");
                }
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("{prog}: cannot trash '{name}': {e}");
            Err(1)
        }
    }
}
