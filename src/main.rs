// musl's malloc takes a global lock; mimalloc keeps the parallel walk fast.
#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "tui")]
use rtrash::tui_binds;
use rtrash::{
    config, du, empty, finder, list, put, reap, restore, rm, setup, status, trash_stats, undo, who,
};

const HELP: &str = "\
Usage: rtrash <COMMAND> [ARGS]...
Fast trash tool: FreeDesktop on Linux (and experimental macOS home trash);
Windows uses the system Recycle Bin. rm-compatible put interface.

Commands:
  put [OPTION]... FILE...     move files to the trash (accepts rm(1) flags)
  empty [OPTION]... [DAYS]    purge trashed items, optionally older than DAYS
  list [OPTION]...            list trashed items
  status [OPTION]...          item count and reclaimable size summary
  trash-stats [OPTION]...     per-trash size, inodes, oldest and largest items
  du [OPTION]... [PATH]...    find the largest directories and files to trash
  restore [OPTION]... [PATH]  restore a trashed item
  undo [OPTION]... [BATCH]    restore everything the last (or BATCH) put run trashed
  rm PATTERN...               permanently delete matching trash entries
  who [OPTION]... PATH...     list the live processes holding PATH
  reap [OPTION]... [DIR]...   trash stale temp entries no live process holds
  finder put|list|restore     macOS Finder Trash with Put Back
  config [--path|--default]   show the config file and the settings it makes
  setup [OPTION]...           install multi-call links, completions, man page
  keys [OPTION]...            show/sample TUI keybind config (customizable)
  completions SHELL           print embedded bash, zsh, fish or nu completion script
  man                         print embedded man(1) page to stdout

Multi-call: a symlink or hardlink named rm or trash-put runs `put`;
trash-empty runs `empty`; trash-list runs `list`; trash-restore runs
`restore`; trash-rm runs selective permanent delete. Anything else
(e.g. `rtrash -rf dir`) falls through to `put`.

Most suite commands accept --home-only (home trash only) and
--trash-dir=PATH (repeatable pin).

After cargo install, run:  rtrash setup

  -h, --help     display this help and exit
  -V, --version  output version information and exit
";

fn main() {
    // args() panics on a name that is not UTF-8; carry its bytes instead.
    let args: Vec<String> = std::env::args_os().map(rtrash::util::str_from_os).collect();
    let argv0 = std::path::Path::new(args.first().map(String::as_str).unwrap_or("rtrash"))
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rtrash".to_string());
    let rest = &args[1..];

    let code = match argv0.as_str() {
        "trash-empty" => empty::run(&argv0, rest),
        "trash-list" => list::run(&argv0, rest),
        "trash-restore" => restore::run(&argv0, rest),
        "trash-rm" => rm::run(&argv0, rest),
        "rm" | "trash-put" | "trash" => put::run(&argv0, rest),
        // Subcommands name themselves in usage lines and messages
        // (`rtrash du --help`), so a hint points at the right help.
        _ => match rest.first().map(String::as_str) {
            Some("put") => put::run(&argv0, &rest[1..]),
            Some("empty") => empty::run(&format!("{argv0} empty"), &rest[1..]),
            Some("list") => list::run(&format!("{argv0} list"), &rest[1..]),
            Some("status") => status::run(&format!("{argv0} status"), &rest[1..]),
            Some("trash-stats") => trash_stats::run(&format!("{argv0} trash-stats"), &rest[1..]),
            Some("du") => du::run(&format!("{argv0} du"), &rest[1..]),
            Some("restore") => restore::run(&format!("{argv0} restore"), &rest[1..]),
            Some("rm") => rm::run(&format!("{argv0} rm"), &rest[1..]),
            Some("undo") => undo::run(&format!("{argv0} undo"), &rest[1..]),
            Some("who") => who::run(&format!("{argv0} who"), &rest[1..]),
            Some("reap") => reap::run(&format!("{argv0} reap"), &rest[1..]),
            Some("finder") => finder::run(&format!("{argv0} finder"), &rest[1..]),
            Some("config") => config::run(&format!("{argv0} config"), &rest[1..]),
            Some("setup") => setup::run("setup", &rest[1..]),
            #[cfg(feature = "tui")]
            Some("keys") => tui_binds::run_cli(&argv0, &rest[1..]),
            #[cfg(not(feature = "tui"))]
            Some("keys") => {
                eprintln!("{argv0}: keys requires the TUI feature");
                1
            }
            Some("completions") => setup::run("completions", &rest[1..]),
            Some("man") => setup::run("man", &rest[1..]),
            Some("-h") | Some("--help") | Some("help") | None => {
                print!("{HELP}");
                i32::from(rest.is_empty())
            }
            Some("-V") | Some("--version") => {
                println!("rtrash {}", rtrash::version());
                0
            }
            // rm-style direct invocation: `rtrash -rf dir`, `rtrash file`.
            Some(_) => put::run(&argv0, rest),
        },
    };
    std::process::exit(code);
}
