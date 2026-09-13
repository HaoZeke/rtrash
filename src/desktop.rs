//! `rtrash desktop` — exec the separate `rtrash-hud` binary.
//!
//! The musl CLI stays thin: this module never depends on iced. The HUD is a
//! glibc workspace crate (`crates/rtrash-hud`) and is not in the musl tarball.

use std::env;
use std::path::PathBuf;

/// Env override for the HUD binary this launcher execs.
pub const HUD_BIN_ENV: &str = "RTRASH_HUD_BIN";

const HELP: &str = "\
Usage: rtrash desktop [OPTION]...
Launch the rtrash-hud icedtea desktop HUD.

The HUD is a separate glibc binary (crates/rtrash-hud, icedtea 0.17).
It is not in the musl CLI tarball. This command execs `rtrash-hud`
(sibling of this binary, $RTRASH_HUD_BIN, or PATH) and forwards flags.

  --offline            paint the catalog without a compositor (no TTY)
  --trash-dir PATH     pin trash roots (repeatable; --trash-dir=PATH ok)
  --home-only          home trash only
  --show / --hide / --toggle
                       talk to a running HUD (hide-miss exits 0)
  --install-desktop    write .desktop + Sway overlay rules
  --foreground         stay on the terminal (HUD default)
  -h, --help           show this help
  -V, --version        print the HUD version via the HUD binary

Examples:
  rtrash desktop --help
  rtrash desktop --offline --trash-dir=/tmp/fixture
";

/// Locate `rtrash-hud`. Missing means exit 127.
pub fn resolve_hud_bin() -> Option<PathBuf> {
    if let Ok(raw) = env::var(HUD_BIN_ENV) {
        let t = raw.trim();
        if !t.is_empty() {
            return Some(PathBuf::from(t));
        }
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("rtrash-hud");
            if sibling.is_file() {
                return Some(sibling);
            }
        }
    }
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join("rtrash-hud");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Hint printed when the launcher cannot find the HUD binary.
pub fn missing_bin_message() -> &'static str {
    "rtrash-hud is not installed. Install it with:\n  cargo install --path crates/rtrash-hud"
}

/// Dispatch `rtrash desktop`. `--help` is answered here so a missing HUD
/// still names the desktop window.
pub fn run(prog: &str, args: &[String]) -> i32 {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{HELP}");
        return 0;
    }
    // Hide with no HUD binary is success (compositor bind miss).
    if args.iter().any(|a| a == "--hide")
        && !resolve_hud_bin().is_some_and(|p| p.is_file())
    {
        return 0;
    }
    let Some(bin) = resolve_hud_bin().filter(|p| p.is_file()) else {
        eprintln!("{prog}: {}", missing_bin_message());
        return 127;
    };
    let mut cmd = std::process::Command::new(&bin);
    cmd.args(args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        eprintln!("{prog}: exec {}: {err}", bin.display());
        1
    }
    #[cfg(not(unix))]
    {
        match cmd.status() {
            Ok(st) => st.code().unwrap_or(1),
            Err(e) => {
                eprintln!("{prog}: spawn {}: {e}", bin.display());
                1
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_text_names_the_hud() {
        assert!(HELP.contains("rtrash-hud"));
        assert!(HELP.contains("icedtea"));
        assert!(HELP.contains("--offline"));
        assert!(missing_bin_message().contains("rtrash-hud"));
    }
}
