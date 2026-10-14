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

/// First line of the block `setup --powershell-profile` adds; its presence
/// means the block is there already.
pub const PROFILE_MARK: &str = "# rtrash completions (added by rtrash setup --powershell-profile)";

/// The block for a PowerShell profile: load the completer when rtrash is on
/// PATH, so a profile shared with machines without rtrash stays quiet.
pub fn profile_block() -> String {
    format!(
        "{PROFILE_MARK}\nif (Get-Command rtrash -ErrorAction SilentlyContinue) {{\n    rtrash completions powershell | Out-String | Invoke-Expression\n}}\n"
    )
}

/// The current user's profile for the current host, as `pwsh` (then
/// Windows PowerShell) reports `$PROFILE.CurrentUserCurrentHost`.
pub fn powershell_profile_path() -> Option<std::path::PathBuf> {
    for shell in ["pwsh", "powershell"] {
        let Ok(out) = std::process::Command::new(shell)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$PROFILE.CurrentUserCurrentHost",
            ])
            .output()
        else {
            continue;
        };
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && !path.is_empty() {
            return Some(path.into());
        }
    }
    None
}

/// Append [`profile_block`] to `profile`, creating it and its directory as
/// needed. `Ok(false)` when the block is there already (nothing written).
pub fn add_to_powershell_profile(profile: &std::path::Path, dry_run: bool) -> io::Result<bool> {
    let old = match std::fs::read_to_string(profile) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    if old.lines().any(|l| l.trim_end() == PROFILE_MARK) {
        return Ok(false);
    }
    if dry_run {
        return Ok(true);
    }
    if let Some(dir) = profile.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let sep = if old.is_empty() || old.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(profile)?;
    f.write_all(format!("{sep}{}", profile_block()).as_bytes())?;
    Ok(true)
}

/// `setup --powershell-profile[=PATH]`: add the block to PATH, or to the
/// profile PowerShell reports. Prints what it did; returns the exit code.
pub fn setup_powershell_profile(arg: Option<&str>, dry_run: bool) -> i32 {
    let path = match arg {
        Some(p) => std::path::PathBuf::from(p),
        None => match powershell_profile_path() {
            Some(p) => p,
            None => {
                eprintln!(
                    "rtrash setup: neither pwsh nor powershell reported a profile; pass --powershell-profile=PATH"
                );
                return 1;
            }
        },
    };
    match add_to_powershell_profile(&path, dry_run) {
        Ok(true) if dry_run => {
            println!("would add the rtrash completer to {}", path.display());
            0
        }
        Ok(true) => {
            println!("added the rtrash completer to {}", path.display());
            0
        }
        Ok(false) => {
            println!("{} loads the rtrash completer already", path.display());
            0
        }
        Err(e) => {
            eprintln!("rtrash setup: {}: {e}", path.display());
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_block_is_added_once_and_keeps_what_was_there() {
        let dir = std::env::temp_dir().join(format!("rtrash-profile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let profile = dir.join("sub").join("Microsoft.PowerShell_profile.ps1");
        assert!(add_to_powershell_profile(&profile, true).unwrap());
        assert!(!profile.exists(), "a dry run wrote the profile");
        std::fs::create_dir_all(profile.parent().unwrap()).unwrap();
        std::fs::write(&profile, "Set-Alias ll Get-ChildItem").unwrap();
        assert!(add_to_powershell_profile(&profile, false).unwrap());
        assert!(!add_to_powershell_profile(&profile, false).unwrap());
        let text = std::fs::read_to_string(&profile).unwrap();
        assert!(
            text.starts_with("Set-Alias ll Get-ChildItem\n# rtrash"),
            "{text}"
        );
        assert_eq!(text.matches(PROFILE_MARK).count(), 1, "{text}");
        assert!(text.contains("rtrash completions powershell | Out-String | Invoke-Expression"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
