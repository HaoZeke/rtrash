//! Process flags for `rtrash-hud` and the `rtrash desktop` launcher.

use std::path::PathBuf;

use crate::app;
use crate::catalog;
use crate::install_desktop;
use crate::summon::{self, SummonAction, SummonCli};

const HELP: &str = "\
Usage: rtrash-hud [OPTION]...
icedtea desktop HUD for rtrash (list / restore / empty / put / status).

Calls list::collect, restore, empty, and put library APIs. Does not spawn
the CLI for the happy path. glibc only; not in the musl binstall tarball.

  --offline            paint the catalog without a compositor (no TTY)
  --trash-dir PATH     pin trash roots (repeatable; --trash-dir=PATH ok)
  --home-only          home trash only
  --show / --hide / --toggle
                       talk to a running HUD (hide-miss exits 0)
  --install-desktop    write .desktop + Sway overlay rules
  --foreground         stay on the terminal (default)
  -h, --help           show this help
  -V, --version        print the product version
";

/// Parsed HUD flags.
#[derive(Debug, Clone, Default)]
pub struct HudCli {
    /// Never open a window; paint the catalog on stdout.
    pub offline: bool,
    /// Stay attached (default). Kept for launcher parity with vissue-hud.
    pub foreground: bool,
    /// Home trash only.
    pub home_only: bool,
    /// Explicit FreeDesktop trash roots.
    pub trash_dirs: Vec<PathBuf>,
    /// Summon verb from the command line.
    pub summon: Option<SummonAction>,
    /// Write desktop files and exit.
    pub install_desktop: bool,
    /// Start with the overlay mapped.
    pub visible: bool,
}

impl HudCli {
    /// Parse argv after the program name.
    pub fn parse_args(args: &[String]) -> Result<Self, String> {
        let mut cli = HudCli {
            foreground: true,
            ..HudCli::default()
        };
        let mut i = 0usize;
        while i < args.len() {
            let arg = &args[i];
            match arg.as_str() {
                "-h" | "--help" => return Err("help".into()),
                "-V" | "--version" => return Err("version".into()),
                "--offline" => cli.offline = true,
                "--foreground" => cli.foreground = true,
                "--home-only" => cli.home_only = true,
                "--show" => cli.summon = Some(SummonAction::Show),
                "--hide" => cli.summon = Some(SummonAction::Hide),
                "--toggle" => cli.summon = Some(SummonAction::Toggle),
                "--install-desktop" => cli.install_desktop = true,
                a if rtrash::util::is_trash_dir_flag(a) => {
                    match rtrash::util::take_trash_dir_value(args, &mut i) {
                        Ok(p) => cli.trash_dirs.push(p),
                        Err(e) => return Err(e.to_string()),
                    }
                }
                a => return Err(format!("unrecognized option '{a}'")),
            }
            i += 1;
        }
        Ok(cli)
    }
}

/// Parse process args and run: offline paint or the iced loop.
pub fn run_cli() -> Result<i32, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    run_with_args(&args)
}

/// Run already-collected flags (tests).
pub fn run_with_args(args: &[String]) -> Result<i32, String> {
    match HudCli::parse_args(args) {
        Err(tag) if tag == "help" => {
            print!("{HELP}");
            Ok(0)
        }
        Err(tag) if tag == "version" => {
            println!("rtrash-hud {}", rtrash::version());
            Ok(0)
        }
        Err(e) => {
            eprintln!("rtrash-hud: {e}");
            eprintln!("Try 'rtrash-hud --help' for more information.");
            Ok(2)
        }
        Ok(cli) if cli.install_desktop => install_desktop::run_cli()
            .map(|_| 0)
            .map_err(|e| e.to_string()),
        Ok(cli) if cli.offline => Ok(catalog::paint_offline(&cli.trash_dirs, cli.home_only)),
        Ok(cli) => dispatch_live(cli),
    }
}

fn dispatch_live(mut cli: HudCli) -> Result<i32, String> {
    if let Some(action) = cli.summon {
        match summon::plan_summon_cli(action, summon::send_command(action)) {
            Ok(SummonCli::Done) => return Ok(0),
            Ok(SummonCli::StartShown) => cli.visible = true,
            Err(err) => {
                eprintln!("rtrash-hud: {err}");
                return Ok(1);
            }
        }
    } else {
        cli.visible = true;
    }
    app::run(cli).map(|()| 0).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_offline_and_trash_dir() {
        let cli = HudCli::parse_args(&[
            "--offline".into(),
            "--trash-dir=/tmp/fixture".into(),
            "--home-only".into(),
        ])
        .unwrap();
        assert!(cli.offline);
        assert!(cli.home_only);
        assert_eq!(cli.trash_dirs, vec![PathBuf::from("/tmp/fixture")]);
    }

    #[test]
    fn help_names_panes_and_offline() {
        assert!(HELP.contains("list / restore / empty / put / status"));
        assert!(HELP.contains("--offline"));
        assert!(HELP.contains("icedtea"));
        assert!(HELP.contains("--toggle"));
        assert!(HELP.contains("--install-desktop"));
    }

    #[test]
    fn hide_without_socket_is_zero() {
        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var(
                "RTRASH_HUD_SOCKET",
                format!("/tmp/rtrash-hud-none-{}", std::process::id()),
            );
        }
        let code = run_with_args(&["--hide".into()]).unwrap();
        assert_eq!(code, 0);
    }
}
