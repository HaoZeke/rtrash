//! Every long option a subcommand prints in `--help` must appear in the
//! bash, zsh and fish completion scripts and in the man page section for
//! that subcommand. The scripts and the man page cover the Unix build,
//! whose `--help` text differs from the Windows one.
#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::Command;

const SUBCOMMANDS: &[&str] = &[
    "put",
    "empty",
    "list",
    "status",
    "trash-stats",
    "du",
    "restore",
    "undo",
    "rm",
    "who",
    "reap",
    "finder",
    "wsl",
    "config",
];

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
        let bash_block = array_block(&bash, &format!("_rtrash_{}_opts", sub.replace('-', "_")));
        let zsh_block = array_block(&zsh, &format!("{}_opts", sub.replace('-', "_")));
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

/// Run the bash completion function on `words` (the last one is the word
/// being completed) and return what it offers. Uses /bin/bash when present,
/// which on macOS is bash 3.2.
#[cfg(unix)]
fn bash_complete(words: &[&str]) -> Vec<String> {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("completions/rtrash.bash");
    let quoted: Vec<String> = words
        .iter()
        .map(|w| format!("'{}'", w.replace('\'', r"'\''")))
        .collect();
    let prog = format!(
        "source '{}'; COMP_WORDS=({}); COMP_CWORD={}; _rtrash_main; printf '%s\\n' \"${{COMPREPLY[@]}}\"",
        script.display(),
        quoted.join(" "),
        words.len() - 1
    );
    let shell = if Path::new("/bin/bash").exists() {
        "/bin/bash"
    } else {
        "bash"
    };
    let out = Command::new(shell)
        .args(["--norc", "--noprofile", "-c", &prog])
        .current_dir(std::env::temp_dir())
        .output()
        .expect("run bash");
    assert!(
        out.status.success(),
        "bash: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

#[cfg(unix)]
#[test]
fn bash_completion_offers_the_right_words() {
    assert_eq!(
        bash_complete(&["rtrash", "completions", ""]),
        ["bash", "zsh", "fish", "nu", "powershell", "elvish"]
    );
    let keys = bash_complete(&["rtrash", "keys", "--"]);
    assert!(keys.contains(&"--sample".to_string()), "{keys:?}");
    assert!(!keys.contains(&"--recursive".to_string()), "{keys:?}");
    let setup = bash_complete(&["rtrash", "setup", "--pr"]);
    assert_eq!(setup, ["--prefix="]);
    assert_eq!(bash_complete(&["rtrash", "restore", "--la"]), ["--latest"]);
    assert_eq!(bash_complete(&["trash-restore", "--js"]), ["--json"]);
    assert_eq!(bash_complete(&["rtrash", "du", "--print"]), ["--print0"]);
}

/// bash completes `restore` and `rm` with the original paths and names in the
/// trash, read from `rtrash list --format`, and quotes the spaces.
#[cfg(unix)]
#[test]
fn bash_restore_completes_trashed_items() {
    let root = std::env::temp_dir().join(format!("rtrash-compl-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("work")).unwrap();
    fs::write(root.join("work/a b.txt"), "x").unwrap();
    let envs = [
        ("XDG_DATA_HOME", root.join("xdg")),
        ("XDG_CONFIG_HOME", root.join(".config")),
        ("HOME", root.clone()),
    ];
    let put = Command::new(bin())
        .args(["put", "a b.txt"])
        .current_dir(root.join("work"))
        .envs(envs.iter().map(|(k, v)| (*k, v.as_os_str())))
        .output()
        .unwrap();
    assert!(
        put.status.success(),
        "{}",
        String::from_utf8_lossy(&put.stderr)
    );
    let bin_dir = Path::new(bin()).parent().unwrap();
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("completions/rtrash.bash");
    let run = |words: &str| -> Vec<String> {
        let prog = format!(
            "source '{}'; COMP_WORDS=({words}); COMP_CWORD=$((${{#COMP_WORDS[@]}} - 1)); _rtrash_main; printf '%s\\n' \"${{COMPREPLY[@]}}\"",
            script.display()
        );
        let out = Command::new("bash")
            .args(["--norc", "--noprofile", "-c", &prog])
            .envs(envs.iter().map(|(k, v)| (*k, v.as_os_str())))
            .env("PATH", &path)
            .output()
            .expect("run bash");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect()
    };
    let paths = run("rtrash restore ''");
    let names = run("rtrash restore --name=a");
    let multi = run("trash-restore ''");
    let rm = run("rtrash rm ''");
    let _ = fs::remove_dir_all(&root);
    assert!(paths.iter().any(|p| p.ends_with("/a\\ b.txt")), "{paths:?}");
    assert!(names.contains(&"a\\ b.txt".to_string()), "{names:?}");
    assert!(multi.iter().any(|p| p.ends_with("/a\\ b.txt")), "{multi:?}");
    assert!(rm.iter().any(|p| p.ends_with("/a\\ b.txt")), "{rm:?}");
}

/// bash completes `wsl restore` and `wsl rm` with the Recycle Bin items
/// (and `--name=` with their file names), read from
/// `rtrash wsl list --format`, against a made-up WSL drive tree.
#[cfg(unix)]
#[test]
fn bash_wsl_restore_completes_recycled_items() {
    use rtrash::win_recycle::{encode_recycle_i_v2, FILETIME_UNIX_EPOCH};
    let sid = "S-1-5-21-1-2-3-1001";
    let top = std::env::temp_dir().join(format!("rtrash-compl-wsl-{}", std::process::id()));
    let _ = fs::remove_dir_all(&top);
    let drives = top.join("drives");
    let bin_dir = drives.join("c").join("$Recycle.Bin").join(sid);
    fs::create_dir_all(&bin_dir).unwrap();
    fs::write(top.join("osrelease"), "5.15.0-microsoft-standard-WSL2\n").unwrap();
    fs::write(
        top.join("wsl.conf"),
        format!("[automount]\nroot = {}/\n", drives.display()),
    )
    .unwrap();
    fs::write(bin_dir.join("$RAB12CD.txt"), "n").unwrap();
    let rec = encode_recycle_i_v2(
        Path::new(r"C:\Users\me\a b.txt"),
        1,
        FILETIME_UNIX_EPOCH + 1_700_000_000 * 10_000_000,
    );
    fs::write(bin_dir.join("$IAB12CD.txt"), rec).unwrap();
    let exe_dir = Path::new(bin()).parent().unwrap();
    let path = format!(
        "{}:{}",
        exe_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("completions/rtrash.bash");
    let run = |words: &str| -> Vec<String> {
        let prog = format!(
            "source '{}'; COMP_WORDS=({words}); COMP_CWORD=$((${{#COMP_WORDS[@]}} - 1)); _rtrash_main; printf '%s\\n' \"${{COMPREPLY[@]}}\"",
            script.display()
        );
        let out = Command::new("bash")
            .args(["--norc", "--noprofile", "-c", &prog])
            .env("PATH", &path)
            .env("RTRASH_WSL_OSRELEASE", top.join("osrelease"))
            .env("RTRASH_WSL_CONF", top.join("wsl.conf"))
            .env("RTRASH_WSL_SID", sid)
            .output()
            .expect("run bash");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect()
    };
    let items = run("rtrash wsl restore ''");
    let rm = run("rtrash wsl rm ''");
    let names = run("rtrash wsl rm --name=a");
    let actions = run("rtrash wsl ''");
    let _ = fs::remove_dir_all(&top);
    assert!(
        items.iter().any(|p| p.ends_with("/c/Users/me/a\\ b.txt")),
        "{items:?}"
    );
    assert!(!items.contains(&"restore".to_string()), "{items:?}");
    assert!(
        rm.iter().any(|p| p.ends_with("/c/Users/me/a\\ b.txt")),
        "{rm:?}"
    );
    assert_eq!(names, ["a\\ b.txt"]);
    assert!(actions.contains(&"restore".to_string()), "{actions:?}");
}

/// Same check through fish, when it is installed.
#[cfg(unix)]
#[test]
fn fish_completion_offers_the_right_words() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("completions/rtrash.fish");
    let run = |line: &str| -> Option<Vec<String>> {
        let prog = format!("source '{}'; complete -C '{line}'", script.display());
        let out = Command::new("fish")
            .args(["--no-config", "-c", &prog])
            .output()
            .ok()?;
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        Some(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(|l| l.split('\t').next().unwrap_or("").to_string())
                .collect(),
        )
    };
    let Some(shells) = run("rtrash completions ") else {
        eprintln!("fish not installed; skipping");
        return;
    };
    assert_eq!(
        shells,
        ["bash", "elvish", "fish", "nu", "powershell", "zsh"]
    );
    let keys = run("rtrash keys --").unwrap();
    assert!(keys.contains(&"--sample".to_string()), "{keys:?}");
    let subs = run("rtrash k").unwrap();
    assert!(subs.contains(&"keys".to_string()), "{subs:?}");
}

#[test]
fn zsh_completion_knows_keys_and_every_shell() {
    let zsh = read("completions/_rtrash");
    assert!(zsh.contains("'keys:"), "keys subcommand missing");
    assert!(zsh.contains("'1:shell:(bash zsh fish nu powershell elvish)'"));
    assert!(zsh.contains("--sample["));
}

/// Lines of the nushell `export extern "NAME" [` ... `]` block.
fn nu_block(script: &str, name: &str) -> String {
    let start = format!("export extern \"{name}\" [");
    let mut out = String::new();
    let mut inside = false;
    for line in script.lines() {
        if !inside {
            inside = line == start;
            continue;
        }
        if line == "]" {
            return out;
        }
        out.push_str(line);
        out.push('\n');
    }
    panic!("nu extern {name} not found");
}

/// Long options that `rtrash SUB --help` shows with an optional value,
/// such as `--interactive[=WHEN]`.
fn help_optional_value_opts(sub: &str) -> Vec<String> {
    let out = Command::new(bin()).args([sub, "--help"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.split("--")
        .skip(1)
        .filter_map(|rest| {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            rest[name.len()..].starts_with("[=").then_some(name)
        })
        .collect()
}

/// Every long option of `--help` is a flag of the matching nushell extern.
/// One that takes an optional value is declared `--opt?: string`, so both
/// `--opt` and `--opt=VALUE` pass through.
#[test]
fn nu_externs_cover_every_help_option() {
    let nu = read("completions/rtrash.nu");
    let mut missing = Vec::new();
    let multicall = [
        ("trash-put", "put"),
        ("trash-empty", "empty"),
        ("trash-list", "list"),
        ("trash-restore", "restore"),
        ("trash-rm", "rm"),
    ];
    let subs = SUBCOMMANDS.iter().map(|s| (format!("rtrash {s}"), *s));
    for (name, sub) in subs.chain(multicall.iter().map(|(n, s)| (n.to_string(), *s))) {
        let block = nu_block(&nu, &name);
        for opt in help_long_opts(sub) {
            let flag = format!("--{opt}");
            let found = block.lines().any(|l| {
                let l = l.trim_start();
                let word: String = l
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                    .collect();
                word == flag
            });
            if !found {
                missing.push(format!("nu {name} {flag}"));
            }
        }
        for opt in help_optional_value_opts(sub) {
            let decl = format!("--{opt}?: string");
            if !block.lines().any(|l| l.trim_start().starts_with(&decl)) {
                missing.push(format!("nu {name} {decl}"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "missing in rtrash.nu:\n{}",
        missing.join("\n")
    );
}

/// `rtrash completions nu` prints the script, and nushell parses it when
/// installed.
#[test]
fn nu_script_is_embedded_and_parses() {
    let out = Command::new(bin())
        .args(["completions", "nu"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        read("completions/rtrash.nu")
    );
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("completions/rtrash.nu");
    let prog = format!(
        "source '{}'; scope externs | where name == 'rtrash empty' | length",
        script.display()
    );
    let Ok(run) = Command::new("nu").args(["-n", "-c", &prog]).output() else {
        eprintln!("nu not installed; skipping");
        return;
    };
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "1");
}

/// The quoted words of the `'SUB' = @(...)` line of the PowerShell script.
fn powershell_words(script: &str, sub: &str) -> Vec<String> {
    let start = format!("'{sub}' = @(");
    let line = script
        .lines()
        .map(str::trim_start)
        .find(|l| l.starts_with(&start))
        .unwrap_or_else(|| panic!("powershell table for {sub} not found"));
    line[start.len()..]
        .trim_end_matches(')')
        .split(", ")
        .map(|w| w.trim_matches('\'').trim_end_matches('=').to_string())
        .collect()
}

/// Every long option of `--help` is in the PowerShell script's table for
/// that subcommand; the multi-call names share the tables.
#[test]
fn powershell_tables_cover_every_help_option() {
    let ps = read("completions/rtrash.ps1");
    let mut missing = Vec::new();
    for sub in SUBCOMMANDS {
        let words = powershell_words(&ps, sub);
        for opt in help_long_opts(sub) {
            let flag = format!("--{opt}");
            if !words.contains(&flag) {
                missing.push(format!("powershell {sub} {flag}"));
            }
        }
    }
    for (name, sub) in [
        ("trash-put", "put"),
        ("trash-empty", "empty"),
        ("trash-list", "list"),
        ("trash-restore", "restore"),
        ("trash-rm", "rm"),
    ] {
        if !ps.contains(&format!("'{name}' = '{sub}'")) {
            missing.push(format!("powershell multi-call {name}"));
        }
    }
    assert!(
        missing.is_empty(),
        "missing in rtrash.ps1:\n{}",
        missing.join("\n")
    );
}

/// `rtrash completions powershell` prints the script, and PowerShell
/// completes from it when pwsh is installed.
#[test]
fn powershell_script_is_embedded_and_completes() {
    let out = Command::new(bin())
        .args(["completions", "powershell"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        read("completions/rtrash.ps1")
    );
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("completions/rtrash.ps1");
    let prog = format!(
        ". '{0}'; foreach ($l in @('rtrash empty --da', 'trash-restore --la', 'rtrash k')) {{ \
         (TabExpansion2 -inputScript $l -cursorColumn $l.Length).CompletionMatches.CompletionText -join ' ' }}; \
         $env:OS = 'Windows_NT'; . '{0}'; \
         (TabExpansion2 -inputScript 'rtrash empty --' -cursorColumn 15).CompletionMatches.CompletionText -join ' '",
        script.display()
    );
    let Ok(run) = Command::new("pwsh")
        .args(["-NoProfile", "-NonInteractive", "-Command", &prog])
        .output()
    else {
        // CI installs pwsh on every runner, so a missing one there is a
        // broken job, not a reason to pass quietly.
        assert!(std::env::var_os("CI").is_none(), "pwsh missing on CI");
        eprintln!("pwsh not installed; skipping");
        return;
    };
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let got: Vec<String> = String::from_utf8_lossy(&run.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(
        got,
        [
            "--dangling",
            "--latest",
            "keys",
            "--dry-run --verbose --older-than= --max-size= --prune= --until-free= --all --json --help --version"
        ]
    );
}

/// The words of the `&SUB=[...]` line of the elvish script.
fn elvish_words(script: &str, sub: &str) -> Vec<String> {
    let start = format!("&{sub}=[");
    let line = script
        .lines()
        .map(str::trim_start)
        .find(|l| l.starts_with(&start))
        .unwrap_or_else(|| panic!("elvish map entry for {sub} not found"));
    line[start.len()..]
        .trim_end_matches(']')
        .split_whitespace()
        .map(|w| w.trim_end_matches('=').to_string())
        .collect()
}

/// Every long option of `--help` is in the elvish script's map for that
/// subcommand; the multi-call names share the map.
#[test]
fn elvish_map_covers_every_help_option() {
    let elv = read("completions/rtrash.elv");
    let mut missing = Vec::new();
    for sub in SUBCOMMANDS {
        let words = elvish_words(&elv, sub);
        for opt in help_long_opts(sub) {
            let flag = format!("--{opt}");
            if !words.contains(&flag) {
                missing.push(format!("elvish {sub} {flag}"));
            }
        }
    }
    for (name, sub) in [
        ("trash-put", "put"),
        ("trash-empty", "empty"),
        ("trash-list", "list"),
        ("trash-restore", "restore"),
        ("trash-rm", "rm"),
    ] {
        if !elv.contains(&format!("&{name}={sub}")) {
            missing.push(format!("elvish multi-call {name}"));
        }
    }
    assert!(
        missing.is_empty(),
        "missing in rtrash.elv:\n{}",
        missing.join("\n")
    );
}

/// `rtrash completions elvish` prints the script, and elvish completes
/// from it when installed. The line editor exists only in an interactive
/// shell, so the run leaves out the lines that register the completer.
#[test]
fn elvish_script_is_embedded_and_completes() {
    let out = Command::new(bin())
        .args(["completions", "elvish"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let script = read("completions/rtrash.elv");
    assert_eq!(String::from_utf8_lossy(&out.stdout), script);
    let (body, hooks) = script
        .split_once("# Register with the line editor")
        .expect("hook marker");
    assert!(hooks.contains("edit:completion:arg-completer[rtrash]"));
    let dir = std::env::temp_dir().join(format!("rtrash-elvish-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("t.elv");
    fs::write(
        &file,
        format!(
            "{body}\nfor l [[rtrash empty --da] [trash-restore --la] [rtrash k]] {{ echo (str:join ' ' [(complete $@l)]) }}\n"
        ),
    )
    .unwrap();
    let run = Command::new("elvish").arg("-norc").arg(&file).output();
    let _ = fs::remove_dir_all(&dir);
    let Ok(run) = run else {
        eprintln!("elvish not installed; skipping");
        return;
    };
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let got: Vec<String> = String::from_utf8_lossy(&run.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(got, ["--dangling", "--latest", "keys"]);
}

/// Long options in each Windows help text of src/win_recycle.rs, keyed by
/// the subcommand its second line names.
fn windows_help_opts() -> Vec<(&'static str, Vec<String>)> {
    let src = read("src/win_recycle.rs");
    let keys = [
        ("Move FILE(s)", "put"),
        ("List items", "list"),
        ("Permanently empty", "empty"),
        ("Restore an item", "restore"),
        ("Summarize the Windows", "status"),
        ("Permanently delete Recycle", "rm"),
    ];
    let mut out = Vec::new();
    for block in src.split("Usage: {prog}").skip(1) {
        let text = block.split('"').next().unwrap_or("");
        let mut lines = text.lines().skip(1);
        let Some(sub) = lines
            .next()
            .and_then(|l| keys.iter().find(|(k, _)| l.starts_with(k)))
            .map(|(_, s)| *s)
        else {
            continue;
        };
        let mut opts = Vec::new();
        for line in lines {
            let indent = line.len() - line.trim_start().len();
            if !(2..=6).contains(&indent) || !line.trim_start().starts_with('-') {
                continue;
            }
            let spec = line.trim_start().split("  ").next().unwrap_or("");
            for tok in spec.split([',', ' ']).filter(|t| t.starts_with("--")) {
                let name: String = tok[2..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                    .collect();
                opts.push(format!("--{name}"));
            }
        }
        out.push((sub, opts));
    }
    out
}

/// On Windows the PowerShell script swaps in tables that match the
/// Windows help of put, list, empty, restore, status and rm.
#[test]
fn powershell_windows_tables_match_the_windows_help() {
    let ps = read("completions/rtrash.ps1");
    let win = ps
        .split_once("$winOpts = @{")
        .expect("winOpts table")
        .1
        .split_once("\n    }")
        .unwrap()
        .0
        .to_string();
    let blocks = windows_help_opts();
    assert_eq!(blocks.len(), 6, "{blocks:?}");
    let mut wrong = Vec::new();
    for (sub, opts) in &blocks {
        let words = powershell_words(&win, sub);
        for flag in opts {
            if !words.contains(flag) {
                wrong.push(format!("powershell windows {sub}: missing {flag}"));
            }
        }
        // Nothing the Windows help leaves out, such as --max-size.
        for w in words.iter().filter(|w| w.starts_with("--")) {
            if !opts.contains(w) {
                wrong.push(format!("powershell windows {sub}: extra {w}"));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
