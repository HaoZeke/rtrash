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
Without root, other users' processes are not seen.

      --json     print a JSON object ({holders, unreadable})
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
        json_escape(&h.how),
        json_escape(&str_from_os_ref(h.path.as_os_str())),
    )
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
    if json {
        let items: Vec<String> = holds.iter().map(hold_json).collect();
        println!(
            r#"{{"holders":[{}],"unreadable":{}}}"#,
            items.join(","),
            snap.unreadable
        );
    } else {
        for h in &holds {
            println!(
                "{:>7} {:<15} {:<7} {}",
                h.pid,
                crate::util::term_text(&h.command),
                h.how,
                crate::util::term_path(&h.path)
            );
        }
    }
    #[cfg(unix)]
    // SAFETY: geteuid has no preconditions.
    let root = unsafe { libc::geteuid() } == 0;
    #[cfg(windows)]
    let root = true;
    if snap.unreadable > 0 && !root && !json {
        eprintln!(
            "{prog}: {} could not be read; run as root to see them",
            crate::util::plural(snap.unreadable as u64, "process", "processes")
        );
    }
    i32::from(holds.is_empty())
}
