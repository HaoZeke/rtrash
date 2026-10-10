//! `rtrash du`: sizes, ordering, hard links, symlinks, filters and output modes.

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
            root: fs::canonicalize(&root).unwrap(),
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
        .map(|s| s.split('"').next().unwrap().to_string())
        .collect()
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

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
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

#[test]
fn age_filters_use_newest_mtime() {
    let t = Tree::new("age");
    let old = t.file("old/stale.bin", 300_000);
    let new = t.file("new/fresh.bin", 200_000);
    let long_ago = SystemTime::now() - Duration::from_secs(90 * 86_400);
    File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(long_ago)
        .unwrap();
    File::open(t.root.join("old"))
        .unwrap()
        .set_modified(long_ago)
        .unwrap();
    let j = t.json(&["--older-than=30"]);
    assert_eq!(paths_in(&j, "files"), [s(&old)], "{j}");
    assert_eq!(paths_in(&j, "dirs"), [s(&t.root.join("old"))], "{j}");
    let j = t.json(&["--newer-than", "30"]);
    assert_eq!(paths_in(&j, "files"), [s(&new)], "{j}");
}

#[test]
fn print0_and_text_output() {
    let t = Tree::new("print0");
    let f = t.file("d/f.bin", 50_000);
    let out = t.du(&["--files", "--print0"]);
    assert!(out.status.success());
    assert_eq!(out.stdout, format!("{}\0", f.display()).into_bytes());

    let out = t.du(&["-n", "5"]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("Largest directories (by disk use)"), "{text}");
    assert!(
        text.contains(&format!("{}/", t.root.join("d").display())),
        "{text}"
    );
    assert!(text.contains("Total "), "{text}");
}

#[test]
fn usage_errors_exit_2() {
    let t = Tree::new("usage");
    assert_eq!(t.du(&["--bogus"]).status.code(), Some(2));
    assert_eq!(t.du(&["-n", "x"]).status.code(), Some(2));
    assert_eq!(t.du(&["--older-than=-1"]).status.code(), Some(2));
    assert_eq!(t.du(&["--json", "--print0"]).status.code(), Some(2));
    assert_eq!(t.du(&["missing-path"]).status.code(), Some(1));
}
