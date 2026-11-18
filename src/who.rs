//! `rtrash who`: the live processes holding a path or anything below it.

use std::path::PathBuf;

use crate::holders::{Hold, Snapshot};
use crate::util::{json_escape, str_from_os_ref};

const HELP: &str = "\
Usage: {prog} [OPTION]... PATH...
List the live processes that hold PATH or anything below it: an open
descriptor, a memory map, the working or root directory, or the
executable. Linux reads /proc as lsof and fuser do; macOS asks libproc
(no memory maps); Windows asks the Restart Manager (open files only).
Without root, other users' processes may be unreadable.

      --json     print a JSON object ({holders, unreadable, unreadable_mine})
      --help     display this help and exit
      --version  output version information and exit

Exit status is 0 when some process holds a PATH, 1 when none does or the
platform cannot tell, and 2 on a usage error.
";

fn usage_err(prog: &str, msg: &str) -> i32 {
    eprintln!("{prog}: {msg}");
    eprintln!("Try '{prog} --help' for more information.");
    2
}

/// One hold as a JSON object.
pub fn hold_json(h: &Hold) -> String {
    format!(
        r#"{{"pid":{},"uid":{},"command":"{}","how":"{}","path":"{}"}}"#,
        h.pid,
        h.uid.map_or("null".to_string(), |u| u.to_string()),
        json_escape(&h.command),
        json_escape(&hold_how(h)),
        json_escape(&str_from_os_ref(h.path.as_os_str())),
    )
}

fn hold_how(h: &Hold) -> String {
    h.how
        .strip_prefix("fd ")
        .map_or_else(|| h.how.clone(), |fd| format!("open fd {fd}"))
}

#[cfg(unix)]
fn hold_user(h: &Hold) -> String {
    h.uid
        .map(crate::trashdir::uid_name)
        .unwrap_or_else(|| "?".to_string())
}

#[cfg(windows)]
fn hold_user(_h: &Hold) -> String {
    "?".to_string()
}

#[cfg(unix)]
fn current_user() -> String {
    // SAFETY: geteuid has no preconditions.
    crate::trashdir::uid_name(unsafe { libc::geteuid() })
}

#[cfg(windows)]
fn current_user() -> String {
    "current user".to_string()
}

fn unreadable_summary(prog: &str, snap: &Snapshot, root: bool) -> String {
    let user = current_user();
    let mine = if snap.unreadable_mine > 0 {
        format!("at least one belongs to current user '{user}'")
    } else {
        format!("none belong to current user '{user}'")
    };
    let mut line = format!(
        "{prog}: {} could not be read; {mine}",
        crate::util::plural(snap.unreadable as u64, "process", "processes")
    );
    if !root && snap.unreadable > 0 {
        line.push_str("; run as root to see them");
    }
    line
}

pub fn run(prog: &str, args: &[String]) -> i32 {
    let mut json = false;
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut no_more = false;
    for arg in args {
        if no_more || !arg.starts_with('-') || arg == "-" {
            paths.push(crate::util::arg_path(arg));
            continue;
        }
        match arg.as_str() {
            "--" => no_more = true,
            "--json" => json = true,
            "--help" => {
                print!("{}", HELP.replace("{prog}", prog));
                return 0;
            }
            "--version" => {
                println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                return 0;
            }
            other => return usage_err(prog, &format!("unrecognized option '{other}'")),
        }
    }
    if paths.is_empty() {
        return usage_err(prog, "missing operand");
    }
    let snap = Snapshot::take();
    if !snap.supported {
        eprintln!("{prog}: cannot list the processes holding a path on this platform");
        return 1;
    }
    let mut holds: Vec<Hold> = paths.iter().flat_map(|p| snap.under(p)).collect();
    holds.sort_by(|a, b| (a.pid, &a.path, &a.how).cmp(&(b.pid, &b.path, &b.how)));
    holds.dedup();
    #[cfg(unix)]
    // SAFETY: geteuid has no preconditions.
    let root = unsafe { libc::geteuid() } == 0;
    #[cfg(windows)]
    let root = true;
    if json {
        let items: Vec<String> = holds.iter().map(hold_json).collect();
        println!(
            r#"{{"holders":[{}],"unreadable":{},"unreadable_mine":{}}}"#,
            items.join(","),
            snap.unreadable,
            snap.unreadable_mine
        );
    } else {
        for h in &holds {
            println!(
                "{:>7} {:<12} {:<15} {:<10} {}",
                h.pid,
                crate::util::term_text(&hold_user(h)),
                crate::util::term_text(&h.command),
                hold_how(h),
                crate::util::term_path(&h.path)
            );
        }
        eprintln!("{}", unreadable_summary(prog, &snap, root));
    }
    i32::from(holds.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_json_writes_open_fd_labels() {
        let h = Hold {
            pid: 7,
            uid: Some(42),
            command: "cat".to_string(),
            how: "fd 3".to_string(),
            path: PathBuf::from("/tmp/a"),
            names: Vec::new(),
        };
        let j = hold_json(&h);
        assert!(j.contains(r#""how":"open fd 3""#), "{j}");
    }

    #[test]
    fn unreadable_summary_counts_and_names_current_user() {
        let mut snap = Snapshot::default();
        snap.unreadable = 2;
        snap.unreadable_mine = 1;
        let s = unreadable_summary("rtrash who", &snap, false);
        assert!(s.contains("2 processes could not be read"), "{s}");
        assert!(s.contains("current user"), "{s}");
        assert!(s.contains("run as root"), "{s}");
    }
}
