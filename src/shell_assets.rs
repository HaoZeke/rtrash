//! Completion scripts and the man page, embedded so `rtrash completions`
//! and `rtrash man` work on every platform without a source checkout.

use std::io::{self, Write};

pub const BASH_COMPLETION: &str = include_str!("../completions/rtrash.bash");
pub const ZSH_COMPLETION: &str = include_str!("../completions/_rtrash");
pub const FISH_COMPLETION: &str = include_str!("../completions/rtrash.fish");
pub const NU_COMPLETION: &str = include_str!("../completions/rtrash.nu");
pub const POWERSHELL_COMPLETION: &str = include_str!("../completions/rtrash.ps1");
pub const ELVISH_COMPLETION: &str = include_str!("../completions/rtrash.elv");
pub const MAN_PAGE: &str = include_str!("../man/rtrash.1");

/// `rtrash completions SHELL`: print that shell's script. `help` is the
/// text for `--help` or a missing SHELL.
pub fn run_completions(args: &[String], help: &str) -> i32 {
    let script = match args.first().map(String::as_str) {
        Some("-h" | "--help") | None => {
            eprint!("{help}");
            return i32::from(args.is_empty());
        }
        Some("bash") => BASH_COMPLETION,
        Some("zsh") => ZSH_COMPLETION,
        Some("fish") => FISH_COMPLETION,
        Some("nu") => NU_COMPLETION,
        Some("powershell" | "pwsh") => POWERSHELL_COMPLETION,
        Some("elvish") => ELVISH_COMPLETION,
        Some(other) => {
            eprintln!(
                "rtrash completions: unknown shell '{other}' (want bash, zsh, fish, nu, powershell or elvish)"
            );
            return 2;
        }
    };
    print!("{script}");
    0
}

/// `rtrash man`: print the man page source.
pub fn run_man(args: &[String], help: &str) -> i32 {
    match args.first().map(String::as_str) {
        Some("-h" | "--help") => {
            print!("{help}");
            0
        }
        Some(other) => {
            eprintln!("rtrash man: unexpected argument '{other}'");
            2
        }
        None => {
            let mut out = io::stdout().lock();
            if let Err(e) = out.write_all(MAN_PAGE.as_bytes()) {
                eprintln!("rtrash man: {e}");
                return 1;
            }
            0
        }
    }
}
