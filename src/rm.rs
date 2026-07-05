//! Selective permanent delete of trash entries (trash-cli `trash-rm` role).

use std::path::PathBuf;

use crate::list;
use crate::trashdir::{self, TrashDir};
use crate::util::fnmatch;

const HELP: &str = "\
Usage: {prog} [OPTION]... PATTERN...
Permanently remove trashed items whose original path or basename matches
a PATTERN (shell-style glob; quote globs from the shell). Matching entries
are deleted from the trash (files/ + .trashinfo), not restored.

      --trash-dir=PATH  only consider this trash directory (repeatable)
  -v, --verbose    print each permanently removed original path
      --help       display this help and exit
      --version    output version information and exit

Examples:
  {prog} '*.o'
  {prog} foo
  {prog} /home/you/old-project
";

pub fn run(prog: &str, args: &[String]) -> i32 {
    let mut verbose = false;
    let mut trash_dirs: Vec<PathBuf> = Vec::new();
    let mut patterns: Vec<String> = Vec::new();

    for arg in args {
        match arg.as_str() {
            "-v" | "--verbose" => verbose = true,
            "--help" => {
                print!("{}", HELP.replace("{prog}", prog));
                return 0;
            }
            "--version" => {
                println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                return 0;
            }
            a if a.starts_with("--trash-dir=") => {
                trash_dirs.push(PathBuf::from(&a["--trash-dir=".len()..]));
            }
            a if a.starts_with('-') && a.len() > 1 => {
                eprintln!("{prog}: unrecognized option '{a}'");
                eprintln!("Try '{prog} --help' for more information.");
                return 2;
            }
            a => patterns.push(a.to_string()),
        }
    }

    if patterns.is_empty() {
        eprintln!("{prog}: missing operand");
        eprintln!("Try '{prog} --help' for more information.");
        return 2;
    }

    let dirs: Vec<TrashDir> = trashdir::resolve_dirs(&trash_dirs);
    let entries = list::collect(&dirs);
    let mut status = 0i32;
    let mut removed = 0u64;

    for entry in &entries {
        let path_s = entry.original.to_string_lossy();
        let base = entry
            .original
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        let hit = patterns.iter().any(|pat| {
            fnmatch(pat, path_s.as_ref())
                || fnmatch(pat, base.as_str())
                || fnmatch(pat, entry.name.as_str())
        });
        if !hit {
            continue;
        }
        let payload = entry.dir.files().join(&entry.name);
        let info_path = entry.dir.info().join(format!("{}.trashinfo", entry.name));
        if let Err(e) = trashdir::remove_any_path(&payload) {
            eprintln!(
                "{prog}: cannot remove '{}': {e}",
                payload.display()
            );
            status = 1;
            continue;
        }
        if let Err(e) = std::fs::remove_file(&info_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "{prog}: cannot remove '{}': {e}",
                    info_path.display()
                );
                status = 1;
                continue;
            }
        }
        // Drop directorysizes line for this trash name if present.
        trashdir::directorysizes_remove(&entry.dir, &entry.name);
        if verbose {
            println!("removed '{}'", entry.original.display());
        }
        removed += 1;
    }

    if verbose || removed > 0 {
        let noun = if removed == 1 { "item" } else { "items" };
        eprintln!("Permanently removed {removed} {noun}");
    }
    status
}

#[cfg(test)]
mod tests {
    use crate::util::fnmatch;

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
