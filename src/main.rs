mod empty;
mod info;
mod list;
mod put;
mod restore;
mod trashdir;
mod util;

const HELP: &str = "\
Usage: rtrash <COMMAND> [ARGS]...
Fast freedesktop.org trash tool with an rm-compatible interface.

Commands:
  put [OPTION]... FILE...   move files to the trash (accepts rm(1) flags)
  empty [OPTION]... [DAYS]  purge trashed items, optionally older than DAYS
  list                      list trashed items
  restore [PATH]            restore a trashed item

Multi-call: a symlink or hardlink named rm or trash-put runs `put`;
trash-empty runs `empty`; trash-list runs `list`; trash-restore runs
`restore`. Anything else (e.g. `rtrash -rf dir`) falls through to `put`.

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
        "rm" | "trash-put" | "trash" => put::run(&argv0, rest),
        _ => match rest.first().map(String::as_str) {
            Some("put") => put::run(&argv0, &rest[1..]),
            Some("empty") => empty::run(&argv0, &rest[1..]),
            Some("list") => list::run(&argv0, &rest[1..]),
            Some("restore") => restore::run(&argv0, &rest[1..]),
            Some("-h") | Some("--help") | Some("help") | None => {
                print!("{HELP}");
                i32::from(rest.is_empty())
            }
            Some("-V") | Some("--version") => {
                println!("rtrash {}", env!("CARGO_PKG_VERSION"));
                0
            }
            // rm-style direct invocation: `rtrash -rf dir`, `rtrash file`.
            Some(_) => put::run(&argv0, rest),
        },
    };
    std::process::exit(code);
}
