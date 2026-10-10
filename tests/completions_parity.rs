//! Every long option a subcommand prints in `--help` must appear in the
//! bash, zsh and fish completion scripts and in the man page section for
//! that subcommand.

use std::fs;
use std::path::Path;
use std::process::Command;

const SUBCOMMANDS: &[&str] = &["put", "empty", "list", "status", "restore", "rm"];

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rtrash")
}

fn read(rel: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)).unwrap()
}

/// Long options from the option table of `rtrash SUB --help` (lines that
/// start with whitespace and then `-x, --opt` or `--opt`).
fn help_long_opts(sub: &str) -> Vec<String> {
    let out = Command::new(bin())
        .args([sub, "--help"])
        .output()
        .expect("run rtrash --help");
    assert!(out.status.success(), "{sub} --help failed");
    let text = String::from_utf8_lossy(&out.stdout);
    let mut opts = Vec::new();
    for line in text.lines() {
        if !line.starts_with(' ') {
            continue;
        }
        let mut rest = line.trim_start();
        if rest.len() > 4 && rest.starts_with('-') && rest.as_bytes()[2] == b',' {
            rest = rest[3..].trim_start();
        }
        let Some(rest) = rest.strip_prefix("--") else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        if !name.is_empty() {
            opts.push(name);
        }
    }
    opts.sort();
    opts.dedup();
    assert!(!opts.is_empty(), "no options parsed from {sub} --help");
    opts
}

/// Body of a `NAME=(` ... `)` array in a bash or zsh script.
fn array_block(script: &str, name: &str) -> String {
    let start = format!("{name}=(");
    let mut out = String::new();
    let mut inside = false;
    for line in script.lines() {
        if !inside {
            if line.trim_start().starts_with(&start) {
                inside = true;
            }
            continue;
        }
        if line.trim() == ")" {
            return out;
        }
        out.push_str(line);
        out.push('\n');
    }
    panic!("array {name} not found");
}

/// fish `complete` lines that apply to `rtrash SUB`.
fn fish_lines(script: &str, sub: &str) -> String {
    let direct = format!("string match -q {sub}'");
    let looped = "string match -q $sub\"";
    let in_loop = matches!(sub, "list" | "status");
    script
        .lines()
        .filter(|l| {
            l.contains(&direct)
                || (sub == "put" && l.contains("$put_cond"))
                || (in_loop && l.contains(looped))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// fish `complete` lines for a multi-call name (`trash-empty`, ...). The put
/// names share one `for cmd in trash-put trash` loop.
fn fish_multicall_lines(script: &str, name: &str) -> String {
    if name == "trash-put" {
        let mut out = Vec::new();
        let mut inside = false;
        for line in script.lines() {
            if line.starts_with("for cmd in trash-put") {
                inside = true;
            } else if inside && line.trim() == "end" {
                break;
            } else if inside {
                out.push(line);
            }
        }
        return out.join("\n");
    }
    let prefix = format!("complete -c {name} ");
    script
        .lines()
        .filter(|l| l.starts_with(&prefix))
        .collect::<Vec<_>>()
        .join("\n")
}

fn fish_has(lines: &str, opt: &str) -> bool {
    let flag = format!("-l {opt}");
    lines.split('\n').any(|l| {
        l.split(" -d ")
            .next()
            .unwrap_or("")
            .split_whitespace()
            .collect::<Vec<_>>()
            .windows(2)
            .any(|w| format!("{} {}", w[0], w[1]) == flag)
    })
}

#[test]
fn fish_multicall_names_cover_every_help_option() {
    let fish = read("completions/rtrash.fish");
    let mut missing = Vec::new();
    for (name, sub) in [
        ("trash-put", "put"),
        ("trash-empty", "empty"),
        ("trash-list", "list"),
        ("trash-restore", "restore"),
        ("trash-rm", "rm"),
    ] {
        let lines = fish_multicall_lines(&fish, name);
        for opt in help_long_opts(sub) {
            if !fish_has(&lines, &opt) {
                missing.push(format!("fish {name} --{opt}"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "missing completions:\n{}",
        missing.join("\n")
    );
}

#[test]
fn completion_scripts_cover_every_help_option() {
    let bash = read("completions/rtrash.bash");
    let zsh = read("completions/_rtrash");
    let fish = read("completions/rtrash.fish");
    let mut missing = Vec::new();
    for sub in SUBCOMMANDS {
        let bash_block = array_block(&bash, &format!("_rtrash_{sub}_opts"));
        let zsh_block = array_block(&zsh, &format!("{sub}_opts"));
        let fish_block = fish_lines(&fish, sub);
        for opt in help_long_opts(sub) {
            let flag = format!("--{opt}");
            let in_bash = bash_block
                .split_whitespace()
                .any(|w| w.trim_end_matches('=') == flag);
            if !in_bash {
                missing.push(format!("bash {sub} {flag}"));
            }
            if !zsh_block.contains(&flag) {
                missing.push(format!("zsh {sub} {flag}"));
            }
            // fish offers --help/--version once, not per subcommand.
            let global = opt == "help" || opt == "version";
            if !global && !fish_has(&fish_block, &opt) {
                missing.push(format!("fish {sub} {flag}"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "missing completions:\n{}",
        missing.join("\n")
    );
}

/// Lines of the man page from `.SS SUB` up to the next `.SS`.
fn man_section(man: &str, sub: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in man.lines() {
        if let Some(rest) = line.strip_prefix(".SS ") {
            if inside {
                break;
            }
            inside = rest == sub || rest.starts_with(&format!("{sub} "));
            continue;
        }
        if inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    assert!(!out.is_empty(), "man section {sub} not found");
    out
}

#[test]
fn man_page_documents_every_help_option() {
    let man = read("man/rtrash.1");
    let mut missing = Vec::new();
    for sub in SUBCOMMANDS {
        let section = man_section(&man, sub).replace("\\-", "-");
        for opt in help_long_opts(sub) {
            let flag = format!("--{opt}");
            if !section.contains(&flag) {
                missing.push(format!("man {sub} {flag}"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "missing in man page:\n{}",
        missing.join("\n")
    );
}
