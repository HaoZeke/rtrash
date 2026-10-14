//! `rtrash completions` and `rtrash man` on Windows, where the PowerShell
//! script is the one that matters.
#![cfg(windows)]

use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rtrash")
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)).unwrap()
}

#[test]
fn completions_and_man_print_the_embedded_files() {
    for (shell, file) in [
        ("powershell", "completions/rtrash.ps1"),
        ("pwsh", "completions/rtrash.ps1"),
        ("bash", "completions/rtrash.bash"),
    ] {
        let out = Command::new(bin())
            .args(["completions", shell])
            .output()
            .unwrap();
        assert!(out.status.success(), "completions {shell}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), read(file), "{shell}");
    }
    let out = Command::new(bin()).arg("man").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains(".TH RTRASH 1"));
    let out = Command::new(bin())
        .args(["completions", "tcsh"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

/// Long options from the option table of `rtrash SUB --help` on Windows.
fn help_long_opts(sub: &str) -> Vec<String> {
    let out = Command::new(bin()).args([sub, "--help"]).output().unwrap();
    assert!(out.status.success(), "{sub} --help");
    let mut opts = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
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
    opts
}

/// The long options in the `$winOpts` table line for `sub`.
fn table_opts(script: &str, sub: &str) -> Vec<String> {
    let win = script.split_once("$winOpts = @{").expect("winOpts").1;
    let start = format!("'{sub}' = @(");
    let line = win
        .lines()
        .map(str::trim_start)
        .find(|l| l.starts_with(&start))
        .unwrap_or_else(|| panic!("no Windows table for {sub}"));
    line[start.len()..]
        .trim_end_matches(')')
        .split(", ")
        .map(|w| w.trim_matches('\'').trim_end_matches('=').to_string())
        .filter(|w| w.starts_with("--"))
        .collect()
}

/// The PowerShell tables for Windows list exactly the options the
/// Windows build's `--help` shows, for each subcommand it implements.
#[test]
fn powershell_tables_match_the_windows_help() {
    let script = read("completions/rtrash.ps1");
    let mut wrong = Vec::new();
    for sub in ["put", "list", "empty", "restore", "status", "rm"] {
        let help = help_long_opts(sub);
        let table = table_opts(&script, sub);
        for o in &help {
            if !table.contains(o) {
                wrong.push(format!("{sub}: table misses {o}"));
            }
        }
        for o in &table {
            if !help.contains(o) {
                wrong.push(format!("{sub}: --help has no {o}"));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// PowerShell, loading the script the binary prints, completes the
/// Windows options. Windows runners ship pwsh; without it the test skips.
#[test]
fn pwsh_completes_the_windows_options() {
    let dir = std::env::temp_dir().join(format!("rtrash-pwsh-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("rtrash.ps1");
    let out = Command::new(bin())
        .args(["completions", "powershell"])
        .output()
        .unwrap();
    std::fs::write(&file, &out.stdout).unwrap();
    let prog = format!(
        ". '{}'; (TabExpansion2 -inputScript 'rtrash empty --' -cursorColumn 15).CompletionMatches.CompletionText -join ' '",
        file.display()
    );
    let run = Command::new("pwsh")
        .args(["-NoProfile", "-NonInteractive", "-Command", &prog])
        .output();
    let _ = std::fs::remove_dir_all(&dir);
    let Ok(run) = run else {
        eprintln!("pwsh not installed; skipping");
        return;
    };
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let got = String::from_utf8_lossy(&run.stdout).trim().to_string();
    // An option the help shows as `--name=VALUE` completes with its `=`.
    let help = Command::new(bin())
        .args(["empty", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&help.stdout).into_owned();
    let mut want: Vec<String> = help_long_opts("empty")
        .into_iter()
        .map(|o| {
            if help.contains(&format!("{o}=")) {
                format!("{o}=")
            } else {
                o
            }
        })
        .collect();
    want.sort();
    let mut got: Vec<String> = got.split(' ').map(str::to_string).collect();
    got.sort();
    assert_eq!(got, want);
}
