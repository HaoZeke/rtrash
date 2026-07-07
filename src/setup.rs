//! User-facing install helpers: multi-call links, completions, man page.
//!
//! Assets are embedded (`include_str!`) so a `cargo install` binary can set up
//! a full shell environment without a source checkout.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

const BASH_COMPLETION: &str = include_str!("../completions/rtrash.bash");
const ZSH_COMPLETION: &str = include_str!("../completions/_rtrash");
const FISH_COMPLETION: &str = include_str!("../completions/rtrash.fish");
const MAN_PAGE: &str = include_str!("../man/rtrash.1");

const MULTICALL: &[&str] = &[
    "trash",
    "trash-put",
    "trash-empty",
    "trash-list",
    "trash-restore",
    "trash-rm",
];

const HELP: &str = "\
Usage: rtrash setup [OPTION]...
       rtrash completions {bash|zsh|fish}
       rtrash man

After `cargo install`, run `rtrash setup` once to install multi-call
symlinks, bash/zsh/fish completions, and the man page under a user prefix
(default: ~/.local). No source tree required — assets are embedded.

setup options:
      --prefix=DIR   install root (default: $HOME/.local, or $PREFIX if set)
      --bin-dir=DIR  override binary/link directory (default: PREFIX/bin)
      --with-rm      also link `rm` → rtrash (put into trash; optional)
  -n, --dry-run      print actions without writing
  -f, --force        replace existing links/files
  -v, --verbose      print each path written
      --help         display this help and exit

completions:
  Print the embedded completion script for bash, zsh, or fish to stdout
  (for package recipes or custom paths).

man:
  Print the embedded man(1) page to stdout.
";

struct SetupOpts {
    prefix: PathBuf,
    bin_dir: Option<PathBuf>,
    with_rm: bool,
    dry_run: bool,
    force: bool,
    verbose: bool,
}

fn default_prefix() -> PathBuf {
    if let Ok(p) = env::var("PREFIX") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let home = env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(home).join(".local")
}

fn resolve_self_exe() -> io::Result<PathBuf> {
    env::current_exe()?.canonicalize()
}

fn ensure_dir(path: &Path, dry_run: bool, verbose: bool) -> io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if verbose || dry_run {
        eprintln!("mkdir -p {}", path.display());
    }
    if !dry_run {
        fs::create_dir_all(path)?;
    }
    Ok(())
}

fn write_file(path: &Path, data: &str, force: bool, dry_run: bool, verbose: bool) -> io::Result<()> {
    if path.exists() && !force {
        // Same content → success; different → error unless --force
        if let Ok(existing) = fs::read_to_string(path) {
            if existing == data {
                if verbose {
                    eprintln!("unchanged {}", path.display());
                }
                return Ok(());
            }
        }
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{} exists (use --force to replace)",
                path.display()
            ),
        ));
    }
    if verbose || dry_run {
        eprintln!("write {}", path.display());
    }
    if !dry_run {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, data)?;
    }
    Ok(())
}

fn link_multicall(
    target: &Path,
    link: &Path,
    force: bool,
    dry_run: bool,
    verbose: bool,
) -> io::Result<()> {
    if link.exists() || link.symlink_metadata().is_ok() {
        // Already points at us?
        if let Ok(dest) = fs::read_link(link) {
            let dest_ok = dest == target
                || dest
                    .canonicalize()
                    .ok()
                    .zip(target.canonicalize().ok())
                    .is_some_and(|(a, b)| a == b)
                || {
                    // relative link that resolves equal
                    link.parent()
                        .map(|p| p.join(&dest))
                        .and_then(|p| p.canonicalize().ok())
                        .zip(target.canonicalize().ok())
                        .is_some_and(|(a, b)| a == b)
                };
            if dest_ok {
                if verbose {
                    eprintln!("unchanged link {} -> {}", link.display(), dest.display());
                }
                return Ok(());
            }
        } else if link
            .canonicalize()
            .ok()
            .zip(target.canonicalize().ok())
            .is_some_and(|(a, b)| a == b)
        {
            // hardlink / same file
            if verbose {
                eprintln!("unchanged {}", link.display());
            }
            return Ok(());
        }
        if !force {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} exists and is not a link to this rtrash (use --force)",
                    link.display()
                ),
            ));
        }
        if verbose || dry_run {
            eprintln!("rm {}", link.display());
        }
        if !dry_run {
            fs::remove_file(link)?;
        }
    }
    if verbose || dry_run {
        eprintln!("ln -s {} {}", target.display(), link.display());
    }
    if !dry_run {
        if let Some(parent) = link.parent() {
            fs::create_dir_all(parent)?;
        }
        symlink(target, link)?;
    }
    Ok(())
}

fn run_setup(args: &[String]) -> i32 {
    let mut opts = SetupOpts {
        prefix: default_prefix(),
        bin_dir: None,
        with_rm: false,
        dry_run: false,
        force: false,
        verbose: false,
    };

    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return 0;
            }
            "-n" | "--dry-run" => opts.dry_run = true,
            "-f" | "--force" => opts.force = true,
            "-v" | "--verbose" => opts.verbose = true,
            "--with-rm" => opts.with_rm = true,
            a if a.starts_with("--prefix=") => {
                opts.prefix = PathBuf::from(&a["--prefix=".len()..]);
            }
            a if a.starts_with("--bin-dir=") => {
                opts.bin_dir = Some(PathBuf::from(&a["--bin-dir=".len()..]));
            }
            other => {
                eprintln!("rtrash setup: unknown option '{other}'");
                eprintln!("Try 'rtrash setup --help' for more information.");
                return 2;
            }
        }
    }

    // Default to verbose on dry-run so the plan is visible
    if opts.dry_run {
        opts.verbose = true;
    }

    let bin_dir = opts
        .bin_dir
        .clone()
        .unwrap_or_else(|| opts.prefix.join("bin"));
    let share = opts.prefix.join("share");
    let bash_dir = share.join("bash-completion/completions");
    let zsh_dir = share.join("zsh/site-functions");
    // Fish loads user completions from $XDG_CONFIG_HOME/fish/completions or
    // ~/.config/fish/completions. Prefer that under HOME when prefix is the
    // default ~/.local; otherwise PREFIX/share/fish/vendor_completions.d.
    let fish_dir = fish_completions_dir(&opts.prefix);
    let man_dir = share.join("man/man1");

    let target = match resolve_self_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("rtrash setup: cannot resolve own path: {e}");
            return 1;
        }
    };

    fn absorb(r: io::Result<()>) -> bool {
        match r {
            Ok(()) => true,
            Err(e) => {
                eprintln!("rtrash setup: {e}");
                false
            }
        }
    }

    let mut ok = true;
    ok &= absorb(ensure_dir(&bin_dir, opts.dry_run, opts.verbose));
    for name in MULTICALL {
        ok &= absorb(link_multicall(
            &target,
            &bin_dir.join(name),
            opts.force,
            opts.dry_run,
            opts.verbose,
        ));
    }
    if opts.with_rm {
        ok &= absorb(link_multicall(
            &target,
            &bin_dir.join("rm"),
            opts.force,
            opts.dry_run,
            opts.verbose,
        ));
    }

    ok &= absorb(ensure_dir(&bash_dir, opts.dry_run, opts.verbose));
    ok &= absorb(write_file(
        &bash_dir.join("rtrash"),
        BASH_COMPLETION,
        opts.force,
        opts.dry_run,
        opts.verbose,
    ));
    // bash-completion looks up by command name; multi-call names may need
    // the same script. Symlink them to rtrash when possible.
    for name in [
        "trash-put",
        "trash-empty",
        "trash-list",
        "trash-restore",
        "trash-rm",
        "trash",
    ] {
        let link = bash_dir.join(name);
        let dest = PathBuf::from("rtrash");
        if link.symlink_metadata().is_ok() {
            if opts.force {
                if opts.verbose || opts.dry_run {
                    eprintln!("rm {}", link.display());
                }
                if !opts.dry_run {
                    ok &= absorb(fs::remove_file(&link));
                    if !ok {
                        continue;
                    }
                }
            } else {
                continue;
            }
        }
        if opts.verbose || opts.dry_run {
            eprintln!("ln -s rtrash {}", link.display());
        }
        if !opts.dry_run {
            match symlink(&dest, &link) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    eprintln!("rtrash setup: {}: {e}", link.display());
                    ok = false;
                }
            }
        }
    }

    ok &= absorb(ensure_dir(&zsh_dir, opts.dry_run, opts.verbose));
    ok &= absorb(write_file(
        &zsh_dir.join("_rtrash"),
        ZSH_COMPLETION,
        opts.force,
        opts.dry_run,
        opts.verbose,
    ));

    ok &= absorb(ensure_dir(&fish_dir, opts.dry_run, opts.verbose));
    ok &= absorb(write_file(
        &fish_dir.join("rtrash.fish"),
        FISH_COMPLETION,
        opts.force,
        opts.dry_run,
        opts.verbose,
    ));
    // Fish only auto-loads `completions/<command>.fish` for that command name.
    // The shared script also defines multi-call completes; symlink it under each
    // multi-call name so `trash-put` etc. load completions without shadowing `rm`.
    for name in MULTICALL {
        let link = fish_dir.join(format!("{name}.fish"));
        let dest = PathBuf::from("rtrash.fish");
        if link.symlink_metadata().is_ok() || link.exists() {
            if opts.force {
                if opts.verbose || opts.dry_run {
                    eprintln!("rm {}", link.display());
                }
                if !opts.dry_run {
                    ok &= absorb(fs::remove_file(&link));
                    if !ok {
                        continue;
                    }
                }
            } else {
                continue;
            }
        }
        if opts.verbose || opts.dry_run {
            eprintln!("ln -s rtrash.fish {}", link.display());
        }
        if !opts.dry_run {
            match symlink(&dest, &link) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    eprintln!("rtrash setup: {}: {e}", link.display());
                    ok = false;
                }
            }
        }
    }

    ok &= absorb(ensure_dir(&man_dir, opts.dry_run, opts.verbose));
    ok &= absorb(write_file(
        &man_dir.join("rtrash.1"),
        MAN_PAGE,
        opts.force,
        opts.dry_run,
        opts.verbose,
    ));

    if !ok {
        return 1;
    }

    let prefix = opts.prefix.display();
    let manpath = share.join("man");
    println!("rtrash setup complete under {prefix}");
    println!();
    println!("Installed:");
    println!("  multi-call links → {}", bin_dir.display());
    println!("  bash completion  → {}/rtrash", bash_dir.display());
    println!("  zsh completion   → {}/_rtrash", zsh_dir.display());
    println!(
        "  fish completion  → {}/rtrash.fish (+ multi-call *.fish links)",
        fish_dir.display()
    );
    println!("  man page         → {}/rtrash.1", man_dir.display());
    println!();
    println!("Shell notes (once per machine/login config):");
    println!("  • PATH must include {}", bin_dir.display());
    println!("    (cargo install already uses ~/.cargo/bin; multi-call names");
    println!("    live under the prefix bin dir above — add it if needed.)");
    println!("  • bash: bash-completion loads");
    println!("    $XDG_DATA_HOME/bash-completion/completions and");
    println!("    ~/.local/share/bash-completion/completions automatically");
    println!("    on most setups that enable bash-completion.");
    println!("  • zsh: ensure the site-functions dir is on fpath, e.g. in ~/.zshrc:");
    println!("      fpath=({}/zsh/site-functions $fpath)", share.display());
    println!("      autoload -Uz compinit && compinit");
    println!("  • fish: user completions are loaded from");
    println!("    ~/.config/fish/completions (default setup target) or");
    println!("    PREFIX/share/fish/vendor_completions.d when --prefix is not ~/.local.");
    println!("  • man: if `man rtrash` misses the page, set:");
    println!(
        "      export MANPATH=\"{}:${{MANPATH:-}}\"",
        manpath.display()
    );
    if !opts.with_rm {
        println!();
        println!("Optional: rtrash setup --with-rm   # also link rm → put into trash");
    }
    println!();
    println!("Re-run with --force after upgrades to refresh completions/man.");
    0
}

/// Prefer fish's user config completions dir for the default user prefix.
fn fish_completions_dir(prefix: &Path) -> PathBuf {
    let home = env::var_os("HOME").map(PathBuf::from);
    let default_local = home.as_ref().map(|h| h.join(".local"));
    if default_local.as_ref().is_some_and(|p| p == prefix) {
        let cfg = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.map(|h| h.join(".config")));
        if let Some(c) = cfg {
            return c.join("fish/completions");
        }
    }
    prefix.join("share/fish/vendor_completions.d")
}

fn run_completions(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("-h" | "--help") | None => {
            eprint!("{HELP}");
            i32::from(args.is_empty())
        }
        Some("bash") => {
            print!("{BASH_COMPLETION}");
            0
        }
        Some("zsh") => {
            print!("{ZSH_COMPLETION}");
            0
        }
        Some("fish") => {
            print!("{FISH_COMPLETION}");
            0
        }
        Some(other) => {
            eprintln!("rtrash completions: unknown shell '{other}' (want bash, zsh, or fish)");
            2
        }
    }
}

fn run_man(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("-h" | "--help") => {
            print!("{HELP}");
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

/// Dispatch `setup` / `completions` / `man` subcommands.
pub fn run(prog_cmd: &str, args: &[String]) -> i32 {
    match prog_cmd {
        "setup" => run_setup(args),
        "completions" => run_completions(args),
        "man" => run_man(args),
        _ => {
            eprintln!("rtrash: internal setup dispatch error for '{prog_cmd}'");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp_prefix() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = env::temp_dir().join(format!("rtrash-setup-test-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn embedded_assets_nonempty() {
        assert!(BASH_COMPLETION.contains("complete -F _rtrash_main rtrash"));
        assert!(ZSH_COMPLETION.contains("#compdef"));
        assert!(FISH_COMPLETION.contains("complete -c rtrash"));
        assert!(FISH_COMPLETION.contains("home-only"));
        assert!(FISH_COMPLETION.contains("dry-run"));
        assert!(FISH_COMPLETION.contains("put"));
        assert!(MAN_PAGE.contains(".TH RTRASH 1"));
    }

    #[test]
    fn setup_dry_run_prefix_ok() {
        let prefix = tmp_prefix();
        let code = run_setup(&[
            format!("--prefix={}", prefix.display()),
            "--dry-run".into(),
        ]);
        assert_eq!(code, 0);
        assert!(!prefix.exists() || fs::read_dir(&prefix).map(|d| d.count()).unwrap_or(0) == 0);
    }

    #[test]
    fn setup_writes_assets() {
        let prefix = tmp_prefix();
        // Use a fake "self" via bin-dir only for links — real current_exe is fine
        let code = run_setup(&[
            format!("--prefix={}", prefix.display()),
            "-f".into(),
            "-v".into(),
        ]);
        assert_eq!(code, 0, "setup failed");
        assert!(prefix.join("share/bash-completion/completions/rtrash").is_file());
        assert!(prefix.join("share/zsh/site-functions/_rtrash").is_file());
        let fish_main = prefix.join("share/fish/vendor_completions.d/rtrash.fish");
        assert!(fish_main.is_file());
        assert!(prefix.join("share/man/man1/rtrash.1").is_file());
        let bash = fs::read_to_string(prefix.join("share/bash-completion/completions/rtrash")).unwrap();
        assert!(bash.contains("--home-only"));
        let fish = fs::read_to_string(&fish_main).unwrap();
        assert!(fish.contains("dry-run"));
        assert!(fish.contains("home-only"));
        // Fish autoloads by command name: multi-call links must exist.
        for name in MULTICALL {
            let p = prefix.join(format!("share/fish/vendor_completions.d/{name}.fish"));
            assert!(
                p.symlink_metadata().is_ok() || p.is_file(),
                "missing fish multi-call completion {p:?}"
            );
            // Symlink (or same content) must resolve to the shared script.
            if p.symlink_metadata().ok().is_some_and(|m| m.file_type().is_symlink()) {
                let dest = fs::read_link(&p).unwrap();
                assert_eq!(dest.as_os_str(), "rtrash.fish");
            }
        }
        let _ = fs::remove_dir_all(&prefix);
    }
}
