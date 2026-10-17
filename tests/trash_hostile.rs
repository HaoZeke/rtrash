//! Property test of the trash commands against hostile trash contents.
//!
//! Each round builds a sandbox: sentinel files outside the trash, real
//! items trashed by `put`, and forged records with odd names (`""`, `.`,
//! `..`, spaces, a leading dash), odd `Path=` values (relative, empty,
//! `..`, the trash itself, the directory around it, a real item's payload,
//! `%00`, escapes, broken `%`), payloads that are symlinks out of the
//! trash or named pipes, records that are symlinks or pipes, and a
//! garbled `directorysizes`. It then runs list, status, rm, empty and
//! restore in a random order, each under a deadline, and checks:
//!
//! - no command hangs;
//! - a sentinel changes only when a record names it (or a directory
//!   above it) and restore ran with `-f`;
//! - every real item is whole, in the trash with its record or back at
//!   its original path;
//! - the trash and its `files/` and `info/` stay real directories;
//! - no restore staging copy is left behind.
//!
//! The generator is a fixed-seed xorshift, so a failure replays exactly.
//! `RTRASH_FUZZ_ROUNDS` sets the number of rounds.
#![cfg(target_os = "linux")]

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Let the child behind `cmd` map at most 2 GiB: past that, an allocation
/// fails and the child aborts.
fn cap_memory(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
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
}

fn rounds() -> usize {
    std::env::var("RTRASH_FUZZ_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24)
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// A file, a directory or a symlink, as found without following links.
#[derive(Debug, PartialEq, Eq, Clone)]
enum Node {
    File(Vec<u8>),
    Dir,
    Link(PathBuf),
    Other,
}

/// Every path under `dir` with what it is, links not followed.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Node> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(meta) = fs::symlink_metadata(&p) else {
            continue;
        };
        let node = if meta.file_type().is_symlink() {
            Node::Link(fs::read_link(&p).unwrap_or_default())
        } else if meta.is_dir() {
            if let Ok(rd) = fs::read_dir(&p) {
                stack.extend(rd.flatten().map(|e| e.path()));
            }
            Node::Dir
        } else if meta.is_file() {
            Node::File(fs::read(&p).unwrap_or_default())
        } else {
            Node::Other
        };
        out.insert(p, node);
    }
    out
}

fn mkfifo(p: &Path) {
    assert!(Command::new("mkfifo").arg(p).status().unwrap().success());
}

struct Sandbox {
    root: PathBuf,
    seed: u64,
}

impl Sandbox {
    fn data(&self) -> PathBuf {
        self.root.join("xdg")
    }
    fn trash(&self) -> PathBuf {
        self.root.join("xdg/Trash")
    }
    fn work(&self) -> PathBuf {
        self.root.join("work")
    }
    fn outside(&self) -> PathBuf {
        self.root.join("outside")
    }

    fn run(&self, args: &[String]) -> Output {
        let out_path = self.root.join("run.out");
        let err_path = self.root.join("run.err");
        let mut cmd = common::rtrash(&self.root);
        // A child that reads without bound (a sizes cache linked to
        // /dev/zero, say) stops at 2 GiB instead of filling the memory.
        cap_memory(&mut cmd);
        let mut child = cmd
            .args(args)
            .current_dir(self.work())
            .env("XDG_DATA_HOME", self.data())
            .stdin(Stdio::null())
            .stdout(fs::File::create(&out_path).unwrap())
            .stderr(fs::File::create(&err_path).unwrap())
            .spawn()
            .unwrap();
        let start = Instant::now();
        let status = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break s;
            }
            if start.elapsed() > Duration::from_secs(20) {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "seed {:#x}: rtrash {args:?} still running after 20 s",
                    self.seed
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        Output {
            status,
            stdout: fs::read(&out_path).unwrap(),
            stderr: fs::read(&err_path).unwrap(),
        }
    }
}

/// A real item: its original path, name in the trash and contents.
struct Real {
    original: PathBuf,
    name: String,
    tree: BTreeMap<PathBuf, Node>,
}

/// The tree under `p` with paths made relative to it.
fn relative_tree(p: &Path) -> BTreeMap<PathBuf, Node> {
    snapshot(p)
        .into_iter()
        .map(|(k, v)| (k.strip_prefix(p).unwrap().to_path_buf(), v))
        .collect()
}

/// The directory every round's sandbox sits in. A record may name a
/// sandbox itself, and a rename puts that item beside it, so the rounds
/// get a parent of their own, removed as a whole.
fn container() -> PathBuf {
    std::env::temp_dir().join(format!("rtrash-hostile-{}", std::process::id()))
}

fn round(seed: u64) {
    let mut rng = Rng(seed);
    let root = container().join(format!("{seed:x}"));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("work")).unwrap();
    let root = fs::canonicalize(&root).unwrap();
    let sb = Sandbox { root, seed };
    let (out, work, trash) = (sb.outside(), sb.work(), sb.trash());

    // Sentinels: outside the trash, and in the directory commands run in.
    fs::create_dir_all(out.join("d/deep")).unwrap();
    for i in 0..3 {
        fs::write(out.join(format!("s{i}")), format!("sentinel {i}")).unwrap();
    }
    fs::write(out.join("d/deep/inner"), b"inner").unwrap();
    fs::write(work.join("rel"), b"cwd sentinel").unwrap();

    // Real items, trashed by put.
    fs::write(work.join("real-0"), b"real 0").unwrap();
    fs::write(work.join("real-1"), b"real 1").unwrap();
    fs::create_dir_all(work.join("real-dir/sub")).unwrap();
    fs::write(work.join("real-dir/sub/f"), b"real dir").unwrap();
    let mut reals = Vec::new();
    for n in ["real-0", "real-1", "real-dir"] {
        reals.push(Real {
            original: work.join(n),
            name: n.to_string(),
            tree: relative_tree(&work.join(n)),
        });
    }
    let put = sb.run(&["put", "-r", "real-0", "real-1", "real-dir"].map(String::from));
    assert!(put.status.success(), "seed {seed:#x}: put failed");

    // Forged records.
    let names = [
        "h-plain",
        ".",
        "..",
        "",
        "h-sp ace",
        "h-\u{fc}n\u{ef}",
        "-h-dash",
        ".h-hidden",
    ];
    let o = out.display().to_string();
    let w = work.display().to_string();
    let t = trash.display().to_string();
    let d = sb.data().display().to_string();
    let r = sb.root.display().to_string();
    let paths: Vec<String> = vec![
        format!("{o}/s0"),
        format!("{o}/s1"),
        format!("{o}/d"),
        format!("{o}/new"),
        format!("{w}/real-0"),
        format!("{w}/real-dir"),
        t.clone(),
        d.clone(),
        r.clone(),
        format!("{t}/files/real-0"),
        format!("{t}/files/real-dir/sub"),
        format!("{t}/info/real-1.trashinfo"),
        format!("{t}/files/h-new"),
        "rel".into(),
        "".into(),
        "../../outside/s2".into(),
        "/".into(),
        format!("{o}/a%00b"),
        format!("{o}/x%1B%5B2J"),
        format!("{o}/bad%zz%4"),
        format!("{o}/s1/below-a-file"),
    ];
    fs::create_dir_all(out.join("records")).unwrap();
    for k in 0..1 + rng.below(6) {
        let base = *rng.pick(&names);
        let name = match base {
            "" | "." | ".." => base.to_string(),
            b => format!("{b}{k}"),
        };
        let path = rng.pick(&paths).clone();
        let date = *rng.pick(&[
            "DeletionDate=2026-01-01T00:00:00\n",
            "DeletionDate=2000-01-01T00:00:00\n",
            "",
            "DeletionDate=garbage\n",
        ]);
        let body = format!("[Trash Info]\nPath={path}\n{date}");
        let payload = trash.join("files").join(&name);
        let info = trash.join("info").join(format!("{name}.trashinfo"));
        if matches!(base, "" | "." | "..") {
            // files/ joined with these is the trash itself: no payload.
        } else {
            match rng.below(6) {
                0 => fs::write(&payload, b"forged").unwrap(),
                1 => {
                    fs::create_dir_all(payload.join("x")).unwrap();
                    fs::write(payload.join("x/y"), b"forged").unwrap();
                }
                2 => std::os::unix::fs::symlink(out.join("s0"), &payload).unwrap(),
                3 => std::os::unix::fs::symlink(out.join("d"), &payload).unwrap(),
                4 => mkfifo(&payload),
                _ => {}
            }
        }
        if info.symlink_metadata().is_ok() {
            continue;
        }
        match rng.below(5) {
            0 => {
                let rec = out.join(format!("records/{k}"));
                fs::write(&rec, &body).unwrap();
                std::os::unix::fs::symlink(&rec, &info).unwrap();
            }
            1 => mkfifo(&info),
            _ => fs::write(&info, &body).unwrap(),
        }
    }
    let sizes = trash.join("directorysizes");
    let kind = rng.below(4);
    if kind < 2 {
        fs::remove_file(&sizes).unwrap_or_default();
    }
    match kind {
        0 => mkfifo(&sizes),
        1 => std::os::unix::fs::symlink("/dev/zero", &sizes).unwrap(),
        2 => fs::write(&sizes, "1 2 %2E%2E\nx y z\n9 9 \n4096 0 real-dir\n").unwrap(),
        _ => {}
    }

    let before_out = snapshot(&out);
    let before_work_rel = fs::read(work.join("rel")).unwrap();

    // Commands, in a random order; -f only in some rounds.
    let force = rng.below(2) == 0;
    let mut cmds: Vec<Vec<String>> = vec![
        vec!["list".into()],
        vec!["list".into(), "--size".into()],
        vec!["status".into()],
        vec!["rm".into(), "h-*".into()],
        vec!["rm".into(), format!("{o}/s0")],
        vec!["empty".into(), "1".into()],
        vec!["restore".into(), "--all".into(), "--conflict=skip".into()],
        vec!["restore".into(), "--all".into(), "--conflict=rename".into()],
        vec!["restore".into(), "--all".into()],
    ];
    if force {
        cmds.push(vec!["restore".into(), "--all".into(), "-f".into()]);
    }
    for i in (1..cmds.len()).rev() {
        let j = rng.below(i + 1);
        cmds.swap(i, j);
    }
    let trace = std::env::var_os("RTRASH_HOSTILE_TRACE").is_some();
    for c in &mut cmds {
        // Never touch the volume trashes of the machine the test runs on.
        c.insert(1, "--home-only".into());
    }
    for c in &cmds {
        let o = sb.run(c);
        if trace {
            eprintln!(
                "$ rtrash {c:?} -> {:?}\n{}{}",
                o.status.code(),
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
        }
    }

    // Absolute places a record names: under -f, restore may replace these
    // and whatever sits below them.
    let named: Vec<PathBuf> = paths
        .iter()
        .filter(|p| p.starts_with('/'))
        .map(|p| PathBuf::from(std::ffi::OsStr::from_bytes(&rtrash::util::url_decode(p))))
        .collect();
    let may_change = |p: &Path| force && named.iter().any(|n| p.starts_with(n));

    for (p, node) in &before_out {
        if may_change(p) {
            continue;
        }
        let now = snapshot(p).remove(p);
        assert_eq!(
            now.as_ref(),
            Some(node),
            "seed {seed:#x}: {} changed",
            p.display()
        );
    }
    assert_eq!(
        fs::read(work.join("rel")).ok(),
        Some(before_work_rel),
        "seed {seed:#x}: a relative Path= reached the working directory"
    );
    for real in &reals {
        let in_trash = trash.join("files").join(&real.name);
        let record = trash.join("info").join(format!("{}.trashinfo", real.name));
        let whole_in_trash = relative_tree(&in_trash) == real.tree && record.exists();
        // --conflict=rename may have put it back as NAME.N.
        let parent = real.original.parent().unwrap();
        let whole_back = fs::read_dir(parent).unwrap().flatten().any(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            let renamed = n
                .strip_prefix(&format!("{}.", real.name))
                .is_some_and(|k| k.parse::<u32>().is_ok());
            (n == real.name || renamed) && relative_tree(&e.path()) == real.tree
        });
        assert!(
            whole_in_trash || whole_back,
            "seed {seed:#x}: real item {} lost or changed",
            real.name
        );
    }
    for dir in [trash.clone(), trash.join("files"), trash.join("info")] {
        let meta = fs::symlink_metadata(&dir);
        assert!(
            meta.is_ok_and(|m| m.is_dir()),
            "seed {seed:#x}: {} is no longer a directory",
            dir.display()
        );
    }
    for (p, _) in snapshot(&sb.root) {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
        assert!(
            !name.is_some_and(|n| n.starts_with(".rtrash-restore-")),
            "seed {seed:#x}: staging copy left at {}",
            p.display()
        );
    }
    fs::remove_dir_all(container()).unwrap();
}

#[test]
fn trash_commands_keep_real_items_and_outside_files_against_hostile_records() {
    if let Some(seed) = std::env::var("RTRASH_HOSTILE_SEED")
        .ok()
        .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
    {
        return round(seed);
    }
    let mut seeds = Rng(0x51de_c0de_2026_1011);
    for _ in 0..rounds() {
        round(seeds.next());
    }
}
