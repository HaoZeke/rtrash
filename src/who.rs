//! `rtrash who`: the live processes holding a path or anything below it.

use std::path::PathBuf;

use crate::holders::{Hold, Snapshot};
use crate::util::{json_escape, str_from_os_ref};

const HELP: &str = "\
Usage: {prog} [OPTION]... PATH...
List the live processes that hold PATH or anything below it: an open
descriptor, a memory map, the working or root directory, or the
executable. One line per holder, for each PATH: the path asked about,
the process id, the user, the command (the short name, and the command
line when it is known), how the path is held, and the path held.
When nothing holds a PATH, that path gets one line saying so. The line
says the result is partial when that path could not be checked in full
or some process could not be inspected.
Linux reads /proc as lsof and fuser do; macOS asks libproc
(no memory maps); Windows asks the Restart Manager (open files only).
Without root, other users' processes cannot be inspected: who lists the
holders it can see and says on stderr how many processes it could not
inspect, since an empty list is then no proof that a PATH is free.

      --json     print a JSON object ({holders, unreadable}); each
                   holder has pid, uid, user, command, cmdline, how, path
      --help     display this help and exit
      --version  output version information and exit

Exit status is 0 when some process holds a PATH, 1 when none was seen or
the platform cannot tell, and 2 on a usage error.
";

fn usage_err(prog: &str, msg: &str) -> i32 {
    eprintln!("{prog}: {msg}");
    eprintln!("Try '{prog} --help' for more information.");
    2
}

/// The account name of a holder, or `-` when the platform gives no uid.
fn user_of(uid: Option<u32>) -> String {
    match uid {
        #[cfg(unix)]
        Some(uid) => crate::trashdir::uid_name(uid),
        #[cfg(not(unix))]
        Some(uid) => uid.to_string(),
        None => "-".to_string(),
    }
}

/// The short command, with the command line in parentheses when it
/// says more than the short name.
fn command_of(h: &Hold) -> String {
    if h.cmdline.is_empty() || h.cmdline == h.command {
        h.command.clone()
    } else {
        format!("{} ({})", h.command, h.cmdline)
    }
}

/// One holder of `asked`, naming the process, its user and its command.
fn holder_line(asked: &std::path::Path, h: &Hold) -> String {
    format!(
        "{}: pid {} user {} command {} {} {}",
        crate::util::term_path(asked),
        h.pid,
        crate::util::term_text(&user_of(h.uid)),
        crate::util::term_text(&command_of(h)),
        h.how,
        crate::util::term_path(&h.path),
    )
}

/// One path nobody visible holds. `gap` is that path's own probe: an
/// unchecked path, or processes that could not be inspected. Another
/// path's failure is not this line's.
fn none_line(asked: &std::path::Path, gap: Option<&str>) -> String {
    match gap {
        Some(gap) => format!(
            "{}: no process holds this path; the result is partial ({gap})",
            crate::util::term_path(asked)
        ),
        None => format!(
            "{}: no process holds this path",
            crate::util::term_path(asked)
        ),
    }
}

/// One hold as a JSON object.
pub fn hold_json(h: &Hold) -> String {
    let user = match h.uid {
        Some(_) => format!("\"{}\"", json_escape(&user_of(h.uid))),
        None => "null".to_string(),
    };
    format!(
        r#"{{"pid":{},"uid":{},"user":{},"command":"{}","cmdline":"{}","how":"{}","path":"{}"}}"#,
        h.pid,
        h.uid.map_or("null".to_string(), |u| u.to_string()),
        user,
        json_escape(&h.command),
        json_escape(&h.cmdline),
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
    let mut unchecked = 0;
    let mut holds: Vec<Hold> = Vec::new();
    for p in &paths {
        let probe = snap.probe(p);
        unchecked += usize::from(probe.unchecked);
        let mut mine = probe.holds;
        mine.sort_by(|a, b| (a.pid, &a.path, &a.how).cmp(&(b.pid, &b.path, &b.how)));
        mine.dedup();
        // Human lines stay off stdout under --json. The gap note below
        // still goes to stderr, in JSON mode too.
        if !json {
            if mine.is_empty() {
                let gap = snap.gap_of(usize::from(probe.unchecked));
                println!("{}", none_line(p, gap.as_deref()));
            } else {
                for h in &mine {
                    println!("{}", holder_line(p, h));
                }
            }
        }
        holds.extend(mine);
    }
    holds.sort_by(|a, b| (a.pid, &a.path, &a.how).cmp(&(b.pid, &b.path, &b.how)));
    holds.dedup();
    if json {
        let items: Vec<String> = holds.iter().map(hold_json).collect();
        println!(
            r#"{{"holders":[{}],"unreadable":{}}}"#,
            items.join(","),
            snap.unreadable
        );
    }
    if let Some(gap) = snap.gap_of(unchecked) {
        #[cfg(unix)]
        // SAFETY: geteuid has no preconditions.
        let hint = if snap.unreadable == 0 || unsafe { libc::geteuid() } == 0 {
            ""
        } else {
            "; run as root to inspect them"
        };
        #[cfg(windows)]
        let hint = "";
        if holds.is_empty() {
            eprintln!("{prog}: no holder seen, but {gap}, so a path may still be in use{hint}");
        } else {
            eprintln!("{prog}: {gap}, so the list may be incomplete{hint}");
        }
    }
    i32::from(holds.is_empty())
}
