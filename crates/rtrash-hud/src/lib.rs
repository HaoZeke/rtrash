//! Trash size and item count for a desktop status display.
//!
//! The numbers come from [`rtrash::status::summarize_dirs_readonly`], the
//! same count `rtrash status` prints. This crate does not read
//! `.trashinfo` files, and it does not lock or rewrite a trash.

use std::path::PathBuf;

use rtrash::util::{format_bytes, json_escape, plural, term_path};

const HELP: &str = "\
Usage: rtrash-hud [OPTION]...
Show each trash's item count and reclaimable size, the same numbers as
`rtrash status`, and exit. The count comes from the rtrash library; this
program does not parse .trashinfo files. It does not take the trash lock
or rewrite directorysizes. It has no age filter (`rtrash status
--older-than` does).

With no --trash-dir, discovery matches `rtrash status`. On macOS, where
the Finder Trash is the default, pass --trash-dir or set
RTRASH_MACOS_TRASH=freedesktop (macos.trash = \"freedesktop\" in the
config). This display does not read the Finder Trash or the Recycle Bin.

      --trash-dir=PATH  only this trash directory (repeatable);
                     --trash-dir PATH is accepted too
      --home-only       only the home trash, when no --trash-dir is given
      --json            one JSON object: items, bytes, and roots
                     (path, items, bytes, orphans, dangling). These are
                     not the status --json names (total_items,
                     total_bytes, and the fs_* fields)
  -h, --help            display this help and exit
  -V, --version         output version information and exit
";

/// The two lines `rtrash status` writes to stderr when it finds no trash.
const NO_TRASH: &str = "No trash directories found\nTotal: 0 items (0 B)\n";

/// One trash root in a [`Snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootTotal {
    /// Trash directory the library resolved.
    pub root: PathBuf,
    /// Items, orphans included.
    pub items: u64,
    /// Reclaimable size in bytes, as `rtrash status` counts it.
    pub bytes: u64,
    /// Of `items`, payloads with no `.trashinfo`.
    pub orphans: u64,
    /// Of `items`, records whose payload is gone.
    pub dangling: u64,
}

/// Item count and reclaimable size of the trashes a run asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// One row per trash, in discovery order.
    pub roots: Vec<RootTotal>,
    /// Sum of [`RootTotal::items`].
    pub items: u64,
    /// Sum of [`RootTotal::bytes`].
    pub bytes: u64,
}

/// Parsed flags for one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// Explicit trash roots. Empty means the usual discovery.
    pub pins: Vec<PathBuf>,
    /// Home trash only, when `pins` is empty.
    pub home_only: bool,
    /// Print JSON instead of the text table.
    pub json: bool,
}

/// What [`parse`] decided.
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    /// Run the display.
    Run(Args),
    /// Print the help and exit 0.
    Help,
    /// Print the version and exit 0.
    Version,
}

/// Parse argv after the program name.
pub fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut pins = Vec::new();
    let mut home_only = false;
    let mut json = false;
    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            "--json" => json = true,
            "--home-only" => home_only = true,
            "--trash-dir" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    return Err("--trash-dir requires a PATH".into());
                };
                pins.push(rtrash::util::arg_path(v));
            }
            a if a.starts_with("--trash-dir=") => {
                pins.push(rtrash::util::arg_path(&a["--trash-dir=".len()..]));
            }
            a => return Err(format!("unrecognized option '{a}'")),
        }
        i += 1;
    }
    Ok(Parsed::Run(Args {
        pins,
        home_only,
        json,
    }))
}

/// The totals of `pins` (or the usual discovery when `pins` is empty).
///
/// On a system without a FreeDesktop trash, this is an error. Where the
/// Finder Trash is the default and `pins` is empty, this is an error too:
/// the Finder numbers are not these numbers.
pub fn snapshot(pins: &[PathBuf], home_only: bool) -> Result<Snapshot, String> {
    #[cfg(unix)]
    {
        load(pins, home_only)
    }
    #[cfg(not(unix))]
    {
        let _ = (pins, home_only);
        Err("rtrash-hud reads a FreeDesktop trash; this system does not have one".into())
    }
}

#[cfg(unix)]
fn load(pins: &[PathBuf], home_only: bool) -> Result<Snapshot, String> {
    if rtrash::finder::is_default(!pins.is_empty()) {
        return Err(concat!(
            "the Finder Trash is the default here; ",
            "pin --trash-dir or set RTRASH_MACOS_TRASH=freedesktop ",
            "(or macos.trash = \"freedesktop\")"
        )
        .into());
    }
    let dirs = rtrash::trashdir::resolve_dirs_opts(pins, home_only);
    if dirs.is_empty() && !pins.is_empty() {
        return Err("no valid --trash-dir pins".into());
    }
    let (rows, items, bytes) = rtrash::status::summarize_dirs_readonly(
        &dirs,
        None,
        None,
        rtrash::list::SizeFilter::default(),
    );
    if rows.len() != dirs.len() {
        return Err("status summary did not cover every trash".into());
    }
    let roots = dirs
        .iter()
        .zip(rows)
        .map(|(dir, row)| RootTotal {
            root: dir.root.clone(),
            items: row.items,
            bytes: row.bytes,
            orphans: row.orphans,
            dangling: row.dangling,
        })
        .collect();
    Ok(Snapshot {
        roots,
        items,
        bytes,
    })
}

fn notes(row: &RootTotal) -> String {
    let mut parts = Vec::new();
    if row.orphans > 0 {
        parts.push(format!("{} without .trashinfo", row.orphans));
    }
    if row.dangling > 0 {
        parts.push(format!("{} without a payload", row.dangling));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    }
}

/// The text table, one trash per line, then the total. A trailing newline
/// is included. An empty discovery is an empty string: [`run_with`] writes
/// [`NO_TRASH`] to stderr, as `rtrash status` does.
pub fn render(snap: &Snapshot) -> String {
    if snap.roots.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for row in &snap.roots {
        let note = notes(row);
        out.push_str(&format!(
            "{}\t{}{note}\t{}\n",
            term_path(&row.root),
            plural(row.items, "item", "items"),
            format_bytes(row.bytes),
        ));
    }
    out.push_str(&format!(
        "Total: {} ({})\n",
        plural(snap.items, "item", "items"),
        format_bytes(snap.bytes),
    ));
    out
}

/// One JSON object: `items`, `bytes`, and `roots` (each root has `path`,
/// `items`, `bytes`, `orphans` and `dangling`). No trailing newline.
/// `rtrash status --json` uses `total_items`, `total_bytes` and `fs_*`
/// instead.
pub fn render_json(snap: &Snapshot) -> String {
    let roots: Vec<String> = snap
        .roots
        .iter()
        .map(|row| {
            let path = row.root.to_string_lossy();
            format!(
                r#"{{"path":"{}","items":{},"bytes":{},"orphans":{},"dangling":{}}}"#,
                json_escape(&path),
                row.items,
                row.bytes,
                row.orphans,
                row.dangling,
            )
        })
        .collect();
    format!(
        r#"{{"items":{},"bytes":{},"roots":[{}]}}"#,
        snap.items,
        snap.bytes,
        roots.join(","),
    )
}

/// Read process arguments and print the display. Exit 0 on success, 2 on
/// a usage error or a trash this display cannot count.
pub fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    run_with(&args)
}

/// [`run`] for an already-collected argument list.
pub fn run_with(args: &[String]) -> i32 {
    match parse(args) {
        Ok(Parsed::Help) => {
            print!("{HELP}");
            0
        }
        Ok(Parsed::Version) => {
            println!("rtrash-hud {}", rtrash::version());
            0
        }
        Err(err) => {
            eprintln!("rtrash-hud: {err}");
            eprintln!("Try 'rtrash-hud --help' for more information.");
            2
        }
        Ok(Parsed::Run(args)) => match snapshot(&args.pins, args.home_only) {
            Ok(snap) => {
                if args.json {
                    println!("{}", render_json(&snap));
                } else if snap.roots.is_empty() {
                    eprint!("{NO_TRASH}");
                } else {
                    print!("{}", render(&snap));
                }
                0
            }
            Err(err) => {
                eprintln!("rtrash-hud: {err}");
                2
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_matches_the_library() {
        assert_eq!(rtrash::version(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn help_names_the_count_and_the_freedesktop_switch() {
        assert!(HELP.contains("rtrash status"));
        assert!(HELP.contains("item count and reclaimable size"));
        assert!(HELP.contains("--trash-dir"));
        assert!(HELP.contains("--json"));
        assert!(HELP.contains("does not"));
        assert!(HELP.contains("RTRASH_MACOS_TRASH=freedesktop"));
        assert!(HELP.contains("macos.trash"));
        assert!(HELP.contains("no age filter"));
        assert!(HELP.contains("total_items"));
        assert!(!HELP.contains("--offline"));
    }

    #[test]
    fn parse_pins_home_and_json() {
        let parsed = parse(&[
            "--home-only".into(),
            "--json".into(),
            "--trash-dir=/tmp/one".into(),
            "--trash-dir".into(),
            "/tmp/two".into(),
        ])
        .unwrap();
        match parsed {
            Parsed::Run(args) => {
                assert!(args.home_only);
                assert!(args.json);
                assert_eq!(
                    args.pins,
                    vec![PathBuf::from("/tmp/one"), PathBuf::from("/tmp/two")]
                );
            }
            other => panic!("expected a run, got {other:?}"),
        }
    }

    #[test]
    fn help_and_version_and_a_bad_flag() {
        assert_eq!(parse(&["--help".into()]).unwrap(), Parsed::Help);
        assert_eq!(parse(&["-V".into()]).unwrap(), Parsed::Version);
        assert!(parse(&["--nope".into()]).unwrap_err().contains("--nope"));
        assert!(parse(&["--offline".into()])
            .unwrap_err()
            .contains("--offline"));
        assert!(parse(&["--trash-dir".into()])
            .unwrap_err()
            .contains("requires a PATH"));
        assert_eq!(run_with(&["--nope".into()]), 2);
    }

    #[test]
    fn render_names_orphans_and_the_total() {
        let snap = Snapshot {
            roots: vec![RootTotal {
                root: PathBuf::from("trash"),
                items: 2,
                bytes: 4096,
                orphans: 1,
                dangling: 0,
            }],
            items: 2,
            bytes: 4096,
        };
        let size = format_bytes(4096);
        assert_eq!(
            render(&snap),
            format!("trash\t2 items (1 without .trashinfo)\t{size}\nTotal: 2 items ({size})\n")
        );
        let expect = concat!(
            r#"{"items":2,"bytes":4096,"roots":[{"path":"trash","#,
            r#""items":2,"bytes":4096,"orphans":1,"dangling":0}]}"#
        );
        assert_eq!(render_json(&snap), expect);
    }

    #[test]
    fn render_json_escapes_a_quote_in_the_path() {
        let snap = Snapshot {
            roots: vec![RootTotal {
                root: PathBuf::from("tra\"sh"),
                items: 0,
                bytes: 0,
                orphans: 0,
                dangling: 1,
            }],
            items: 0,
            bytes: 0,
        };
        let json = render_json(&snap);
        assert!(json.contains(r#"tra\"sh"#), "{json}");
        assert!(json.contains(r#""dangling":1"#), "{json}");
    }

    #[test]
    fn render_of_no_trash_is_empty() {
        let snap = Snapshot {
            roots: Vec::new(),
            items: 0,
            bytes: 0,
        };
        assert_eq!(render(&snap), "");
        assert_eq!(
            NO_TRASH,
            "No trash directories found\nTotal: 0 items (0 B)\n"
        );
    }

    #[cfg(unix)]
    mod unix {
        use std::fs;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use super::*;

        struct Tmp(PathBuf);

        impl Tmp {
            fn dir(tag: &str) -> Self {
                static N: AtomicUsize = AtomicUsize::new(0);
                let n = N.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir()
                    .join(format!("rtrash-hud-{tag}-{}-{n}", std::process::id()));
                let _ = fs::remove_dir_all(&path);
                fs::create_dir_all(&path).unwrap();
                Tmp(path)
            }

            fn trash(tag: &str) -> Self {
                let tmp = Self::dir(tag);
                fs::create_dir_all(tmp.0.join("files")).unwrap();
                fs::create_dir_all(tmp.0.join("info")).unwrap();
                tmp
            }
        }

        impl Drop for Tmp {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        fn record(root: &std::path::Path, name: &str, body: &[u8]) {
            fs::write(root.join("files").join(name), body).unwrap();
            fs::write(
                root.join("info").join(format!("{name}.trashinfo")),
                format!("[Trash Info]\nPath=/tmp/{name}\nDeletionDate=2026-01-02T03:04:05\n"),
            )
            .unwrap();
        }

        fn rows(root: &PathBuf) -> Vec<rtrash::status::RootSummary> {
            let dirs = rtrash::trashdir::resolve_dirs_opts(std::slice::from_ref(root), false);
            let (rows, items, bytes) = rtrash::status::summarize_dirs_readonly(
                &dirs,
                None,
                None,
                rtrash::list::SizeFilter::default(),
            );
            assert_eq!(rows.iter().map(|r| r.items).sum::<u64>(), items);
            assert_eq!(rows.iter().map(|r| r.bytes).sum::<u64>(), bytes);
            rows
        }

        #[test]
        fn an_empty_trash_counts_zero() {
            let tmp = Tmp::trash("empty");
            let snap = snapshot(std::slice::from_ref(&tmp.0), false).unwrap();
            assert_eq!(snap.roots.len(), 1);
            assert_eq!((snap.items, snap.bytes), (0, 0));
            let got = rows(&tmp.0);
            assert_eq!(got.len(), 1);
            assert_eq!((got[0].items, got[0].bytes), (0, 0));
            let text = render(&snap);
            assert!(text.contains("0 items"), "{text}");
            assert!(!text.contains("No trash directories found"), "{text}");
        }

        #[test]
        fn a_record_and_an_orphan_match_the_status_library() {
            let tmp = Tmp::trash("both");
            record(&tmp.0, "hello.txt", b"hello-payload");
            fs::write(tmp.0.join("files").join("loose.txt"), b"orphan-payload").unwrap();
            let snap = snapshot(&[tmp.0.clone(), tmp.0.clone()], true).unwrap();
            let got = rows(&tmp.0);
            assert_eq!(snap.roots.len(), 1, "a trash pinned twice is one trash");
            assert_eq!(snap.items, 2, "the orphan counts, as status counts it");
            assert_eq!(snap.roots[0].items, got[0].items);
            assert_eq!(snap.roots[0].bytes, got[0].bytes);
            assert_eq!(snap.roots[0].orphans, got[0].orphans);
            assert_eq!(snap.roots[0].dangling, got[0].dangling);
            assert!(snap.bytes > 0, "a payload takes some disk");
            assert_eq!(snap.roots[0].orphans, 1);
            let text = render(&snap);
            assert!(text.contains("2 items"), "{text}");
            assert!(text.contains("1 without .trashinfo"), "{text}");
            assert!(text.contains(&format_bytes(snap.roots[0].bytes)), "{text}");
        }

        /// Zeroing one root field must fail this: the row is the status row.
        #[test]
        fn each_root_field_matches_status_including_dangling() {
            let tmp = Tmp::trash("dangling");
            record(&tmp.0, "gone.txt", b"gone-payload");
            fs::remove_file(tmp.0.join("files").join("gone.txt")).unwrap();
            let snap = snapshot(std::slice::from_ref(&tmp.0), false).unwrap();
            let got = rows(&tmp.0);
            assert_eq!(snap.roots.len(), 1);
            assert_eq!(snap.roots[0].items, got[0].items);
            assert_eq!(snap.roots[0].bytes, got[0].bytes);
            assert_eq!(snap.roots[0].orphans, got[0].orphans);
            assert_eq!(snap.roots[0].dangling, got[0].dangling);
            assert!(snap.roots[0].dangling >= 1, "{:?}", snap.roots[0].dangling);
            assert!(snap.roots[0].bytes > 0, "the record itself takes space");
            let text = render(&snap);
            assert!(text.contains("without a payload"), "{text}");
            assert!(text.contains(&format_bytes(snap.roots[0].bytes)), "{text}");
        }

        /// A payload symlink counts as the link, not the file it names.
        #[test]
        fn a_payload_symlink_counts_as_the_link() {
            let tmp = Tmp::trash("payload-link");
            let outside = tmp.0.join("outside-big");
            fs::write(&outside, vec![b'x'; 1_000_000]).unwrap();
            std::os::unix::fs::symlink(&outside, tmp.0.join("files").join("link")).unwrap();
            fs::write(
                tmp.0.join("info").join("link.trashinfo"),
                "[Trash Info]\nPath=/tmp/link\nDeletionDate=2026-01-02T03:04:05\n",
            )
            .unwrap();
            let snap = snapshot(std::slice::from_ref(&tmp.0), false).unwrap();
            assert_eq!(snap.items, 1);
            assert!(
                snap.bytes < 100_000,
                "counted the link target: {} bytes",
                snap.bytes
            );
            assert_eq!(snap.bytes, rows(&tmp.0)[0].bytes);
            assert_eq!(fs::metadata(&outside).unwrap().len(), 1_000_000);
        }

        #[test]
        fn a_pin_that_is_not_a_trash_is_refused() {
            let tmp = Tmp::dir("not-a-trash");
            let err = snapshot(std::slice::from_ref(&tmp.0), false).unwrap_err();
            assert!(err.contains("no valid"), "{err}");
        }
    }
}
