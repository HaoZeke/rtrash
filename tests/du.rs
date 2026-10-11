//! `rtrash du`: sizes, ordering, hard links, symlinks, filters and output modes.
//!
//! These run on Windows too. A few stay Unix only: sparse files (NTFS does
//! not make them by default), symlinks (they need a privilege there), the
//! GNU du parity check, and the FreeDesktop trash row.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rtrash")
}

struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("rtrash-du-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Tree {
            root: plain(fs::canonicalize(&root).unwrap()),
        }
    }

    fn file(&self, rel: &str, len: usize) -> PathBuf {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, vec![b'x'; len]).unwrap();
        p
    }

    fn du(&self, args: &[&str]) -> Output {
        Command::new(bin())
            .arg("du")
            .args(args)
            .current_dir(&self.root)
            .env("XDG_DATA_HOME", self.root.join(".xdg"))
            .env("XDG_CONFIG_HOME", self.root.join(".cfg"))
            .env("HOME", &self.root)
            .output()
            .expect("run rtrash du")
    }

    fn json(&self, args: &[&str]) -> String {
        let mut a = vec!["--json"];
        a.extend_from_slice(args);
        let out = self.du(&a);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// `canonicalize` on Windows returns a `\\?\` path; rtrash prints the
/// ordinary form, so tests compare against that.
fn plain(p: PathBuf) -> PathBuf {
    match p.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(rest) if cfg!(windows) => PathBuf::from(rest),
        _ => p,
    }
}

/// Undo the JSON escapes rtrash writes into a path string.
fn unescape(s: &str) -> String {
    s.replace(r"\\", "\u{0}")
        .replace(r#"\""#, "\"")
        .replace('\u{0}', "\\")
}

/// A path as it appears between quotes in rtrash's JSON.
fn escaped(path: &str) -> String {
    path.replace('\\', r"\\").replace('"', r#"\""#)
}

/// Set a file or directory's modification time. Windows opens a directory
/// only with FILE_FLAG_BACKUP_SEMANTICS.
fn set_mtime(p: &Path, t: SystemTime) {
    #[cfg(windows)]
    let f = {
        use std::os::windows::fs::OpenOptionsExt;
        File::options()
            .access_mode(0x100) // FILE_WRITE_ATTRIBUTES
            .custom_flags(0x0200_0000) // FILE_FLAG_BACKUP_SEMANTICS
            .open(p)
            .unwrap()
    };
    #[cfg(not(windows))]
    let f = if p.is_dir() {
        File::open(p).unwrap()
    } else {
        File::options().write(true).open(p).unwrap()
    };
    f.set_modified(t).unwrap();
}

/// Paths of one JSON array ("dirs" or "files"), in order.
fn paths_in(json: &str, key: &str) -> Vec<String> {
    let start = json.find(&format!("\"{key}\":[")).expect(key) + key.len() + 4;
    let mut depth = 1;
    let mut end = start;
    for (i, c) in json[start..].char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    end = start + i;
                    break;
                }
            }
            _ => {}
        }
    }
    json[start..end]
        .split("\"path\":\"")
        .skip(1)
        .map(|s| unescape(path_end(s)))
        .collect()
}

/// The JSON string body at the start of `s`, up to its closing quote.
fn path_end(s: &str) -> &str {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i] != b'"' {
        i += if b[i] == b'\\' { 2 } else { 1 };
    }
    &s[..i.min(b.len())]
}

fn field(json: &str, key: &str) -> u64 {
    let at = json.find(&format!("\"{key}\":")).expect(key) + key.len() + 3;
    json[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap()
}

/// A path as rtrash prints it. Tests build paths with `/`; on Windows the
/// printed form uses `\\` throughout.
fn s(p: &Path) -> String {
    let text = p.to_string_lossy().into_owned();
    if cfg!(windows) {
        text.replace('/', "\\")
    } else {
        text
    }
}

#[test]
fn lists_largest_files_and_dirs_first() {
    let t = Tree::new("order");
    let big = t.file("a/big.bin", 300_000);
    let mid = t.file("b/mid.bin", 200_000);
    t.file("b/small.bin", 10);
    let j = t.json(&[]);
    assert_eq!(paths_in(&j, "files")[..2], [s(&big), s(&mid)], "{j}");
    let dirs = paths_in(&j, "dirs");
    assert_eq!(
        dirs[..2],
        [s(&t.root.join("a")), s(&t.root.join("b"))],
        "{j}"
    );
    assert!(
        !dirs.contains(&s(&t.root)),
        "scan root is a total, not a row"
    );
    assert!(j.contains("\"files\":3"), "{j}");
}

// Unix only: see the comment at the top of the file.
#[cfg(unix)]
#[test]
fn apparent_ranks_sparse_file_first() {
    let t = Tree::new("sparse");
    let dense = t.file("dense.bin", 256 * 1024);
    let sparse = t.root.join("sparse.bin");
    File::create(&sparse)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let by_disk = paths_in(&t.json(&["--files"]), "files");
    assert_eq!(by_disk[0], s(&dense));
    let by_len = paths_in(&t.json(&["--files", "--apparent"]), "files");
    assert_eq!(by_len[0], s(&sparse));
}

#[test]
fn hard_links_count_once() {
    // Wine's directory listing reports file ID 0 for every file, so it has
    // nothing to match links by; real NTFS reports IDs.
    if cfg!(windows) && std::env::var_os("WINEPREFIX").is_some() {
        return;
    }
    let t = Tree::new("hardlinks");
    let a = t.file("d/a.bin", 100_000);
    fs::hard_link(&a, t.root.join("d/a-link.bin")).unwrap();
    fs::create_dir(t.root.join("e")).unwrap();
    fs::hard_link(&a, t.root.join("e/a-link2.bin")).unwrap();
    let j = t.json(&["--files"]);
    let roots = &j[..j.find("\"dirs\"").unwrap()];
    assert_eq!(field(roots, "apparent"), 100_000, "{j}");
    assert_eq!(field(roots, "files"), 1, "{j}");
    assert_eq!(field(roots, "links"), 2, "{j}");
    assert_eq!(paths_in(&j, "files").len(), 1, "{j}");
    let text = String::from_utf8(t.du(&[]).stdout).unwrap();
    assert!(text.contains("1 file (+2 hard links), "), "{text}");
}

#[cfg(unix)]
#[test]
fn hard_links_count_once_and_symlinks_are_not_followed() {
    let t = Tree::new("links");
    let outside = Tree::new("links-outside");
    let target = outside.file("huge.bin", 1_000_000);
    let a = t.file("d/a.bin", 100_000);
    fs::hard_link(&a, t.root.join("d/a-link.bin")).unwrap();
    std::os::unix::fs::symlink(&target, t.root.join("d/huge-link")).unwrap();
    let j = t.json(&["--files"]);
    let roots = &j[..j.find("\"dirs\"").unwrap()];
    let apparent = field(roots, "apparent");
    // Directories add nothing to the apparent size (as in GNU du), so the
    // total is exactly one copy of a.bin plus the symlink's own length.
    let link_len = fs::symlink_metadata(t.root.join("d/huge-link"))
        .unwrap()
        .len();
    assert_eq!(apparent, 100_000 + link_len, "{j}");
}

#[test]
fn dominant_child_hides_parent_unless_all_dirs() {
    let t = Tree::new("collapse");
    t.file("outer/inner/blob.bin", 500_000);
    t.file("outer/tiny.txt", 10);
    let outer = s(&t.root.join("outer"));
    let inner = s(&t.root.join("outer/inner"));
    let dirs = paths_in(&t.json(&["--dirs"]), "dirs");
    assert_eq!(dirs, std::slice::from_ref(&inner));
    let all = paths_in(&t.json(&["--dirs", "--all-dirs"]), "dirs");
    assert_eq!(all, [outer, inner]);
}

#[test]
fn max_depth_and_top_limit_rows() {
    let t = Tree::new("depth");
    t.file("one/two/three/deep.bin", 400_000);
    t.file("one/shallow.bin", 300_000);
    t.file("one/also.bin", 200_000);
    let files = paths_in(&t.json(&["--files", "--max-depth=2"]), "files");
    assert_eq!(
        files,
        [
            s(&t.root.join("one/shallow.bin")),
            s(&t.root.join("one/also.bin"))
        ]
    );
    let top1 = paths_in(&t.json(&["--files", "-n", "1"]), "files");
    assert_eq!(top1, [s(&t.root.join("one/two/three/deep.bin"))]);
}

/// A directory that one subdirectory dominates still gets its own row
/// when that subdirectory cannot be listed: past --max-depth, or a `.git`.
#[test]
fn dominated_dir_stays_when_its_bulk_is_hidden() {
    let t = Tree::new("hidden-bulk");
    t.file("big/inner/blob.bin", 900_000);
    t.file("big/small.bin", 1_000);
    t.file("repo/.git/objects/pack.bin", 900_000);
    t.file("repo/README", 1_000);
    let big = s(&t.root.join("big"));
    let inner = s(&t.root.join("big/inner"));
    let repo = s(&t.root.join("repo"));

    let shallow = paths_in(&t.json(&["--dirs", "--max-depth=1", "--no-git"]), "dirs");
    assert!(shallow.contains(&big), "{shallow:?}");
    assert!(shallow.contains(&repo), "{shallow:?}");

    // Without the depth cap, the inner directory takes the row as before.
    let deep = paths_in(&t.json(&["--dirs", "--no-git"]), "dirs");
    assert!(deep.contains(&inner) && !deep.contains(&big), "{deep:?}");
    assert!(deep.contains(&repo), "{deep:?}");

    // --newer-than drops the stale inner directory, so `big` (fresh
    // through small.bin) keeps its row.
    let long_ago = SystemTime::now() - Duration::from_secs(90 * 86_400);
    for p in ["big/inner/blob.bin", "big/inner"] {
        set_mtime(&t.root.join(p), long_ago);
    }
    let fresh = paths_in(&t.json(&["--dirs", "--newer-than=30", "--no-git"]), "dirs");
    assert!(fresh.contains(&big) && !fresh.contains(&inner), "{fresh:?}");
}

#[test]
fn age_filters_use_newest_mtime() {
    let t = Tree::new("age");
    let old = t.file("old/stale.bin", 300_000);
    let new = t.file("new/fresh.bin", 200_000);
    let long_ago = SystemTime::now() - Duration::from_secs(90 * 86_400);
    set_mtime(&old, long_ago);
    set_mtime(&t.root.join("old"), long_ago);
    let j = t.json(&["--older-than=30"]);
    assert_eq!(paths_in(&j, "files"), [s(&old)], "{j}");
    assert_eq!(paths_in(&j, "dirs"), [s(&t.root.join("old"))], "{j}");
    let j = t.json(&["--newer-than", "30"]);
    assert_eq!(paths_in(&j, "files"), [s(&new)], "{j}");
}

#[test]
fn since_and_until_take_dates() {
    let t = Tree::new("dates");
    let old = t.file("old/stale.bin", 300_000);
    let new = t.file("new/fresh.bin", 200_000);
    // 2020-06-15 12:00 UTC, far from any boundary below in every zone.
    let then = SystemTime::UNIX_EPOCH + Duration::from_secs(1_592_222_400);
    set_mtime(&old, then);
    set_mtime(&t.root.join("old"), then);
    let j = t.json(&["--until=2021-01-01"]);
    assert_eq!(paths_in(&j, "files"), [s(&old)], "{j}");
    assert_eq!(paths_in(&j, "dirs"), [s(&t.root.join("old"))], "{j}");
    let j = t.json(&["--since", "2021-01-01"]);
    assert_eq!(paths_in(&j, "files"), [s(&new)], "{j}");
    let j = t.json(&["--since=2020-06-01", "--until=2020-07-01T00:00"]);
    assert_eq!(paths_in(&j, "files"), [s(&old)], "{j}");
    // The stricter cutoff wins, whichever option comes first.
    let j = t.json(&["--newer-than=30", "--since=2020-01-01"]);
    assert_eq!(paths_in(&j, "files"), [s(&new)], "{j}");
    let out = t.du(&["--since=2021-02-30"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("invalid DATE"),
        "{out:?}"
    );
}

#[test]
fn sort_by_items_counts_files_and_dirs_below() {
    let t = Tree::new("items");
    for i in 0..3 {
        t.file(&format!("many/f{i}"), 10);
    }
    t.file("many/s/g0", 10);
    t.file("many/s/g1", 10);
    t.file("few/big.bin", 500_000);
    let j = t.json(&["--no-git", "--sort=items"]);
    // many: 3 files, the s directory and its 2 files.
    assert_eq!(paths_in(&j, "dirs")[0], s(&t.root.join("many")), "{j}");
    assert!(
        row(&j, &s(&t.root.join("many"))).contains("\"items\":6,"),
        "{j}"
    );
    assert!(
        row(&j, &s(&t.root.join("few"))).contains("\"items\":1,"),
        "{j}"
    );
    assert!(paths_in(&j, "files").is_empty(), "{j}");
    // By size the order flips.
    let j = t.json(&["--no-git", "--sort", "disk"]);
    assert_eq!(paths_in(&j, "dirs")[0], s(&t.root.join("few")), "{j}");

    let text = String::from_utf8(t.du(&["--no-git", "--sort=count"]).stdout).unwrap();
    assert!(
        text.starts_with("Directories with the most items\n"),
        "{text}"
    );
    assert!(text.contains("ITEMS"), "{text}");
    let text = String::from_utf8(t.du(&["--no-git", "-C", "-n1"]).stdout).unwrap();
    assert!(text.contains("ITEMS"), "{text}");
    assert_eq!(text.matches(&*s(&t.root.join("few"))).count(), 2, "{text}");
    assert_eq!(t.du(&["--sort=bogus"]).status.code(), Some(2));
}

#[test]
fn hard_links_count_once_in_items() {
    if cfg!(windows) && std::env::var_os("WINEPREFIX").is_some() {
        return;
    }
    let t = Tree::new("items-links");
    let a = t.file("d/a.bin", 1000);
    fs::hard_link(&a, t.root.join("d/b.bin")).unwrap();
    t.file("d/c.bin", 1000);
    let j = t.json(&["--no-git", "--sort=items", "--all-dirs"]);
    assert!(
        row(&j, &s(&t.root.join("d"))).contains("\"items\":2,"),
        "{j}"
    );
}

#[test]
fn sort_by_mtime_lists_where_the_newest_change_is() {
    let t = Tree::new("mtime-sort");
    let long_ago = SystemTime::now() - Duration::from_secs(90 * 86_400);
    let a_bit_ago = SystemTime::now() - Duration::from_secs(5 * 86_400);
    let old = t.file("old/stale.bin", 900_000);
    let mid = t.file("mid/m.bin", 10);
    let fresh = t.file("proj/src/fresh.rs", 10);
    let other = t.file("proj/doc/a.md", 10);
    set_mtime(&old, long_ago);
    set_mtime(&mid, a_bit_ago);
    set_mtime(&other, long_ago);
    for d in ["old", "mid", "proj/src", "proj/doc", "proj"] {
        set_mtime(&t.root.join(d), long_ago);
    }
    let j = t.json(&["--no-git", "--sort=mtime"]);
    let dirs = paths_in(&j, "dirs");
    // proj gives way to proj/src, which holds its newest change.
    assert_eq!(
        dirs[..2],
        [s(&t.root.join("proj/src")), s(&t.root.join("mid"))],
        "{j}"
    );
    assert!(!dirs.contains(&s(&t.root.join("proj"))), "{j}");
    assert_eq!(
        paths_in(&j, "files")[..3],
        [s(&fresh), s(&mid), s(&old)],
        "{j}"
    );
    let text = String::from_utf8(t.du(&["--no-git", "--sort=mtime"]).stdout).unwrap();
    assert!(
        text.starts_with("Most recently modified directories\n"),
        "{text}"
    );
}

#[test]
fn snapshots_cannot_rank_by_items() {
    let t = Tree::new("snap-items");
    t.file("a/x", 10);
    let out = t.du(&["-o", "s.snap"]);
    assert!(out.status.success());
    assert_eq!(
        t.du(&["-f", "s.snap", "--sort=items"]).status.code(),
        Some(2)
    );
    assert!(t.du(&["-f", "s.snap", "--sort=mtime"]).status.success());
}

#[test]
fn print0_and_text_output() {
    let t = Tree::new("print0");
    let f = t.file("d/f.bin", 50_000);
    let out = t.du(&["--files", "--print0"]);
    assert!(out.status.success());
    assert_eq!(out.stdout, format!("{}\0", s(&f)).into_bytes());

    let out = t.du(&["-n", "5"]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("Largest directories (by disk use)"), "{text}");
    assert!(
        text.contains(&format!(
            "{}{}",
            s(&t.root.join("d")),
            std::path::MAIN_SEPARATOR
        )),
        "{text}"
    );
    assert!(text.contains("Total "), "{text}");
    assert!(text.contains(" apparent, 1 file, 2 directories"), "{text}");
}

#[test]
fn usage_errors_exit_2() {
    let t = Tree::new("usage");
    assert_eq!(t.du(&["--bogus"]).status.code(), Some(2));
    assert_eq!(t.du(&["-n", "x"]).status.code(), Some(2));
    assert_eq!(t.du(&["--older-than=-1"]).status.code(), Some(2));
    assert_eq!(t.du(&["--json", "--print0"]).status.code(), Some(2));
    let threads = t.du(&["--threads=0"]);
    assert_eq!(threads.status.code(), Some(2));
    // The hint names the subcommand's own help.
    let hint = String::from_utf8_lossy(&threads.stderr);
    assert!(hint.contains(" du --help'"), "{hint}");
    let help = String::from_utf8(t.du(&["--help"]).stdout).unwrap();
    assert!(
        help.lines().next().unwrap().contains(" du [OPTION]..."),
        "{help}"
    );
    assert_eq!(t.du(&["--threads", "many"]).status.code(), Some(2));
    // No terminal on stdin in tests: --pick refuses instead of hanging.
    assert_eq!(t.du(&["--pick"]).status.code(), Some(2));
    assert_eq!(t.du(&["missing-path"]).status.code(), Some(1));
}

#[test]
fn artefacts_are_one_row_and_filterable() {
    let t = Tree::new("artefacts");
    t.file("proj/target/.rustc_info.json", 10);
    t.file("proj/target/debug/huge.rlib", 600_000);
    t.file("web/node_modules/pkg/index.js", 300_000);
    let src = t.file("proj/src/main.rs", 100_000);
    let target = s(&t.root.join("proj/target"));
    let nm = s(&t.root.join("web/node_modules"));

    let j = t.json(&[]);
    let dirs = paths_in(&j, "dirs");
    // proj (target + src) outranks target; both rows appear.
    assert_eq!(dirs[..2], [s(&t.root.join("proj")), target.clone()], "{j}");
    assert!(
        !dirs.iter().any(|d| d.starts_with(&format!("{target}/"))),
        "{j}"
    );
    assert!(j.contains("\"artefact\":\"cargo-target\""), "{j}");
    assert!(j.contains("\"artefact\":\"node-modules\""), "{j}");
    // Files inside artefacts are covered by the artefact row.
    assert_eq!(paths_in(&j, "files"), [s(&src)], "{j}");

    let j = t.json(&["--artefacts"]);
    assert_eq!(paths_in(&j, "dirs"), [target, nm], "{j}");
    assert!(paths_in(&j, "files").is_empty(), "{j}");

    let out = t.du(&["--artefacts"]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("cargo-target"), "{text}");
}

#[test]
fn an_artefact_named_as_path_lists_its_contents() {
    let t = Tree::new("artefact-root");
    t.file("web/node_modules/pkg/index.js", 300_000);
    t.file("web/node_modules/pkg/dist/bundle.js", 200_000);
    let nm = t.root.join("web/node_modules");
    let nm_s = s(&nm);

    let j = t.json(&[&nm_s]);
    assert!(j.contains("\"artefact\":\"node-modules\"}"), "{j}");
    assert_eq!(paths_in(&j, "dirs")[0], s(&nm.join("pkg")), "{j}");
    assert_eq!(
        paths_in(&j, "files"),
        [
            s(&nm.join("pkg/index.js")),
            s(&nm.join("pkg/dist/bundle.js"))
        ],
        "{j}"
    );
    let text = String::from_utf8(t.du(&[&nm_s]).stdout).unwrap();
    assert!(
        text.contains(&format!("Total {nm_s} (node-modules):")),
        "{text}"
    );
}

#[test]
fn exclude_patterns_leave_entries_out_of_sizes_and_rows() {
    let t = Tree::new("exclude");
    t.file("keep/a.bin", 100_000);
    t.file("keep/skip.iso", 400_000);
    t.file("deep/x/skip.iso", 300_000);
    t.file("build/out.o", 200_000);
    t.file("deep/build/k.bin", 50_000);
    let all = t.json(&["--no-git"]);
    let total = field(&all, "apparent");

    let j = t.json(&["--no-git", "--exclude=*.iso", "--exclude", "/build/"]);
    assert_eq!(field(&j, "apparent"), total - 900_000, "{j}");
    assert_eq!(field(&j, "excluded"), 3, "{j}");
    let files = paths_in(&j, "files");
    assert!(!files.iter().any(|f| f.ends_with(".iso")), "{j}");
    // Anchored: only the top-level build/ goes; deep/build stays.
    assert!(files.contains(&s(&t.root.join("deep/build/k.bin"))), "{j}");
    assert!(!files.contains(&s(&t.root.join("build/out.o"))), "{j}");

    // A negation re-includes; patterns come from a file too.
    fs::write(
        t.root.join("pats"),
        "# comment\n*.iso\n!keep/skip.iso\nbuild/\n",
    )
    .unwrap();
    let j = t.json(&["--no-git", "-X", "pats"]);
    let files = paths_in(&j, "files");
    assert!(files.contains(&s(&t.root.join("keep/skip.iso"))), "{j}");
    assert!(!files.contains(&s(&t.root.join("deep/x/skip.iso"))), "{j}");
    assert!(!files.contains(&s(&t.root.join("deep/build/k.bin"))), "{j}");

    // An absolute pattern under PATH works like a relative one.
    let abs = format!("--exclude={}", t.root.join("deep").display());
    let j = t.json(&["--no-git", &abs]);
    assert!(
        !paths_in(&j, "files").iter().any(|f| f.contains("deep")),
        "{j}"
    );

    let text = String::from_utf8(t.du(&["--no-git", "--exclude=*.iso"]).stdout).unwrap();
    assert!(text.contains(", 2 excluded"), "{text}");
    let out = t.du(&["-X", "no-such-file"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(t.du(&["--exclude="]).status.code(), Some(2));
}

#[test]
fn config_file_sets_du_defaults_and_options_win() {
    let t = Tree::new("config");
    t.file("a/x.iso", 300_000);
    t.file("a/y.bin", 100_000);
    t.file("b/z.bin", 200_000);
    fs::create_dir_all(t.root.join(".cfg/rtrash")).unwrap();
    fs::write(
        t.root.join(".cfg/rtrash/config.toml"),
        "[du]\ntop = 1\ngit = false\nexclude = [\"*.iso\"]\n",
    )
    .unwrap();
    let j = t.json(&[]);
    assert_eq!(paths_in(&j, "files"), [s(&t.root.join("b/z.bin"))], "{j}");
    assert_eq!(field(&j, "excluded"), 1, "{j}");
    // Options override; command-line patterns add to the config's.
    let j = t.json(&["--top=5", "--exclude=z.bin", "--exclude=/.cfg/"]);
    assert_eq!(paths_in(&j, "files"), [s(&t.root.join("a/y.bin"))], "{j}");
    fs::write(t.root.join(".cfg/rtrash/config.toml"), "[du]\ntop = -1\n").unwrap();
    let out = t.du(&["--no-git"]);
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("ignoring du.top = '-1'"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn exclude_caches_needs_a_valid_tag() {
    let t = Tree::new("cachedir");
    t.file("c/data.bin", 300_000);
    fs::write(
        t.root.join("c/CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55\n# a cache\n",
    )
    .unwrap();
    t.file("fake/data.bin", 200_000);
    fs::write(t.root.join("fake/CACHEDIR.TAG"), "not a tag\n").unwrap();
    let j = t.json(&["--no-git", "--exclude-caches"]);
    let dirs = paths_in(&j, "dirs");
    assert!(!dirs.contains(&s(&t.root.join("c"))), "{j}");
    assert!(dirs.contains(&s(&t.root.join("fake"))), "{j}");
    assert_eq!(field(&j, "excluded"), 1, "{j}");
    // Named as PATH, a cache is still scanned.
    let c = s(&t.root.join("c"));
    let j = t.json(&["--no-git", "--exclude-caches", &c]);
    assert!(field(&j, "apparent") >= 300_000, "{j}");
}

#[test]
fn gitignore_follows_git_rules_inside_repositories() {
    let t = Tree::new("gitignore");
    fs::create_dir_all(t.root.join("repo/.git/info")).unwrap();
    fs::write(t.root.join("repo/.git/info/exclude"), "*.tmp\n").unwrap();
    fs::write(t.root.join("repo/.gitignore"), "*.log\n/out/\n").unwrap();
    fs::create_dir_all(t.root.join("repo/sub")).unwrap();
    fs::write(t.root.join("repo/sub/.gitignore"), "!keep.log\n").unwrap();
    t.file("repo/a.log", 100_000);
    t.file("repo/sub/keep.log", 90_000);
    t.file("repo/sub/drop.log", 80_000);
    t.file("repo/x.tmp", 70_000);
    t.file("repo/out/big.bin", 300_000);
    t.file("repo/src/out/kept.bin", 60_000);
    // A nested repository does not inherit the outer rules.
    fs::create_dir_all(t.root.join("repo/inner/.git")).unwrap();
    t.file("repo/inner/n.log", 50_000);
    // Outside any repository a .gitignore means nothing.
    fs::create_dir_all(t.root.join("loose")).unwrap();
    fs::write(t.root.join("loose/.gitignore"), "*\n").unwrap();
    t.file("loose/l.bin", 40_000);

    let j = t.json(&["--no-git", "--gitignore"]);
    let files = paths_in(&j, "files");
    for kept in [
        "repo/sub/keep.log",
        "repo/src/out/kept.bin",
        "repo/inner/n.log",
        "loose/l.bin",
    ] {
        assert!(files.contains(&s(&t.root.join(kept))), "{kept}: {j}");
    }
    for gone in [
        "repo/a.log",
        "repo/sub/drop.log",
        "repo/x.tmp",
        "repo/out/big.bin",
    ] {
        assert!(!files.contains(&s(&t.root.join(gone))), "{gone}: {j}");
    }

    // Starting below the repository root still applies the rules above.
    let sub = s(&t.root.join("repo/sub"));
    let j = t.json(&["--no-git", "--gitignore", &sub]);
    let files = paths_in(&j, "files");
    assert!(files.contains(&s(&t.root.join("repo/sub/keep.log"))), "{j}");
    assert!(
        !files.contains(&s(&t.root.join("repo/sub/drop.log"))),
        "{j}"
    );
}

fn git_ok() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.org"])
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The JSON object of the row whose path is exactly `path`.
fn row<'a>(json: &'a str, path: &str) -> &'a str {
    let key = format!("{{\"path\":\"{}\",", escaped(path));
    let at = json
        .find(&key)
        .unwrap_or_else(|| panic!("no row {path}: {json}"));
    &json[at..at + json[at..].find('}').unwrap() + 1]
}

#[test]
fn bare_repos_are_one_row_with_a_git_state() {
    if !git_ok() {
        return;
    }
    let t = Tree::new("bare");
    let work = t.root.join("work");
    fs::create_dir(&work).unwrap();
    git(&work, &["init", "-q"]);
    t.file("work/blob.bin", 300_000);
    git(&work, &["add", "blob.bin"]);
    git(&work, &["commit", "-q", "-m", "blob"]);
    git(&t.root, &["clone", "-q", "--bare", "work", "mirror.git"]);
    git(&t.root, &["init", "-q", "--bare", "empty.git"]);
    // Git's names without a git HEAD: an ordinary directory.
    t.file("fake/HEAD", 10);
    t.file("fake/objects/data.bin", 200_000);
    t.file("fake/refs/r", 10);
    let mirror = s(&t.root.join("mirror.git"));

    let j = t.json(&["--all-dirs", "-n", "50"]);
    let inside = format!("{mirror}{}", std::path::MAIN_SEPARATOR);
    let all: Vec<String> = paths_in(&j, "dirs")
        .into_iter()
        .chain(paths_in(&j, "files"))
        .collect();
    assert!(!all.iter().any(|p| p.starts_with(&inside)), "{j}");
    // No remote has the mirror's commits: it may be the only copy.
    assert!(row(&j, &mirror).contains("\"git\":\"unpushed\""), "{j}");
    let empty = s(&t.root.join("empty.git"));
    assert!(row(&j, &empty).contains("\"git\":\"clean\""), "{j}");
    assert!(
        all.contains(&s(&t.root.join("fake/objects/data.bin"))),
        "{j}"
    );

    let out = t.du(&["--print0", "--include-dirty", "--all-dirs", "-n", "50"]);
    let printed = String::from_utf8(out.stdout).unwrap();
    assert!(!printed.contains(&inside), "{printed:?}");
}

#[test]
fn git_state_marks_rows_and_print0_holds_back_risky_ones() {
    if !git_ok() {
        return;
    }
    let t = Tree::new("git");
    // Clean repo with a remote: committed, pushed, target/ ignored.
    let remote = t.root.join("remote.git");
    fs::create_dir(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare"]);
    let clean = t.root.join("clean");
    t.file("clean/.gitignore", 0);
    fs::write(clean.join(".gitignore"), "target/\n").unwrap();
    t.file("clean/src/lib.rs", 200_000);
    t.file("clean/target/.rustc_info.json", 10);
    t.file("clean/target/debug/out.bin", 400_000);
    // A nested ignored directory: on Windows git gets its relative path
    // with a backslash.
    t.file("clean/crates/inner/target/out.bin", 300_000);
    git(&clean, &["init", "-q"]);
    git(&clean, &["add", "."]);
    git(&clean, &["commit", "-qm", "init"]);
    git(
        &clean,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&clean, &["push", "-q", "origin", "HEAD:main"]);
    git(&clean, &["fetch", "-q", "origin"]);
    // Local-only repo: its commit exists nowhere else.
    let local = t.root.join("local");
    t.file("local/notes.txt", 300_000);
    git(&local, &["init", "-q"]);
    git(&local, &["add", "."]);
    git(&local, &["commit", "-qm", "mine"]);
    // A large file inside .git is never listed.
    t.file("local/.git/big-pack.bin", 900_000);

    let target = s(&clean.join("target"));
    let j = t.json(&["--all-dirs"]);
    assert!(row(&j, &target).contains("\"git\":\"ignored\""), "{j}");
    let nested = s(&clean.join("crates/inner/target"));
    assert!(row(&j, &nested).contains("\"git\":\"ignored\""), "{j}");
    assert!(
        row(&j, &s(&clean.join("src"))).contains("\"git\":\"clean\""),
        "{j}"
    );
    let local_row = row(&j, &s(&local));
    assert!(local_row.contains("\"git\":\"unpushed\""), "{local_row}");
    assert!(
        local_row.contains(&format!("\"git_repo\":\"{}\"", escaped(&s(&local)))),
        "{local_row}"
    );
    assert!(!j.contains("big-pack.bin"), "{j}");
    assert!(!j.contains(&escaped(&s(&local.join(".git")))), "{j}");

    // Untracked work makes the clean repo dirty.
    t.file("clean/src/new.rs", 10);
    let j = t.json(&["--all-dirs", "--dirs"]);
    assert!(
        row(&j, &s(&clean.join("src"))).contains("\"git\":\"dirty\""),
        "{j}"
    );
    assert!(row(&j, &target).contains("\"git\":\"ignored\""), "{j}");

    let out = t.du(&["--all-dirs", "--dirs", "--print0"]);
    let printed = String::from_utf8(out.stdout).unwrap();
    let printed: Vec<&str> = printed.split('\0').filter(|p| !p.is_empty()).collect();
    assert!(printed.contains(&target.as_str()), "{printed:?}");
    assert!(!printed.contains(&s(&local).as_str()), "{printed:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("--include-dirty"));
    let out = t.du(&["--all-dirs", "--dirs", "--print0", "--include-dirty"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains(&s(&local)));

    let j = t.json(&["--no-git"]);
    assert!(!j.contains("\"git\":\""), "{j}");
}

#[test]
fn stash_alone_marks_repo_dirty_and_ignores_batch() {
    if !git_ok() {
        return;
    }
    let t = Tree::new("git-stash");
    let repo = t.root.join("repo");
    t.file("repo/a.txt", 100_000);
    t.file("repo/b/data.bin", 200_000);
    t.file("repo/cache/x.bin", 300_000);
    t.file("repo/logs/run.log", 50_000);
    fs::write(repo.join(".gitignore"), "cache/\n*.log\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    // Detach so the commit is not on a local branch: no unpushed work.
    git(&repo, &["checkout", "-q", "--detach"]);
    git(&repo, &["branch", "-q", "-D", "main"]);
    let j = t.json(&["--all-dirs"]);
    assert!(
        row(&j, &s(&repo.join("b"))).contains("\"git\":\"clean\""),
        "{j}"
    );
    // One check-ignore batch answers rows matched by dir and file rules.
    assert!(
        row(&j, &s(&repo.join("cache"))).contains("\"git\":\"ignored\""),
        "{j}"
    );
    assert!(
        row(&j, &s(&repo.join("logs/run.log"))).contains("\"git\":\"ignored\""),
        "{j}"
    );
    assert!(
        row(&j, &s(&repo.join("a.txt"))).contains("\"git\":\"clean\""),
        "{j}"
    );

    // A stash is the only change: the worktree is clean but work would be lost.
    fs::write(repo.join("a.txt"), "changed").unwrap();
    git(&repo, &["stash", "-q"]);
    let j = t.json(&["--all-dirs"]);
    assert!(
        row(&j, &s(&repo.join("b"))).contains("\"git\":\"dirty\""),
        "{j}"
    );
    assert!(
        row(&j, &s(&repo.join("cache"))).contains("\"git\":\"ignored\""),
        "{j}"
    );
}

#[cfg(unix)]
fn gnu_du() -> bool {
    Command::new("du")
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("GNU coreutils"))
        .unwrap_or(false)
}

#[cfg(unix)]
fn du_total(flag: &str, root: &Path) -> u64 {
    let out = Command::new("du")
        .args(["-s", flag])
        .arg(root)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

// Unix only: see the comment at the top of the file.
#[cfg(unix)]
#[test]
fn totals_match_du_with_deep_links_and_unreadable_dirs() {
    use std::os::unix::fs::PermissionsExt;
    if !gnu_du() {
        return;
    }
    let t = Tree::new("parity");
    // Deeper than the walker keeps directory descriptors open for.
    let mut deep = String::from("deep");
    for i in 0..80 {
        deep.push_str(&format!("/d{i}"));
        t.file(&format!("{deep}/f.bin"), 1000 + i);
    }
    let a = t.file("links/a.bin", 50_000);
    fs::hard_link(&a, t.root.join("links/b.bin")).unwrap();
    std::os::unix::fs::symlink(&a, t.root.join("links/sym")).unwrap();
    std::os::unix::fs::symlink(t.root.join("deep"), t.root.join("dirlink")).unwrap();
    for i in 0..600 {
        t.file(&format!("wide/f{i:04}"), i);
    }
    t.file("locked/inner/x.bin", 70_000);
    let locked = t.root.join("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let blocked = fs::read_dir(&locked).is_err();

    let out = t.du(&["--json", "--no-git", "-n", "0", "."]);
    let j = String::from_utf8(out.stdout).unwrap();
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    let disk = du_total("-B1", &t.root);
    let apparent = du_total("-b", &t.root);
    let inodes = du_total("--inodes", &t.root);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(field(&j, "disk"), disk, "{j}");
    assert_eq!(field(&j, "apparent"), apparent, "{j}");
    // Files and directories, each hard-linked file once, as du counts inodes.
    assert_eq!(field(&j, "files") + field(&j, "dirs"), inodes, "{j}");
    assert_eq!(field(&j, "links"), 1, "{j}");
    if blocked {
        assert_eq!(field(&j, "unreadable"), 1, "{j}");
        let named = format!("cannot read '{}': Permission denied", locked.display());
        assert!(err.contains(&named), "{err}");
        assert!(
            j.contains(&format!(
                "\"unreadable_paths\":[{{\"path\":\"{}\",\"error\":\"Permission denied",
                escaped(&s(&locked))
            )),
            "{j}"
        );
    }
}

// Unix only: see the comment at the top of the file.
#[cfg(unix)]
#[test]
fn unreadable_paths_are_named_up_to_ten_then_counted() {
    use std::os::unix::fs::PermissionsExt;
    let t = Tree::new("unread");
    let locked: Vec<PathBuf> = (0..12)
        .map(|i| {
            t.file(&format!("l{i:02}/x"), 10);
            t.root.join(format!("l{i:02}"))
        })
        .collect();
    for l in &locked {
        fs::set_permissions(l, fs::Permissions::from_mode(0o000)).unwrap();
    }
    let blocked = fs::read_dir(&locked[0]).is_err();
    let out = t.du(&["--no-git", "-n", "0", "."]);
    let json = t.json(&["--no-git", "-n", "0", "."]);
    for l in &locked {
        fs::set_permissions(l, fs::Permissions::from_mode(0o755)).unwrap();
    }
    if !blocked {
        return;
    }
    let err = String::from_utf8_lossy(&out.stderr);
    // Sorted by path: the first ten by name, then a count.
    for (i, l) in locked.iter().enumerate() {
        let line = format!("cannot read '{}'", l.display());
        assert_eq!(err.contains(&line), i < 10, "{err}");
    }
    assert!(err.contains("and 2 more could not be read"), "{err}");
    assert_eq!(field(&json, "unreadable"), 12, "{json}");
    assert_eq!(json.matches("\"error\":").count(), 12, "{json}");
}

#[test]
fn wide_directory_split_keeps_rows_and_counts() {
    let t = Tree::new("wide");
    for i in 0..1000 {
        t.file(&format!("flat/f{i:04}"), 10);
    }
    let big = t.file("flat/f0777", 300_000);
    t.file("flat/sub/inner.bin", 200_000);
    let j = t.json(&["--no-git", "-n", "2", "--files"]);
    let files = paths_in(&j, "files");
    assert_eq!(files[0], s(&big), "{j}");
    assert_eq!(files[1], s(&t.root.join("flat/sub/inner.bin")), "{j}");
    assert_eq!(field(&j, "files"), 1001, "{j}");
    // Any thread count gives the same answer.
    for n in ["1", "3"] {
        let k = t.json(&["--no-git", "-n", "2", "--files", "--threads", n]);
        assert_eq!(k, j, "--threads {n}");
    }
}

#[test]
fn size_ties_list_the_smallest_names() {
    let t = Tree::new("ties");
    // Names are written in reverse so creation order does not match the
    // expected rows, and the wide directory spans several chunks.
    for i in (0..900).rev() {
        t.file(&format!("flat/f{i:04}"), 10);
    }
    for i in (0..30).rev() {
        t.file(&format!("narrow/g{i:02}"), 10);
    }
    let j = t.json(&["--no-git", "-n", "4", "--files"]);
    let want: Vec<String> = ["flat/f0000", "flat/f0001", "flat/f0002", "flat/f0003"]
        .iter()
        .map(|p| s(&t.root.join(p)))
        .collect();
    assert_eq!(paths_in(&j, "files"), want, "{j}");
    // The trash summary reads the system Recycle Bin on Windows, which
    // other processes change while this runs; compare the scan alone.
    let scan = |j: &str| j[..j.find("\"trash\":").unwrap_or(j.len())].to_string();
    for n in ["1", "2", "5"] {
        let k = t.json(&["--no-git", "-n", "4", "--files", "--threads", n]);
        assert_eq!(scan(&k), scan(&j), "--threads {n}");
    }
}

// Unix only: see the comment at the top of the file.
#[cfg(unix)]
#[test]
fn trash_is_one_row_and_summarized() {
    let t = Tree::new("trash");
    let trash = t.root.join("Trash");
    let pin = format!("--trash-dir={}", trash.display());
    t.file("junk/old.bin", 400_000);
    let put = Command::new(bin())
        .args(["put", &pin, "-r", "junk"])
        .current_dir(&t.root)
        .output()
        .unwrap();
    assert!(
        put.status.success(),
        "{}",
        String::from_utf8_lossy(&put.stderr)
    );
    let keep = t.file("keep.bin", 100_000);

    let j = t.json(&[&pin]);
    let trash_s = s(&trash);
    assert!(row(&j, &trash_s).contains("\"artefact\":\"trash\""), "{j}");
    assert!(!j.contains("old.bin"), "{j}");
    let summary = &j[j.find("\"trash\":{").unwrap()..];
    assert!(
        summary.contains(&format!("\"roots\":[\"{trash_s}\"]")),
        "{summary}"
    );
    assert!(summary.contains("\"items\":1"), "{summary}");
    assert!(field(summary, "bytes") >= 400_000, "{summary}");

    let out = t.du(&[&pin, "--print0"]);
    let printed = String::from_utf8(out.stdout).unwrap();
    assert!(!printed.contains(&trash_s), "{printed:?}");
    assert!(printed.contains(&s(&keep)), "{printed:?}");

    // Named as the PATH, a trash root still keeps its contents to itself.
    let j = t.json(&[&pin, &trash_s]);
    assert!(paths_in(&j, "dirs").is_empty(), "{j}");
    assert!(paths_in(&j, "files").is_empty(), "{j}");

    let out = t.du(&[&pin]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("Trash on these file systems:"), "{text}");
    assert!(text.contains("rtrash empty -n"), "{text}");
    assert_eq!(
        t.du(&["--trash-dir=/nonexistent-rtrash"]).status.code(),
        Some(2)
    );
}

#[test]
fn snapshots_list_back_and_diff_against_a_later_scan() {
    let t = Tree::new("snap");
    t.file("grow/a", 10_000);
    t.file("shrink/b", 300_000);
    t.file("gone/c", 5_000);
    t.file("same/d", 1_000);
    let snap_a = PathBuf::from(format!("{}.a.snap", t.root.display()));
    let snap_b = PathBuf::from(format!("{}.b.snap", t.root.display()));
    let root = t.root.to_string_lossy().into_owned();
    let a = snap_a.to_string_lossy().into_owned();
    let b = snap_b.to_string_lossy().into_owned();

    let out = t.du(&["-o", &a, &root]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = fs::read_to_string(&snap_a).unwrap();
    assert!(text.starts_with("# rtrash du snapshot 1\n"), "{text}");
    assert!(
        text.lines()
            .any(|l| l.starts_with("dir ") && l.ends_with("same")),
        "{text}"
    );

    // Listing a snapshot needs no scan: the tree can change meanwhile.
    t.file("grow/more", 2_000_000);
    fs::remove_file(t.root.join("shrink/b")).unwrap();
    fs::remove_dir_all(t.root.join("gone")).unwrap();
    t.file("new/e", 50_000);
    let listed = t.du(&["-f", &a]);
    assert!(listed.status.success());
    let listed = String::from_utf8_lossy(&listed.stdout).into_owned();
    let first = listed.lines().nth(2).unwrap_or_default();
    assert!(
        first.contains("shrink"),
        "snapshot order, not the live tree: {listed}"
    );

    // Without NEW, diff scans the roots of OLD again.
    let live = t.du(&["diff", &a]);
    assert!(
        live.status.success(),
        "{}",
        String::from_utf8_lossy(&live.stderr)
    );
    let live = String::from_utf8_lossy(&live.stdout).into_owned();
    let rows: Vec<&str> = live.lines().skip(1).collect();
    assert!(
        rows[0].trim_end().ends_with("grow") && rows[0].trim_start().starts_with('+'),
        "{live}"
    );
    assert!(
        live.contains("gone") && live.lines().any(|l| l.contains(" gone ")),
        "{live}"
    );
    assert!(
        live.lines()
            .any(|l| l.contains(" new ") && l.ends_with("new")),
        "{live}"
    );
    assert!(!live.lines().any(|l| l.ends_with("same")), "{live}");

    // Two files, as JSON.
    assert!(t.du(&["-o", &b, &root]).status.success());
    let json = t.du(&["diff", "--json", "-n", "2", &a, &b]);
    let json = String::from_utf8_lossy(&json.stdout).into_owned();
    assert!(json.contains("\"changes\":[{"), "{json}");
    assert_eq!(json.matches("\"delta\":").count(), 2, "{json}");

    // Anything else is no snapshot.
    let bad = t.du(&["-f", &root]);
    assert_eq!(bad.status.code(), Some(1));
    let _ = fs::remove_file(&snap_a);
    let _ = fs::remove_file(&snap_b);
}

#[test]
fn ncdu_exports_list_and_diff_like_snapshots() {
    let t = Tree::new("ncdu");
    let doc = |big: u64| {
        format!(
            r#"[1,2,{{"progname":"ncdu","progver":"1.22","timestamp":1700000000}},
[{{"name":"/srv/data","dsize":4096,"dev":3}},
[{{"name":"big","dsize":4096}},{{"name":"f","asize":{big},"dsize":{big},"ino":7,"nlink":2}}],
[{{"name":"small","dsize":4096}},{{"name":"g","asize":100,"dsize":4096}},{{"name":"f2","dsize":{big},"ino":7,"nlink":2}}],
{{"name":"skipped","excluded":"pattern"}}]]"#
        )
    };
    let old = t.root.join("old.json");
    let new = t.root.join("new.json");
    fs::write(&old, doc(1_000_000)).unwrap();
    fs::write(&new, doc(3_000_000)).unwrap();
    let (o, n) = (old.to_str().unwrap(), new.to_str().unwrap());

    let json = t.du(&["-f", o, "--json"]);
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let json = String::from_utf8_lossy(&json.stdout).into_owned();
    // The second name of f counts as a link, not as bytes.
    assert!(
        json.contains(
            r#""disk":1016384,"apparent":1000100,"files":2,"links":1,"dirs":3,"excluded":1"#
        ),
        "{json}"
    );
    let listed = String::from_utf8_lossy(&t.du(&["-f", o]).stdout).into_owned();
    let rows: Vec<&str> = listed
        .lines()
        .filter(|l| l.contains("/srv/data/"))
        .collect();
    assert!(
        rows[0].trim_end_matches(['/', '\\']).ends_with("big"),
        "{listed}"
    );
    assert!(rows[1].contains("small"), "{listed}");

    let d = t.du(&["diff", o, n]);
    assert!(d.status.success(), "{}", String::from_utf8_lossy(&d.stderr));
    let d = String::from_utf8_lossy(&d.stdout).into_owned();
    let first = d.lines().nth(1).unwrap_or_default();
    assert!(
        first.trim_start().starts_with('+') && first.contains("/srv/data"),
        "{d}"
    );
    assert!(!d.contains("small"), "{d}");

    // A broken export names the byte where reading stopped.
    fs::write(&old, "[1,2,{},[{\"name\":\"/x\"}").unwrap();
    let bad = t.du(&["-f", o]);
    assert_eq!(bad.status.code(), Some(1));
    let err = String::from_utf8_lossy(&bad.stderr).into_owned();
    assert!(err.contains("ncdu export, byte "), "{err}");
}

#[test]
fn ncdu_binary_exports_list_and_diff_like_their_json_form() {
    let t = Tree::new("ncdu-bin");
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let bin = data.join("ncdu-2.9.1-export.ncdu");
    let json = data.join("ncdu-2.9.1-export.json");
    let (b, j) = (bin.to_str().unwrap(), json.to_str().unwrap());
    let out = t.du(&["-f", b, "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.contains(r#""disk":49152,"apparent":9009,"files":4,"links":1,"dirs":7,"excluded":1"#),
        "{out}"
    );
    // The same scan in both formats: nothing changed between them.
    let d = t.du(&["diff", j, b]);
    assert!(d.status.success(), "{}", String::from_utf8_lossy(&d.stderr));
    let d = String::from_utf8_lossy(&d.stdout).into_owned();
    assert!(!d.contains("/tmp/ncdu-fixture/"), "{d}");
    // A cut-off export says where it broke.
    let cut = t.root.join("cut.ncdu");
    fs::write(&cut, &fs::read(&bin).unwrap()[..200]).unwrap();
    let bad = t.du(&["-f", cut.to_str().unwrap()]);
    assert_eq!(bad.status.code(), Some(1));
    let err = String::from_utf8_lossy(&bad.stderr).into_owned();
    assert!(err.contains("ncdu binary export"), "{err}");
}

#[test]
fn ncdu_exports_written_by_du_read_back_with_the_scan_totals() {
    let t = Tree::new("ncdu-out");
    t.file("t/a/f", 40_000);
    t.file("t/a/b/g", 9_000);
    t.file("t/x.log", 70_000);
    let out = t.root.join("out.json");
    let o = out.to_str().unwrap();
    let w = t.du(&["--ncdu", o, "--exclude=*.log", "t"]);
    assert!(w.status.success(), "{}", String::from_utf8_lossy(&w.stderr));
    let text = fs::read_to_string(&out).unwrap();
    assert!(text.starts_with("[1,2,{\"progname\":\"rtrash\""), "{text}");
    assert!(text.contains("\"excluded\":\"pattern\""), "{text}");
    let back = t.du(&["-f", o, "--json"]);
    let live = t.du(&["--json", "--no-git", "--exclude=*.log", "t"]);
    let roots = |o: &std::process::Output| {
        let s = String::from_utf8_lossy(&o.stdout).into_owned();
        let start = s.find("\"disk\"").unwrap();
        let end = s[start..].find(",\"dirs\"").unwrap() + start;
        s[start..end].to_string()
    };
    assert_eq!(roots(&back), roots(&live));

    // To stdout, and only alone.
    let s = t.du(&["--ncdu=-", "t"]);
    assert!(String::from_utf8_lossy(&s.stdout).starts_with("[1,2,"));
    assert_eq!(t.du(&["--ncdu", o, "--json", "t"]).status.code(), Some(2));
    assert_eq!(t.du(&["--ncdu", o, "t", "t/a"]).status.code(), Some(2));
}

#[test]
fn ncdu_binary_exports_written_by_du_read_back_like_the_json_one() {
    let t = Tree::new("ncdu-bin-out");
    t.file("t/a/f", 40_000);
    t.file("t/a/b/g", 9_000);
    t.file("t/x.log", 70_000);
    let bin = t.root.join("out.ncdu");
    let json = t.root.join("out.json");
    let (b, j) = (bin.to_str().unwrap(), json.to_str().unwrap());
    let w = t.du(&["--ncdu-binary", b, "--exclude=*.log", "t"]);
    assert!(w.status.success(), "{}", String::from_utf8_lossy(&w.stderr));
    assert!(fs::read(&bin).unwrap().starts_with(b"\xbfncduEX1"));
    let w = t.du(&["--ncdu", j, "--exclude=*.log", "t"]);
    assert!(w.status.success(), "{}", String::from_utf8_lossy(&w.stderr));
    let from_bin = t.du(&["-f", b, "--json"]);
    let from_json = t.du(&["-f", j, "--json"]);
    assert!(
        from_bin.status.success(),
        "{}",
        String::from_utf8_lossy(&from_bin.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&from_bin.stdout).replace(b, "F"),
        String::from_utf8_lossy(&from_json.stdout).replace(j, "F")
    );

    // To stdout, and only alone.
    let s = t.du(&["--ncdu-binary=-", "t"]);
    assert!(s.stdout.starts_with(b"\xbfncduEX1"));
    let both = t.du(&["--ncdu", j, "--ncdu-binary", b, "t"]);
    assert_eq!(both.status.code(), Some(2));
    let err = t.du(&["--ncdu-binary", b, "--json", "t"]);
    assert_eq!(err.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&err.stderr).contains("--ncdu-binary writes"));
    assert_eq!(
        t.du(&["--ncdu-binary", b, "t", "t/a"]).status.code(),
        Some(2)
    );
}

#[test]
fn devices_list_mounted_file_systems_and_take_no_path() {
    let t = Tree::new("devices");
    let out = t.du(&["--devices"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        text.lines().next().unwrap_or_default().contains("USE%"),
        "{text}"
    );
    assert!(
        text.lines().count() >= 2,
        "at least one file system: {text}"
    );
    let json = t.du(&["--devices", "--json"]);
    assert!(String::from_utf8_lossy(&json.stdout).starts_with("{\"devices\":[{\"mount\":"));
    assert_eq!(t.du(&["--devices", "."]).status.code(), Some(2));
    assert_eq!(t.du(&["--devices", "--ncdu=-"]).status.code(), Some(2));
}

#[test]
fn stacks_fold_each_directory_by_its_own_size() {
    let t = Tree::new("stacks");
    t.file("t/top", 3);
    t.file("t/a/g", 5);
    t.file("t/a/b/f", 10);
    t.file("t/x;y/h", 7);
    let scan = t.du(&["--stacks", "--apparent", "--no-git", "t"]);
    assert!(
        scan.status.success(),
        "{}",
        String::from_utf8_lossy(&scan.stderr)
    );
    let text = String::from_utf8(scan.stdout).unwrap();
    let root = t.root.join("t").display().to_string();
    let want = format!("{root} 3\n{root};a 5\n{root};a;b 10\n{root};x_y 7\n");
    assert_eq!(text, want);

    // A snapshot folds the same way.
    let snap = t.root.join("snap");
    let w = t.du(&["-o", snap.to_str().unwrap(), "--no-git", "t"]);
    assert!(w.status.success(), "{}", String::from_utf8_lossy(&w.stderr));
    let back = t.du(&["-f", snap.to_str().unwrap(), "--stacks", "--apparent"]);
    assert_eq!(String::from_utf8(back.stdout).unwrap(), want);

    // Disk weights add up to the root's disk total.
    let disk = String::from_utf8(t.du(&["--stacks", "--no-git", "t"]).stdout).unwrap();
    let sum: u64 = disk
        .lines()
        .map(|l| l.rsplit(' ').next().unwrap().parse::<u64>().unwrap())
        .sum();
    let json = t.json(&["--no-git", "t"]);
    let total = json.split("\"disk\":").nth(1).unwrap();
    let total: u64 = total[..total.find(',').unwrap()].parse().unwrap();
    assert_eq!(sum, total, "{disk}\n{json}");

    for bad in [&["--stacks", "--json"][..], &["--stacks", "--sort=items"]] {
        assert_eq!(t.du(bad).status.code(), Some(2), "{bad:?}");
    }
}

#[test]
fn unique_adds_a_column_and_a_shared_summary() {
    let t = Tree::new("unique");
    t.file("u/a/big", 200_000);
    t.file("u/small", 3000);
    let out = t.du(&["--unique", "--no-git", "u"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("UNIQUE"), "{text}");
    assert!(text.contains(" unique, "), "{text}");
    assert!(text.contains("Shared extents: "), "{text}");
    // The plain listing keeps its shape.
    let plain = String::from_utf8(t.du(&["--no-git", "u"]).stdout).unwrap();
    assert!(
        !plain.contains("UNIQUE") && !plain.contains("Shared extents"),
        "{plain}"
    );
    let json = t.json(&["--unique", "--no-git", "u"]);
    assert!(
        json.contains("\"unique\":") && json.contains("\"shared\":{"),
        "{json}"
    );
    for bad in [
        &["--unique", "--stacks"][..],
        &["--unique", "-o", "-"],
        &["--unique", "-i"],
    ] {
        assert_eq!(t.du(bad).status.code(), Some(2), "{bad:?}");
    }
}

#[test]
fn shared_counts_shared_extents_once_in_sizes() {
    let t = Tree::new("shared");
    t.file("s/a/big", 200_000);
    t.file("s/small", 3000);
    let out = t.du(&["--shared", "--no-git", "s"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains(" on disk with shared extents once ("),
        "{text}"
    );
    let json = t.json(&["--shared", "--no-git", "s"]);
    assert!(json.contains("\"disk_all\":"), "{json}");
    let plain = t.json(&["--no-git", "s"]);
    assert!(!plain.contains("\"disk_all\""), "{plain}");
    for bad in [
        &["--shared", "--stacks"][..],
        &["--shared", "-o", "-"],
        &["--shared", "-i"],
        &["--shared", "--types"],
        &["--shared", "--ncdu=-"],
    ] {
        assert_eq!(t.du(bad).status.code(), Some(2), "{bad:?}");
    }
}

// The Apple File System refuses names that are not UTF-8 (EILSEQ), so
// this runs on Linux only, like the CLI tests for such names.
#[cfg(target_os = "linux")]
#[test]
fn exclude_matches_names_that_are_not_utf8() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let t = Tree::new("bytes");
    t.file("b/keep", 4000);
    let odd = t.root.join("b").join(OsStr::from_bytes(b"n\xe9.log"));
    fs::write(&odd, vec![b'x'; 300_000]).unwrap();
    let run = |extra: &[&OsStr]| {
        let out = Command::new(bin())
            .args(["du", "--json", "--no-git"])
            .args(extra)
            .arg("b")
            .current_dir(&t.root)
            .env("XDG_DATA_HOME", t.root.join(".xdg"))
            .env("XDG_CONFIG_HOME", t.root.join(".cfg"))
            .env("HOME", &t.root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    let plain = run(&[]);
    assert!(plain.contains(".log"), "{plain}");
    // The pattern carries the same raw byte as the name.
    let excl = run(&[OsStr::from_bytes(b"--exclude=n\xe9*")]);
    assert!(!excl.contains(".log"), "{excl}");
    assert!(excl.contains("\"excluded\":1"), "{excl}");
    // From a pattern file too.
    let pf = t.root.join("pats");
    fs::write(&pf, b"*\xe9.log\n").unwrap();
    let mut arg = b"--exclude-from=".to_vec();
    arg.extend_from_slice(pf.as_os_str().as_bytes());
    let from = run(&[OsStr::from_bytes(&arg)]);
    assert!(!from.contains(".log"), "{from}");
}

#[test]
fn no_delete_needs_the_explorer() {
    let t = Tree::new("nodelete");
    t.file("a.bin", 10);
    let out = t.du(&["--no-delete"]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--no-delete applies to --interactive"),
        "{err}"
    );
    assert!(out.stdout.is_empty());
}

#[test]
fn si_prints_decimal_units_and_leaves_json_in_bytes() {
    let t = Tree::new("si");
    t.file("d/a.bin", 5000);
    let text = String::from_utf8(t.du(&["--no-git", "--apparent", "--si"]).stdout).unwrap();
    assert!(text.contains("5.0 kB"), "{text}");
    assert!(!text.contains("KiB"), "{text}");
    let text = String::from_utf8(t.du(&["--no-git", "--apparent"]).stdout).unwrap();
    assert!(text.contains("4.9 KiB"), "{text}");
    let j = t.json(&["--no-git", "--si"]);
    assert!(j.contains("\"apparent\":5000"), "{j}");
}

/// Ctrl-C while `du` waits on `-X -` stops the scan before it starts, so
/// the result is the PATH's own files, marked partial, with exit 130.
#[cfg(unix)]
#[test]
fn ctrl_c_keeps_a_partial_listing() {
    let t = Tree::new("ctrlc");
    t.file("top.bin", 7000);
    t.file("sub/deep/far.bin", 90_000);
    let interrupted = |extra: &[&str]| {
        let mut child = Command::new(bin())
            .args(["du", "--no-git", "-X", "-"])
            .args(extra)
            .arg(&t.root)
            .current_dir(&t.root)
            .env("XDG_DATA_HOME", t.root.join(".xdg"))
            .env("XDG_CONFIG_HOME", t.root.join(".cfg"))
            .env("HOME", &t.root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // Long enough for the handler to be in place before the signal.
        std::thread::sleep(std::time::Duration::from_millis(400));
        let sent = Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .unwrap();
        assert!(sent.success());
        drop(child.stdin.take());
        child.wait_with_output().unwrap()
    };
    let out = interrupted(&[]);
    let text = String::from_utf8_lossy(&out.stdout);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(130), "{text}{err}");
    assert!(text.contains("top.bin"), "{text}");
    assert!(!text.contains("far.bin"), "{text}");
    assert!(text.contains("Partial scan"), "{text}");
    assert!(err.contains("interrupted"), "{err}");

    let out = interrupted(&["--json"]);
    assert_eq!(out.status.code(), Some(130));
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"partial\":true"));

    let out = interrupted(&["--print0"]);
    assert_eq!(out.status.code(), Some(130));
    assert!(out.stdout.is_empty());
}

#[test]
fn types_total_each_extension_and_the_rest() {
    let t = Tree::new("types");
    t.file("a/one.RS", 30_000);
    t.file("b/two.rs", 20_000);
    t.file("big.iso", 100_000);
    t.file("Makefile", 10);
    t.file(".bashrc", 10);
    t.file("log.1", 5);
    let json = t.json(&["--types", "--apparent"]);
    let types = &json[json.find("\"types\":[").expect("types key")..];
    // By apparent size: iso, rs (both cases), then the rest.
    let order: Vec<&str> = types
        .split("\"type\":\"")
        .skip(1)
        .map(|s| &s[..s.find('"').unwrap()])
        .collect();
    assert_eq!(&order[..2], ["iso", "rs"], "{types}");
    assert!(types.contains("{\"type\":\"rs\",\"disk\":"), "{types}");
    assert!(types.contains("\"apparent\":50000,\"files\":2}"), "{types}");
    // Makefile and .bashrc have no extension; log.1 has "1".
    assert!(types.contains("\"apparent\":20,\"files\":2}"), "{types}");
    assert!(order.contains(&"1"), "{types}");

    let out = t.du(&["--types", "-n", "2"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Largest file types (by disk use)"), "{text}");
    assert!(text.contains("TYPE"), "{text}");
    assert!(text.contains("(2 more types)"), "{text}");
    assert!(!text.contains("Largest directories"), "{text}");
    assert!(text.contains("Total "), "{text}");

    let out = t.du(&["--types", "--sort=mtime"]);
    assert_eq!(out.status.code(), Some(2));
    let out = t.du(&["--types", "--print0"]);
    assert_eq!(out.status.code(), Some(2));
    // A plain scan keeps its JSON shape.
    assert!(!t.json(&[]).contains("\"types\""));
}

/// Under a cap on the address space the walk threads may not get their
/// full stacks. The walk recurses once per level, so with a smaller stack
/// it must enter fewer levels: falling back to small stacks with the full
/// depth overflowed the stack near 600 levels and aborted.
#[cfg(target_os = "linux")]
#[test]
fn a_capped_address_space_walks_fewer_levels_instead_of_overflowing() {
    use std::os::unix::process::CommandExt;
    let t = Tree::new("capped-stack");
    let levels = 1100;
    let mut deep = t.root.join("chain");
    for _ in 0..levels {
        deep.push("d");
    }
    fs::create_dir_all(&deep).unwrap();
    let mut cmd = Command::new(bin());
    cmd.args(["du", "-n", "1", "chain"])
        .current_dir(&t.root)
        .env("XDG_DATA_HOME", t.root.join(".xdg"))
        .env("XDG_CONFIG_HOME", t.root.join(".cfg"))
        .env("HOME", &t.root)
        // Eight walk threads of 256 MiB fill a 2 GiB cap on their own.
        .env("RAYON_NUM_THREADS", "8");
    // SAFETY: setrlimit is async-signal-safe and touches no memory of the
    // parent, as a hook run between fork and exec must.
    unsafe {
        cmd.pre_exec(|| {
            let cap = libc::rlimit {
                rlim_cur: 2 << 30,
                rlim_max: 2 << 30,
            };
            if libc::setrlimit(libc::RLIMIT_AS, &cap) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let out = cmd.output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{:?}: {err}", out.status);
    // A quarter of the cap gives each thread 64 MiB, a quarter of the full
    // stack, and so a quarter of the full 4096 levels.
    assert!(err.contains("1 directory past 1024 levels"), "{err}");
    fs::remove_dir_all(&t.root).unwrap();
}
