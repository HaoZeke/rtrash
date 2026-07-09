#[cfg(all(unix, feature = "tui"))]
use rtrash::tui_binds;
use rtrash::{empty, list, put, restore, rm, setup, status};

const HELP: &str = "\
Usage: rtrash <COMMAND> [ARGS]...
Fast trash tool: FreeDesktop on Linux (and experimental macOS home trash);
Windows uses the system Recycle Bin. rm-compatible put interface.

Commands:
  put [OPTION]... FILE...     move files to the trash (accepts rm(1) flags)
  empty [OPTION]... [DAYS]    purge trashed items, optionally older than DAYS
  list [OPTION]...            list trashed items
  status [OPTION]...          item count and reclaimable size summary
  restore [OPTION]... [PATH]  restore a trashed item
  rm PATTERN...               permanently delete matching trash entries
  setup [OPTION]...           install multi-call links, completions, man page
  keys [OPTION]...            show/sample TUI keybind config (customizable)
  completions {bash|zsh|fish} print embedded shell completion script
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
    let args: Vec<String> = std::env::args().collect();
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
        _ => match rest.first().map(String::as_str) {
            Some("put") => put::run(&argv0, &rest[1..]),
            Some("empty") => empty::run(&argv0, &rest[1..]),
            Some("list") => list::run(&argv0, &rest[1..]),
            Some("status") => status::run(&argv0, &rest[1..]),
            Some("restore") => restore::run(&argv0, &rest[1..]),
            Some("rm") => rm::run(&argv0, &rest[1..]),
            Some("setup") => setup::run("setup", &rest[1..]),
            #[cfg(all(unix, feature = "tui"))]
            Some("keys") => tui_binds::run_cli(&argv0, &rest[1..]),
            #[cfg(not(all(unix, feature = "tui")))]
            Some("keys") => {
                eprintln!("{argv0}: keys requires the TUI feature on Unix");
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
