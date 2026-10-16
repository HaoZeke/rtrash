//! Non-UTF-8 and odd file names, and payloads that vanish under their
//! records, through every listing and removal command. Names come from a
//! fixed-seed xorshift, so a failure replays exactly. Each command must
//! finish without a panic, and restore must bring back the exact bytes.
#![cfg(target_os = "linux")]

mod common;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rtrash")
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// A file name of 1..12 bytes: invalid UTF-8, a decomposed accent,
/// control bytes, spaces, a leading dash or `%`, never `/` or NUL.
fn odd_name(r: &mut Rng) -> OsString {
    let pieces: [&[u8]; 10] = [
        b"\xff",
        b"\xc3",
        b"e\xcc\x81",
        b"\n",
        b"\t",
        b" ",
        b"%2F",
        b"-",
        b"a",
        b"\xe2\x80\xae",
    ];
    loop {
        let mut v = Vec::new();
        for _ in 0..1 + r.below(5) {
            v.extend_from_slice(pieces[r.below(pieces.len())]);
        }
        if v != b"." && v != b".." {
            return OsString::from_vec(v);
        }
    }
}

struct Sb {
    root: PathBuf,
}
impl Sb {
    fn new(tag: &str) -> Sb {
        let root = common::scratch_dir("odd", tag);
        fs::create_dir_all(root.join("w")).unwrap();
        Sb { root }
    }
    fn work(&self) -> PathBuf {
        self.root.join("w")
    }
    fn trash(&self) -> PathBuf {
        self.root.join("xdg/Trash")
    }
    fn run(&self, args: &[&OsStr]) -> Output {
        let out = Command::new(bin())
            .args(args)
            .current_dir(self.work())
            .env("XDG_DATA_HOME", self.root.join("xdg"))
            .env("XDG_CONFIG_HOME", self.root.join(".config"))
            .env("HOME", &self.root)
            .env("RTRASH_MACOS_TRASH", "freedesktop")
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.code() != Some(101) && !err.contains("panicked"),
            "{args:?} panicked: {err}"
        );
        out
    }
    fn s(&self, args: &[&str]) -> Output {
        let a: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        self.run(&a)
    }
}

impl Drop for Sb {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn listing_commands(sb: &Sb) {
    for args in [
        &["list", "--home-only"][..],
        &["list", "--home-only", "--json"],
        &["list", "--home-only", "--size", "--sort=name"],
        &[
            "list",
            "--home-only",
            "--format={name}\\0{path}\\0{size}\\0",
        ],
        &["list", "--home-only", "--glob=*"],
        &["list", "--home-only", "--regex=."],
        &["status", "--home-only"],
        &["status", "--home-only", "--json"],
        &["empty", "--home-only", "-n", "-v", "--all"],
    ] {
        let out = sb.s(args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn odd_names_survive_put_list_restore_and_empty() {
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    for round in 0..6 {
        let sb = Sb::new(&format!("r{round}"));
        let mut names: Vec<OsString> = Vec::new();
        while names.len() < 6 {
            let n = odd_name(&mut r);
            if !names.contains(&n) {
                names.push(n);
            }
        }
        for (k, n) in names.iter().enumerate() {
            let p = sb.work().join(n);
            fs::write(&p, n.as_bytes()).unwrap();
            if k % 3 == 0 {
                // A directory named oddly, holding an oddly named file.
                fs::remove_file(&p).unwrap();
                fs::create_dir(&p).unwrap();
                fs::write(p.join(n), b"x").unwrap();
            }
            let out = sb.run(&[
                OsStr::new("put"),
                OsStr::new("-r"),
                OsStr::new("--"),
                p.as_os_str(),
            ]);
            assert!(
                out.status.success(),
                "put {n:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        listing_commands(&sb);
        // The format template writes the raw bytes back out.
        let out = sb.s(&["list", "--home-only", "--format={path}\\0"]);
        let got: Vec<&[u8]> = out
            .stdout
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .collect();
        for n in &names {
            let want = sb.work().join(n);
            assert!(
                got.iter().any(|g| *g == want.as_os_str().as_bytes()),
                "{n:?} missing from {got:?}"
            );
        }
        // Restore the first by its exact path; the bytes come back.
        let first = sb.work().join(&names[1]);
        let out = sb.run(&[
            OsStr::new("restore"),
            OsStr::new("--home-only"),
            first.as_os_str(),
        ]);
        assert!(
            out.status.success(),
            "restore {first:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(fs::read(&first).unwrap(), names[1].as_bytes());

        // Pull half the payloads out from under their records.
        let files = sb.trash().join("files");
        let mut gone = 0;
        for (k, e) in fs::read_dir(&files).unwrap().flatten().enumerate() {
            if k % 2 == 0 {
                let p = e.path();
                if p.is_dir() {
                    fs::remove_dir_all(&p).unwrap();
                } else {
                    fs::remove_file(&p).unwrap();
                }
                gone += 1;
            }
        }
        assert!(gone > 0);
        listing_commands(&sb);
        let out = sb.s(&["list", "--home-only", "--json"]);
        let json = String::from_utf8_lossy(&out.stdout);
        assert_eq!(json.matches("\"missing\":true").count(), gone, "{json}");
        // Restoring everything restores what is left and reports the rest.
        let _ = sb.s(&["restore", "--home-only", "--all", "--glob=*"]);
        let _ = sb.s(&["rm", "--home-only", "*"]);
        let out = sb.s(&["empty", "--home-only", "-v", "--all"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            fs::read_dir(sb.trash().join("info"))
                .map(|d| d.count())
                .unwrap_or(0)
                == 0
        );
    }
}
