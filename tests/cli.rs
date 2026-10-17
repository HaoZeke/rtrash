//! The FreeDesktop trash end to end: put, list, restore, empty and rm.
//! Windows sends put to the Recycle Bin instead, so these run on Unix only.
#![cfg(unix)]

mod common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rtrash")
}

/// Isolated sandbox: its own XDG_DATA_HOME and working directory so parallel
/// tests never share a trash.
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root = common::scratch_dir("test", tag);
        fs::create_dir_all(root.join("work")).unwrap();
        // put records originals with symlinked parents resolved (on macOS
        // the temp dir sits under /var, a symlink to /private/var), so the
        // expected paths use the resolved root too.
        let root = fs::canonicalize(&root).unwrap();
        Sandbox { root }
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    fn trash(&self) -> PathBuf {
        self.root.join("xdg/Trash")
    }

    fn run(&self, args: &[&str]) -> Output {
        common::rtrash(&self.root)
            .args(args)
            .current_dir(self.work())
            .output()
            .expect("failed to run rtrash")
    }

    /// Like `run`, with `envs` set on top.
    fn run_env(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
        common::rtrash(&self.root)
            .args(args)
            .current_dir(self.work())
            .envs(envs.iter().copied())
            .output()
            .expect("failed to run rtrash")
    }

    /// Like `run`, with `RTRASH_NO_PERMANENT=1`.
    fn run_guarded(&self, args: &[&str]) -> Output {
        common::rtrash(&self.root)
            .args(args)
            .current_dir(self.work())
            .env("RTRASH_NO_PERMANENT", "1")
            .env_remove("RTRASH_MAX_SIZE")
            .output()
            .expect("failed to run rtrash")
    }

    /// Like `run`, but feeds `stdin` to the child (interactive restore selection).
    fn run_with_stdin(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = common::rtrash(&self.root)
            .args(args)
            .current_dir(self.work())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn rtrash");
        {
            let mut w = child.stdin.take().expect("stdin");
            w.write_all(stdin.as_bytes()).unwrap();
        }
        child.wait_with_output().expect("failed to wait rtrash")
    }

    /// Like `run`, but a child still running after `secs` is killed and
    /// the test fails, so a hang shows as a failure. Where the platform
    /// allows an address-space cap, the child may map at most 2 GiB, so a
    /// read without bound fails the test instead of filling the machine's
    /// memory.
    fn run_within(&self, secs: u64, args: &[&str]) -> Output {
        let out_path = self.root.join("run-within.out");
        let err_path = self.root.join("run-within.err");
        let mut cmd = common::rtrash(&self.root);
        cap_memory(&mut cmd);
        let mut child = cmd
            .args(args)
            .current_dir(self.work())
            .stdin(Stdio::null())
            .stdout(fs::File::create(&out_path).unwrap())
            .stderr(fs::File::create(&err_path).unwrap())
            .spawn()
            .expect("failed to spawn rtrash");
        let start = std::time::Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if start.elapsed() > std::time::Duration::from_secs(secs) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("rtrash {args:?} still running after {secs} s");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        Output {
            status,
            stdout: fs::read(&out_path).unwrap(),
            stderr: fs::read(&err_path).unwrap(),
        }
    }

    fn touch(&self, name: &str) -> PathBuf {
        let p = self.work().join(name);
        fs::write(&p, b"payload").unwrap();
        p
    }

    /// Run `empty` pinned to this sandbox's trash dir; a bare `empty` scans
    /// every mounted volume's trash and would purge the host's real trash.
    fn empty(&self, extra: &[&str]) -> Output {
        let pin = format!("--trash-dir={}", self.trash().display());
        let mut args: Vec<&str> = vec!["empty", &pin];
        args.extend_from_slice(extra);
        self.run(&args)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn trash_names(sb: &Sandbox) -> Vec<String> {
    match fs::read_dir(sb.trash().join("files")) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// A reader that closes stdout early, as `| grep -q` does, ends rtrash
/// quietly: no panic text, no abort, as coreutils behave.
#[test]
fn a_closed_stdout_pipe_ends_quietly() {
    let sb = Sandbox::new("epipe");
    for i in 0..50 {
        sb.touch(&format!("f{i}.txt"));
        assert!(sb.run(&["put", &format!("f{i}.txt")]).status.success());
    }
    for args in [&["list"][..], &["list", "--json"], &["status", "--json"]] {
        let mut child = common::rtrash(&sb.root)
            .args(args)
            .current_dir(sb.work())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // Close the read end before rtrash writes anything.
        drop(child.stdout.take());
        let out = child.wait_with_output().unwrap();
        let err = stderr_of(&out);
        assert!(!err.contains("panicked"), "{args:?}: {err}");
        assert!(!err.contains("Broken pipe"), "{args:?}: {err}");
        // 0 when the output fitted before the close, 141 after it; never a
        // signal (abort) or the panic status 101.
        assert!(
            matches!(out.status.code(), Some(0) | Some(141)),
            "{args:?}: {:?} {err}",
            out.status
        );
    }
}

#[test]
fn put_moves_file_and_writes_info() {
    let sb = Sandbox::new("put-basic");
    let f = sb.touch("hello.txt");
    let out = sb.run(&["put", "hello.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!f.exists());
    assert!(sb.trash().join("files/hello.txt").exists());
    let info = fs::read_to_string(sb.trash().join("info/hello.txt.trashinfo")).unwrap();
    assert!(info.starts_with("[Trash Info]\n"), "{info}");
    assert!(info.contains(&format!("Path={}", f.display())), "{info}");
    assert!(info.contains("DeletionDate="), "{info}");
}

#[test]
fn put_refuses_the_trash_and_what_contains_or_sits_in_it() {
    let sb = Sandbox::new("put-own-trash");
    fs::write(sb.work().join("f"), b"x").unwrap();
    assert!(sb.run(&["put", "f"]).status.success());
    // The data directory contains the trash it would move into.
    let out = sb.run(&["put", "-r", "../xdg"]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("it contains the trash directory"), "{err}");
    assert!(!err.contains("Invalid argument"), "{err}");
    let out = sb.run(&["put", "-rf", "../xdg/Trash"]);
    assert_eq!(out.status.code(), Some(1));
    // A trashed item stays where it is, with its info.
    let out = sb.run(&["put", "../xdg/Trash/files/f"]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("inside the trash directory"), "{err}");
    assert!(sb.trash().join("files/f").exists());
    assert!(sb.trash().join("info/f.trashinfo").exists());
    assert!(sb.root.join("xdg").is_dir());
}

#[test]
fn info_files_that_name_no_payload_are_never_items() {
    let sb = Sandbox::new("info-no-payload");
    fs::write(sb.work().join("keep"), b"x").unwrap();
    assert!(sb.run(&["put", "keep"]).status.success());
    // Joined to files/, these names point at files/ itself and at the
    // trash root; `rm evil` used to delete every item through them.
    for (n, info) in [(1, ".trashinfo"), (2, "..trashinfo"), (3, "...trashinfo")] {
        let record = format!(
            "[Trash Info]\nPath={}/evil{n}\nDeletionDate=2000-01-01T00:00:00\n",
            sb.work().display()
        );
        fs::write(sb.trash().join("info").join(info), record).unwrap();
    }
    let out = sb.run(&["list"]);
    let listed = stdout_of(&out);
    assert!(listed.contains("/keep"), "{listed}");
    assert!(!listed.contains("evil"), "{listed}");
    for n in 1..=3 {
        let evil = format!("evil{n}");
        // No match: rm, like trash-rm, still exits 0.
        let out = sb.run(&["rm", "--home-only", &evil]);
        assert!(!stdout_of(&out).contains("removed"), "{}", stdout_of(&out));
        assert!(!sb.run(&["restore", "--home-only", &evil]).status.success());
        assert!(sb.trash().join("files/keep").exists(), "{evil}");
    }
    // An age cut that the hostile records pass removes nothing through them.
    assert!(sb.run(&["empty", "--home-only", "1"]).status.success());
    assert!(sb.trash().join("files/keep").exists());
    assert!(sb.trash().join("info/keep.trashinfo").exists());
    assert!(!sb.work().join("evil3").exists());
}

/// Let the child behind `cmd` map at most 2 GiB where the platform allows
/// it: past that, an allocation fails and the child aborts. Linux enforces
/// `RLIMIT_AS`, and a refusal there is still an error. Other systems apply
/// the same limit only when `setrlimit` succeeds; macOS refuses `RLIMIT_AS`
/// with `EINVAL`, and failing the spawn on that would hide the test.
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
            let limited = libc::setrlimit(libc::RLIMIT_AS, &cap) == 0;
            #[cfg(target_os = "linux")]
            if !limited {
                return Err(std::io::Error::last_os_error());
            }
            #[cfg(not(target_os = "linux"))]
            let _ = limited;
            Ok(())
        });
    }
}

/// The largest resident size any child of this test process has reached,
/// in MiB. No command of the suite comes near 512 MiB, so a test can bound
/// it to catch a read that grows without end under [`cap_memory`].
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn peak_child_rss_mib() -> i64 {
    // SAFETY: getrusage only fills the zeroed struct handed to it.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) },
        0
    );
    let rss = usage.ru_maxrss as i64;
    // Linux counts ru_maxrss in KiB. macOS counts it in bytes.
    #[cfg(target_os = "linux")]
    {
        rss / 1024
    }
    #[cfg(target_os = "macos")]
    {
        rss / (1024 * 1024)
    }
}

#[test]
fn records_and_a_sizes_cache_that_are_no_regular_files_hang_nothing() {
    let sb = Sandbox::new("fifo-records");
    fs::write(sb.work().join("keep"), b"x").unwrap();
    fs::create_dir(sb.work().join("d")).unwrap();
    fs::write(sb.work().join("d/f"), b"x").unwrap();
    assert!(sb.run(&["put", "-r", "keep", "d"]).status.success());
    let t = sb.trash();
    let mkfifo = |p: PathBuf| {
        let _ = fs::remove_file(&p);
        assert!(Command::new("mkfifo").arg(&p).status().unwrap().success());
    };
    // Nothing writes to these FIFOs, so reading one waits forever.
    mkfifo(t.join("info/fifo.trashinfo"));
    fs::write(t.join("files/fifo"), b"x").unwrap();
    mkfifo(t.join("directorysizes"));
    let out = sb.run_within(20, &["list"]);
    let listed = stdout_of(&out);
    assert!(
        listed.contains("/keep") && listed.contains("/d"),
        "{listed}"
    );
    // --home-only keeps rm, restore and empty off the host's volume trashes.
    for args in [
        &["list", "--size"][..],
        &["status"],
        &["rm", "--home-only", "nothing-matches"],
        &["restore", "--home-only", "nothing-matches"],
        &["empty", "--home-only", "1"],
    ] {
        sb.run_within(20, args);
    }
    assert!(t.join("files/keep").exists() && t.join("files/d/f").exists());
    // A link to /dev/zero never ends, so reading the whole record or cache
    // would take all the memory. The commands must finish, and where the
    // platform can measure it the children stay small.
    for p in [t.join("info/fifo.trashinfo"), t.join("directorysizes")] {
        let _ = fs::remove_file(&p);
        std::os::unix::fs::symlink("/dev/zero", &p).unwrap();
    }
    for args in [&["list"][..], &["list", "--size"], &["status"]] {
        let out = sb.run_within(20, args);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{args:?}: {err}");
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    assert!(peak_child_rss_mib() < 512, "{} MiB", peak_child_rss_mib());
    assert!(t.join("files/keep").exists() && t.join("files/d/f").exists());
}

/// Plant a forged item `name` in the sandbox trash whose record restores to
/// `to`, with `body` as its payload.
fn forge_record(sb: &Sandbox, name: &str, to: &Path, body: &[u8]) {
    fs::create_dir_all(sb.trash().join("files")).unwrap();
    fs::create_dir_all(sb.trash().join("info")).unwrap();
    fs::write(sb.trash().join("files").join(name), body).unwrap();
    let record = format!(
        "[Trash Info]\nPath={}\nDeletionDate=2026-01-01T00:00:00\n",
        to.display()
    );
    let info = sb.trash().join("info").join(format!("{name}.trashinfo"));
    fs::write(info, record).unwrap();
}

#[test]
fn restore_takes_age_filters() {
    let sb = Sandbox::new("restore-age");
    fs::write(sb.work().join("fresh.txt"), b"fresh").unwrap();
    assert!(sb.run(&["put", "fresh.txt"]).status.success());
    // An item trashed on 2026-01-01, months before now.
    forge_record(&sb, "old.txt", &sb.work().join("old.txt"), b"old");

    // The space form reads as the = form, as list takes it.
    let out = sb.run(&["restore", "--older-than", "30", "--all"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(sb.work().join("old.txt")).unwrap(), b"old");
    assert!(!sb.work().join("fresh.txt").exists());
    // Nothing older is left; the fresh item stays put.
    let out = sb.run(&["restore", "--older-than=30", "--all"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    let out = sb.run(&["restore", "--newer-than", "1h", "--all"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(sb.work().join("fresh.txt")).unwrap(), b"fresh");
    let out = sb.run(&["restore", "--older-than=soon"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr_of(&out));
    let out = sb.run(&["restore", "--older-than"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr_of(&out));
}

#[test]
fn restore_takes_size_filters() {
    let sb = Sandbox::new("restore-size");
    fs::write(sb.work().join("big.bin"), vec![7u8; 1 << 20]).unwrap();
    fs::write(sb.work().join("small.txt"), b"s").unwrap();
    assert!(sb.run(&["put", "big.bin", "small.txt"]).status.success());

    let out = sb.run(&["restore", "--larger-than", "100k", "--all"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(sb.work().join("big.bin").exists());
    assert!(!sb.work().join("small.txt").exists());
    let out = sb.run(&["restore", "--larger-than=100k", "--all"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    for bad in ["--smaller-than=lots", "--larger-than=2K"] {
        let out = sb.run(&["restore", bad, "--smaller-than=1K", "--all"]);
        assert_eq!(out.status.code(), Some(2), "{bad}: {}", stderr_of(&out));
    }
    let out = sb.run(&["restore", "--smaller-than=100k", "--all"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(sb.work().join("small.txt")).unwrap(), b"s");
}

#[test]
fn list_depth_shows_a_trashed_tree() {
    let sb = Sandbox::new("list-depth");
    let dir = sb.work().join("proj");
    fs::create_dir_all(dir.join("sub/deeper")).unwrap();
    fs::write(dir.join("a.txt"), b"a").unwrap();
    fs::write(dir.join("sub/c.txt"), b"c").unwrap();
    fs::write(dir.join("sub/deeper/x.txt"), b"x").unwrap();
    assert!(sb.run(&["put", "-r", "proj"]).status.success());

    let out = sb.run(&["list", "--depth=2"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    let lines: Vec<&str> = text.lines().collect();
    let row = lines.iter().position(|l| l.ends_with("proj"));
    let row = row.unwrap_or_else(|| panic!("{text}"));
    assert_eq!(
        lines[row + 1..],
        ["    a.txt", "    sub/", "      c.txt", "      deeper/"],
        "{text}"
    );
    // Without --depth only the row shows.
    let plain = stdout_of(&sb.run(&["list"]));
    assert_eq!(plain.lines().count(), 1, "{plain}");
    for bad in [&["list", "--depth=x"][..], &["list", "--depth=1", "--json"]] {
        let out = sb.run(bad);
        assert_eq!(out.status.code(), Some(2), "{}", stderr_of(&out));
    }
}

#[test]
fn put_inspect_shows_the_size_and_asks() {
    let sb = Sandbox::new("put-inspect");
    let dir = sb.work().join("proj");
    fs::create_dir_all(dir.join("sub")).unwrap();
    for f in ["a.txt", "b.txt", "sub/c.txt"] {
        fs::write(dir.join(f), b"data").unwrap();
    }
    // A "no" keeps it, and the prompt names the file count.
    let out = sb.run_with_stdin(&["put", "-r", "--inspect", "proj"], "n\n");
    let err = stderr_of(&out);
    assert!(err.contains("remove directory 'proj' ("), "{err}");
    assert!(err.contains(", 3 files)? "), "{err}");
    assert!(dir.exists());
    // A "yes" trashes it; a plain file shows only its size.
    let out = sb.run_with_stdin(&["put", "-r", "--inspect", "proj"], "y\n");
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!dir.exists());
    fs::write(sb.work().join("one.txt"), b"x").unwrap();
    let out = sb.run_with_stdin(&["put", "--inspect", "one.txt"], "n\n");
    let err = stderr_of(&out);
    assert!(
        err.contains("'one.txt' (") && !err.contains("files)"),
        "{err}"
    );
    assert!(sb.work().join("one.txt").exists());
    // A later -f turns the prompt off, as it does for -i.
    let out = sb.run_with_stdin(&["put", "--inspect", "-f", "one.txt"], "");
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!sb.work().join("one.txt").exists());
}

#[test]
fn restore_sorts_the_numbered_table_by_size() {
    let sb = Sandbox::new("restore-sort-size");
    // The small file goes first, so date order would list it first.
    fs::write(sb.work().join("small.txt"), b"s").unwrap();
    assert!(sb.run(&["put", "small.txt"]).status.success());
    fs::write(sb.work().join("big.bin"), vec![7u8; 1 << 20]).unwrap();
    assert!(sb.run(&["put", "big.bin"]).status.success());

    let out = sb.run_with_stdin(&["restore", "--plain", "--sort=size"], "0\n");
    assert!(out.status.success(), "{}", stderr_of(&out));
    let first = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    assert!(first.ends_with("big.bin"), "{first}");
    assert!(sb.work().join("big.bin").exists());
    assert!(!sb.work().join("small.txt").exists());
    let out = sb.run(&["restore", "--sort=bulk"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr_of(&out));
}

#[test]
fn restore_never_writes_to_the_trash_or_what_contains_it() {
    let sb = Sandbox::new("restore-into-trash");
    fs::write(sb.work().join("keep"), b"real").unwrap();
    assert!(sb.run(&["put", "keep"]).status.success());
    let data = sb.root.join("xdg");
    let real = sb.trash().join("files/keep");
    let deep = sb.trash().join("files/new");
    forge_record(&sb, "onto-item", &real, b"forged");
    forge_record(&sb, "onto-data", &data, b"forged");
    forge_record(&sb, "into-files", &deep, b"forged");
    // A rename beside the data directory harms nothing, so only an
    // overwrite of it is refused; inside the trash, both are.
    for (to, why, conflicts) in [
        (
            &real,
            "inside the trash directory",
            &["overwrite", "rename"][..],
        ),
        (&data, "contains the trash directory", &["overwrite"]),
        (
            &deep,
            "inside the trash directory",
            &["overwrite", "rename"],
        ),
    ] {
        let to = to.to_str().unwrap();
        for conflict in conflicts {
            let conflict = format!("--conflict={conflict}");
            let out = sb.run(&["restore", &conflict, to]);
            assert_eq!(out.status.code(), Some(1), "{to} {conflict}");
            let err = stderr_of(&out);
            assert!(err.contains(why), "{to} {conflict}: {err}");
        }
    }
    // --to into the trash is refused as well.
    let files = sb.trash().join("files");
    let to = format!("--to={}", files.display());
    let out = sb.run(&["restore", "--home-only", &to, "--name=onto-item"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    // A record in another trash may not reach into the home trash either.
    let other = sb.root.join("other");
    fs::create_dir_all(other.join("files")).unwrap();
    fs::create_dir_all(other.join("info")).unwrap();
    fs::write(other.join("files/far"), b"forged").unwrap();
    let record = format!(
        "[Trash Info]\nPath={}\nDeletionDate=2026-01-01T00:00:00\n",
        real.display()
    );
    fs::write(other.join("info/far.trashinfo"), record).unwrap();
    let pin = format!("--trash-dir={}", other.display());
    let out = sb.run(&["restore", &pin, "-f", real.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains("inside the trash directory"));
    assert!(other.join("files/far").exists());
    assert_eq!(fs::read(&real).unwrap(), b"real");
    assert!(sb.trash().join("info/keep.trashinfo").exists());
    assert!(!deep.exists());
    for n in ["onto-item", "onto-data", "into-files"] {
        assert!(sb.trash().join("files").join(n).exists(), "{n}");
    }
}

#[test]
fn restore_refuses_a_record_with_no_absolute_location() {
    let sb = Sandbox::new("restore-relative");
    fs::write(sb.work().join("victim"), b"mine").unwrap();
    forge_record(&sb, "rel", Path::new("victim"), b"forged");
    forge_record(&sb, "blank", Path::new(""), b"forged");
    // restore runs in work/, where a relative Path= would land.
    for name in ["--name=rel", "--name=blank"] {
        // --home-only: a --name match would also reach the host's trashes.
        let out = sb.run(&["restore", "--home-only", "-f", name]);
        assert_eq!(out.status.code(), Some(1), "{name}");
        let err = stderr_of(&out);
        assert!(err.contains("no absolute location"), "{name}: {err}");
    }
    assert_eq!(fs::read(sb.work().join("victim")).unwrap(), b"mine");
    // --to still gets the item out, a relative DIR counting from here.
    fs::create_dir(sb.work().join("out")).unwrap();
    let out = sb.run(&["restore", "--home-only", "--to=out", "--name=rel"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(sb.work().join("out/victim")).unwrap(), b"forged");
    assert!(!sb.trash().join("info/rel.trashinfo").exists());
}

#[test]
fn a_restore_that_cannot_place_the_item_leaves_it_in_the_trash() {
    let sb = Sandbox::new("restore-unplaceable");
    // `%00` decodes to a zero byte, which no file name can hold.
    let record = format!(
        "[Trash Info]\nPath={}/a%00b\nDeletionDate=2026-01-01T00:00:00\n",
        sb.work().display()
    );
    fs::create_dir_all(sb.trash().join("files")).unwrap();
    fs::create_dir_all(sb.trash().join("info")).unwrap();
    fs::write(sb.trash().join("files/nul"), b"payload").unwrap();
    fs::write(sb.trash().join("info/nul.trashinfo"), record).unwrap();
    let out = sb.run(&["restore", "--home-only", "--name=nul"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr_of(&out);
    assert!(err.contains("the item stays in the trash"), "{err}");
    assert_eq!(fs::read(sb.trash().join("files/nul")).unwrap(), b"payload");
    let left: Vec<_> = fs::read_dir(sb.work()).unwrap().flatten().collect();
    assert!(left.is_empty(), "{left:?}");
}

/// restore errors quote the record's Path= verbatim; on a terminal its
/// escape sequences must show as `?`, as list shows them.
#[cfg(target_os = "linux")]
#[test]
fn restore_errors_scrub_control_characters_on_a_terminal() {
    if !Path::new("/usr/bin/script").exists() {
        eprintln!("skipping: no script(1) to give rtrash a terminal");
        return;
    }
    let sb = Sandbox::new("restore-scrub");
    // The destination exists, so restore refuses and quotes it.
    let dest = sb.work().join("a\x1b[2Jb");
    fs::create_dir(&dest).unwrap();
    let record = format!(
        "[Trash Info]\nPath={}/a%1B%5B2Jb\nDeletionDate=2024-01-02T03:04:05\n",
        sb.work().display()
    );
    fs::create_dir_all(sb.trash().join("files")).unwrap();
    fs::create_dir_all(sb.trash().join("info")).unwrap();
    fs::write(sb.trash().join("files/evil"), b"x").unwrap();
    fs::write(sb.trash().join("info/evil.trashinfo"), record).unwrap();
    let piped = sb.run(&["restore", "--home-only", "--name=evil"]);
    assert!(
        stderr_of(&piped).contains("a\x1b[2Jb"),
        "{}",
        stderr_of(&piped)
    );
    let cmd = format!("{} restore --home-only --name=evil", bin());
    let tty = Command::new("/usr/bin/script")
        .args(["-qec", &cmd, "/dev/null"])
        .current_dir(sb.work())
        .env("XDG_DATA_HOME", sb.root.join("xdg"))
        .env("XDG_CONFIG_HOME", sb.root.join(".config"))
        .env("HOME", &sb.root)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run script");
    let shown = stdout_of(&tty);
    assert!(shown.contains("refusing to overwrite"), "{shown:?}");
    assert!(shown.contains("a?[2Jb"), "{shown:?}");
    assert!(!shown.contains('\x1b'), "{shown:?}");
}

#[test]
fn age_cuts_keep_items_whose_age_is_unknown() {
    let sb = Sandbox::new("age-unknown");
    fs::create_dir_all(sb.trash().join("files")).unwrap();
    fs::create_dir_all(sb.trash().join("info")).unwrap();
    let w = sb.work();
    for (name, date) in [
        ("old", "DeletionDate=2000-01-01T00:00:00\n"),
        ("undated", ""),
        ("garbled", "DeletionDate=yesterday\n"),
    ] {
        fs::write(sb.trash().join("files").join(name), b"x").unwrap();
        let record = format!("[Trash Info]\nPath={}/{name}\n{date}", w.display());
        fs::write(sb.trash().join(format!("info/{name}.trashinfo")), record).unwrap();
    }
    let pattern = format!("{}/*", w.display());
    // --home-only keeps rm and empty off the host's volume trashes.
    let out = sb.run(&["rm", "--home-only", "--older-than=1", &pattern]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!sb.trash().join("files/old").exists());
    fs::write(sb.trash().join("files/old"), b"x").unwrap();
    let record = format!(
        "[Trash Info]\nPath={}/old\nDeletionDate=2000-01-01T00:00:00\n",
        w.display()
    );
    fs::write(sb.trash().join("info/old.trashinfo"), record).unwrap();
    assert!(sb.run(&["empty", "--home-only", "1"]).status.success());
    assert!(!sb.trash().join("files/old").exists());
    for name in ["undated", "garbled"] {
        assert!(sb.trash().join("files").join(name).exists(), "{name}");
        assert!(
            sb.trash().join(format!("info/{name}.trashinfo")).exists(),
            "{name}"
        );
    }
    // A full empty still clears them.
    assert!(sb.run(&["empty", "--home-only", "--all"]).status.success());
    assert!(!sb.trash().join("files/undated").exists());
}

#[test]
fn put_json_reports_trashed_and_failed() {
    let sb = Sandbox::new("put-json");
    sb.touch("a.txt");
    fs::create_dir(sb.work().join("d")).unwrap();
    fs::write(sb.work().join("d/x"), b"x").unwrap();
    let pin = format!("--trash-dir={}", sb.trash().display());
    let out = sb.run(&["put", &pin, "--json", "-rv", "a.txt", "d", "missing.txt"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    let stdout = stdout_of(&out);
    // -v lines must not mix into the JSON on stdout.
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    let line = stdout.trim();
    assert!(line.starts_with("{\"trashed\":["), "{line}");
    assert!(line.ends_with(",\"failed\":1}"), "{line}");
    let a = sb.work().join("a.txt");
    assert!(
        line.contains(&format!("\"original\":\"{}\"", a.display())),
        "{line}"
    );
    assert!(line.contains("\"name\":\"d\""), "{line}");
    assert!(
        line.contains(&format!("\"trash_dir\":\"{}\"", sb.trash().display())),
        "{line}"
    );
    assert!(stderr_of(&out).contains("missing.txt"));
}

/// Run rtrash in `cwd` with the trash under `data_home`.
fn run_in(cwd: &Path, data_home: &Path, args: &[&str]) -> Output {
    common::rtrash(data_home)
        .args(args)
        .current_dir(cwd)
        .env("XDG_DATA_HOME", data_home)
        .output()
        .expect("failed to run rtrash")
}

/// The JSON string value of `key` in `json`.
fn json_str(json: &str, key: &str) -> String {
    let at = json
        .find(&format!("\"{key}\":\""))
        .unwrap_or_else(|| panic!("no {key} in {json}"))
        + key.len()
        + 4;
    json[at..at + json[at..].find('"').unwrap()].to_string()
}

/// A sandbox reached through a symlink, the way macOS reaches its temp
/// dir through /var -> /private/var. Originals and trash dirs both come
/// out resolved, and a path typed through the link still restores.
#[test]
fn symlinked_parents_resolve_alike_for_originals_and_trash() {
    let sb = Sandbox::new("symlinked");
    let link = sb.root.with_extension("link");
    let _ = fs::remove_file(&link);
    std::os::unix::fs::symlink(&sb.root, &link).unwrap();
    let lwork = link.join("work");
    let ldata = link.join("xdg");
    fs::write(lwork.join("a.txt"), b"a").unwrap();

    let out = run_in(&lwork, &ldata, &["put", "--json", "a.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let json = stdout_of(&out);
    let original = sb.work().join("a.txt");
    assert_eq!(json_str(&json, "original"), original.display().to_string());
    assert_eq!(
        json_str(&json, "trash_dir"),
        sb.trash().display().to_string()
    );
    let info = fs::read_to_string(sb.trash().join("info/a.txt.trashinfo")).unwrap();
    assert!(
        info.contains(&format!("Path={}\n", original.display())),
        "{info}"
    );

    // Restore by the path as typed through the link.
    let typed = lwork.join("a.txt");
    let out = run_in(&lwork, &ldata, &["restore", typed.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(original.exists());

    // A pinned volume trash reached through the link keeps Path= relative
    // to its topdir, and restore finds the file again.
    let uid = {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(&sb.root).unwrap().uid()
    };
    fs::create_dir(sb.root.join("vol")).unwrap();
    fs::write(sb.root.join("vol/f.txt"), b"f").unwrap();
    let pin = link.join(format!("vol/.Trash-{uid}"));
    let pin_arg = format!("--trash-dir={}", pin.display());
    let lvol = link.join("vol");
    let out = run_in(&lvol, &ldata, &["put", &pin_arg, "--json", "f.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let real_pin = sb.root.join(format!("vol/.Trash-{uid}"));
    assert_eq!(
        json_str(&stdout_of(&out), "trash_dir"),
        real_pin.display().to_string()
    );
    let info = fs::read_to_string(real_pin.join("info/f.txt.trashinfo")).unwrap();
    assert!(info.contains("Path=f.txt\n"), "{info}");
    let out = run_in(&lvol, &ldata, &["restore", &pin_arg, "f.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(sb.root.join("vol/f.txt").exists());
    let _ = fs::remove_file(&link);
}

/// `rm` takes an absolute pattern typed through a symlinked directory,
/// as restore takes such a path.
#[test]
fn rm_patterns_through_a_symlinked_parent_match() {
    let sb = Sandbox::new("rm-symlinked");
    let link = sb.root.with_extension("link");
    let _ = fs::remove_file(&link);
    std::os::unix::fs::symlink(&sb.root, &link).unwrap();
    let lwork = link.join("work");
    let ldata = link.join("xdg");
    for f in ["a.txt", "b.o", "c.o"] {
        fs::write(lwork.join(f), b"x").unwrap();
    }
    let out = run_in(&lwork, &ldata, &["put", "a.txt", "b.o", "c.o"]);
    assert!(out.status.success(), "{}", stderr_of(&out));

    let typed = lwork.join("a.txt");
    let out = run_in(&lwork, &ldata, &["rm", typed.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let glob = format!("{}/*.o", lwork.display());
    let out = run_in(&lwork, &ldata, &["rm", "-n", &glob]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(stdout_of(&out).lines().count(), 2, "{}", stdout_of(&out));
    let left: Vec<_> = fs::read_dir(sb.trash().join("info"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(!left.iter().any(|n| n.starts_with("a.txt")), "{left:?}");
    assert_eq!(left.len(), 2, "{left:?}");
    let _ = fs::remove_file(&link);
}

#[test]
fn put_json_force_missing_is_empty_summary() {
    let sb = Sandbox::new("put-json-force");
    let pin = format!("--trash-dir={}", sb.trash().display());
    let out = sb.run(&["put", &pin, "--json", "-f", "nope.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(stdout_of(&out).trim(), r#"{"trashed":[],"failed":0}"#);
}

#[test]
fn put_directory_requires_recursive() {
    let sb = Sandbox::new("put-dir");
    fs::create_dir(sb.work().join("d")).unwrap();
    fs::write(sb.work().join("d/x"), b"x").unwrap();

    let out = sb.run(&["put", "d"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).contains("Is a directory"),
        "{}",
        stderr_of(&out)
    );
    assert!(sb.work().join("d").exists());

    let out = sb.run(&["put", "-r", "d"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!sb.work().join("d").exists());
    assert!(sb.trash().join("files/d/x").exists());
}

#[test]
fn put_d_flag_only_empty_dirs() {
    let sb = Sandbox::new("put-dflag");
    fs::create_dir(sb.work().join("full")).unwrap();
    fs::write(sb.work().join("full/x"), b"x").unwrap();
    fs::create_dir(sb.work().join("hollow")).unwrap();

    let out = sb.run(&["put", "-d", "full"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr_of(&out).contains("Directory not empty"));

    let out = sb.run(&["put", "-d", "hollow"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!sb.work().join("hollow").exists());
}

#[test]
fn force_ignores_missing_and_bare_force_exits_zero() {
    let sb = Sandbox::new("put-force");
    let out = sb.run(&["put", "nope"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr_of(&out).contains("No such file or directory"));

    let out = sb.run(&["put", "-f", "nope"]);
    assert!(out.status.success());

    let out = sb.run(&["put", "-f"]);
    assert!(out.status.success());

    let out = sb.run(&["put"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn combined_short_flags_and_rm_fallthrough() {
    let sb = Sandbox::new("put-rf");
    fs::create_dir(sb.work().join("d")).unwrap();
    // No subcommand: `rtrash -rf d` behaves like rm -rf.
    let out = sb.run(&["-rf", "d"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!sb.work().join("d").exists());
}

#[test]
fn refuses_dot_and_root() {
    let sb = Sandbox::new("put-dots");
    let out = sb.run(&["put", "-rf", "."]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr_of(&out).contains("refusing"));

    let out = sb.run(&["put", "-rf", "/"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr_of(&out).contains("dangerous"));
}

#[cfg(unix)]
#[test]
fn put_skips_safe_rm_paths_and_refuses_home() {
    let sb = Sandbox::new("put-safety");
    fs::create_dir(sb.work().join("precious")).unwrap();
    sb.touch("precious/f");
    fs::create_dir(sb.work().join("other")).unwrap();
    // safe-rm's per-user list lives in ~/.safe-rm; patterns work too.
    fs::write(
        sb.root.join(".safe-rm"),
        format!("# keep\n{}/prec*\n", sb.work().display()),
    )
    .unwrap();
    let out = sb.run(&["put", "-r", "precious", "other"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr_of(&out);
    assert!(err.contains("skipping protected path 'precious'"), "{err}");
    assert!(sb.work().join("precious/f").exists());
    assert!(!sb.work().join("other").exists(), "unlisted paths still go");

    let out = sb.run(&["put", "-rf", "/usr"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr_of(&out).contains("safe-rm list entry '/usr'"));

    let home = sb.root.display().to_string();
    let out = sb.run(&["put", "-r", "--preserve-root=all", &home]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).contains("home directory"),
        "{}",
        stderr_of(&out)
    );
    assert!(sb.root.join("work").exists());
}

#[cfg(unix)]
#[test]
fn put_warns_before_trashing_a_repo_with_unsaved_work() {
    let sb = Sandbox::new("put-repo-warn");
    let repo = sb.work().join("repo");
    fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", &sb.root)
            .output()
    };
    if !git(&["init", "-q"]).is_ok_and(|o| o.status.success()) {
        eprintln!("git unavailable; skipping");
        return;
    }
    fs::write(repo.join("f"), b"x").unwrap();
    fs::create_dir(sb.work().join("plain")).unwrap();
    let out = sb.run(&["put", "-r", "repo", "plain"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(
        err.contains("'repo' contains a git repository with"),
        "{err}"
    );
    assert!(err.contains("rtrash undo"), "{err}");
    assert!(!err.contains("'plain'"), "{err}");
    assert!(!repo.exists(), "a warning, not a refusal");
}

#[test]
fn collision_gets_suffixed_name() {
    let sb = Sandbox::new("put-collide");
    sb.touch("same.txt");
    assert!(sb.run(&["put", "same.txt"]).status.success());
    sb.touch("same.txt");
    assert!(sb.run(&["put", "same.txt"]).status.success());
    let mut names = trash_names(&sb);
    names.sort();
    assert_eq!(names, vec!["same.txt", "same.txt.2"]);
    assert!(sb.trash().join("info/same.txt.2.trashinfo").exists());
}

#[test]
fn symlink_trashes_link_not_target() {
    let sb = Sandbox::new("put-symlink");
    let target = sb.touch("target.txt");
    std::os::unix::fs::symlink(&target, sb.work().join("link")).unwrap();
    let out = sb.run(&["put", "link"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(target.exists(), "target must survive");
    assert!(sb
        .trash()
        .join("files/link")
        .symlink_metadata()
        .unwrap()
        .is_symlink());
}

#[test]
fn list_shows_original_path() {
    let sb = Sandbox::new("list");
    let f = sb.touch("seen.txt");
    sb.run(&["put", "seen.txt"]);
    let out = sb.run(&["list"]);
    assert!(out.status.success());
    assert!(
        stdout_of(&out).contains(&f.display().to_string()),
        "{}",
        stdout_of(&out)
    );
}

#[test]
fn empty_removes_everything() {
    let sb = Sandbox::new("empty-all");
    sb.touch("a");
    sb.touch("b");
    fs::create_dir(sb.work().join("d")).unwrap();
    fs::write(sb.work().join("d/x"), b"x").unwrap();
    sb.run(&["put", "-r", "a", "b", "d"]);
    assert_eq!(trash_names(&sb).len(), 3);

    let out = sb.empty(&[]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(trash_names(&sb).is_empty());
    assert!(fs::read_dir(sb.trash().join("info"))
        .unwrap()
        .next()
        .is_none());
}

/// A makepkg build tree: `pkg` and its inner directory are mode 0311,
/// so the owner can enter but not list them.
fn locked_build_tree(at: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(at.join("pkg/linux/usr/lib")).unwrap();
    fs::write(at.join("pkg/linux/usr/lib/vmlinuz"), b"k").unwrap();
    fs::write(at.join("PKGBUILD"), b"p").unwrap();
    for d in ["pkg/linux/usr/lib", "pkg/linux", "pkg"] {
        fs::set_permissions(at.join(d), fs::Permissions::from_mode(0o311)).unwrap();
    }
}

#[test]
fn empty_opens_up_owned_directories_it_cannot_list() {
    let sb = Sandbox::new("empty-locked");
    // One trashed through put, one orphan with no .trashinfo.
    fs::create_dir(sb.work().join("build")).unwrap();
    locked_build_tree(&sb.work().join("build"));
    let out = sb.run(&["put", "-r", "build"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    locked_build_tree(&sb.trash().join("files/linux-6.9"));

    let out = sb.empty(&[]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(trash_names(&sb).is_empty(), "{:?}", trash_names(&sb));

    // An age-filtered empty removes item by item; same repair.
    fs::create_dir(sb.work().join("build")).unwrap();
    locked_build_tree(&sb.work().join("build"));
    sb.run(&["put", "-r", "build"]);
    let out = sb.empty(&["0"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(trash_names(&sb).is_empty(), "{:?}", trash_names(&sb));
}

#[test]
fn orphans_show_in_list_and_status_and_go_with_rm() {
    let sb = Sandbox::new("orphans");
    sb.touch("kept");
    sb.run(&["put", "kept"]);
    let orphan = sb.trash().join("files/linux-6.9");
    locked_build_tree(&orphan);

    let out = sb.run(&["list", "--home-only"]);
    let text = stdout_of(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(lines[0].ends_with("/work/kept"), "{text}");
    assert_eq!(
        lines[1],
        format!("????-??-?? ??:??:?? {}", orphan.display())
    );
    let json = stdout_of(&sb.run(&["list", "--home-only", "--json"]));
    assert!(json.contains("\"name\":\"linux-6.9\""), "{json}");
    assert_eq!(json.matches("\"orphan\":true").count(), 1, "{json}");
    // No date, so an age filter leaves the orphan out.
    let old = stdout_of(&sb.run(&["list", "--home-only", "--older-than=0"]));
    assert!(!old.contains("linux-6.9"), "{old}");

    let st = stdout_of(&sb.run(&["status", "--home-only"]));
    assert!(st.contains("2 items (1 without .trashinfo)"), "{st}");
    let st = stdout_of(&sb.run(&["status", "--home-only", "--json"]));
    assert!(st.contains("\"items\":2,"), "{st}");
    assert!(st.contains("\"orphans\":1"), "{st}");

    let out = sb.run(&["rm", "--home-only", "linux-6.9"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(trash_names(&sb).len(), 1);
    assert!(!orphan.exists());
}

#[test]
fn empty_days_keeps_recent_items() {
    let sb = Sandbox::new("empty-days");
    sb.touch("fresh.txt");
    sb.run(&["put", "fresh.txt"]);

    let out = sb.empty(&["5"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(
        trash_names(&sb),
        vec!["fresh.txt"],
        "recent item must survive"
    );

    // Backdate the info file, then a 5-day empty purges it.
    let info_path = sb.trash().join("info/fresh.txt.trashinfo");
    let body = fs::read_to_string(&info_path).unwrap();
    let old = body
        .lines()
        .map(|l| {
            if l.starts_with("DeletionDate=") {
                "DeletionDate=2001-01-01T00:00:00".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&info_path, old).unwrap();

    let out = sb.empty(&["5"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(trash_names(&sb).is_empty());
}

#[test]
fn empty_dry_run_removes_nothing() {
    let sb = Sandbox::new("empty-dry");
    sb.touch("keep.txt");
    sb.run(&["put", "keep.txt"]);
    let out = sb.empty(&["--dry-run"]);
    assert!(out.status.success());
    assert_eq!(trash_names(&sb), vec!["keep.txt"]);
    let err = stderr_of(&out);
    assert!(err.contains("Would remove 1 item"), "{err}");
    assert!(
        err.contains("reclaimable"),
        "dry-run must report reclaimable space: {err}"
    );
}

#[test]
fn directorysizes_atomic_rewrite_survives_put() {
    let sb = Sandbox::new("dirsizes-atomic");
    fs::create_dir(sb.work().join("d")).unwrap();
    fs::write(sb.work().join("d/x"), b"payload-bytes").unwrap();
    assert!(sb.run(&["put", "-r", "d"]).status.success());
    let cache = sb.trash().join("directorysizes");
    assert!(cache.is_file());
    let body = fs::read_to_string(&cache).unwrap();
    assert!(
        body.contains(" d\n") || body.ends_with(" d") || body.split_whitespace().any(|t| t == "d"),
        "{body}"
    );
    // No leftover temp files from atomic write.
    let leftovers: Vec<_> = fs::read_dir(sb.trash())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
        .collect();
    assert!(
        leftovers.is_empty(),
        "atomic write left temps: {leftovers:?}"
    );
}

#[test]
fn trash_rm_dry_run_keeps_entries_and_reports_reclaim() {
    let sb = Sandbox::new("rm-dry");
    sb.touch("drop.o");
    sb.touch("keep.c");
    assert!(sb.run(&["put", "drop.o", "keep.c"]).status.success());
    let out = sb.run(&["rm", "--dry-run", "*.o"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(err.contains("Would permanently remove"), "{err}");
    assert!(err.contains("reclaimable"), "{err}");
    let mut names = trash_names(&sb);
    names.sort();
    assert_eq!(names, vec!["drop.o", "keep.c"]);
}

#[test]
fn status_reports_items_and_size_after_put() {
    let sb = Sandbox::new("status");
    fs::write(sb.work().join("blob.bin"), vec![b'x'; 8192]).unwrap();
    assert!(sb.run(&["put", "blob.bin"]).status.success());
    let out = sb.run(&["status", "--home-only"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let s = stdout_of(&out);
    assert!(s.contains("Total:"), "{s}");
    assert!(
        s.contains("\t1 item\t") && s.contains("Total: 1 item ("),
        "{s}"
    );
    assert!(!s.contains("Total: 0 items"), "{s}");
}

#[test]
fn home_only_empty_ignores_foreign_trash_dir() {
    let sb = Sandbox::new("home-only");
    sb.touch("home.txt");
    assert!(sb.run(&["put", "home.txt"]).status.success());
    // Foreign volume-style trash with a separate entry (not under home discovery
    // when --home-only). We pin is not used; foreign is outside XDG home trash.
    let foreign = sb.root.join("mnt/.Trash-1");
    fs::create_dir_all(foreign.join("files")).unwrap();
    fs::create_dir_all(foreign.join("info")).unwrap();
    fs::write(foreign.join("files/vol.dat"), b"keep-on-volume").unwrap();
    fs::write(
        foreign.join("info/vol.dat.trashinfo"),
        "[Trash Info]\nPath=vol.dat\nDeletionDate=2020-01-01T00:00:00\n",
    )
    .unwrap();
    let out = sb.run(&["empty", "--home-only"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(trash_names(&sb).is_empty());
    assert!(
        foreign.join("files/vol.dat").is_file(),
        "volume trash must survive --home-only empty"
    );
}

#[test]
fn put_writes_complete_trashinfo_after_fsync_path() {
    let sb = Sandbox::new("put-fsync-info");
    let f = sb.touch("syncme.txt");
    assert!(sb.run(&["put", "syncme.txt"]).status.success());
    let info = fs::read_to_string(sb.trash().join("info/syncme.txt.trashinfo")).unwrap();
    assert!(info.starts_with("[Trash Info]\n"), "{info}");
    assert!(info.contains("Path="), "{info}");
    assert!(info.contains("DeletionDate="), "{info}");
    // Full body must be present (not truncated reservation).
    assert!(
        info.lines().count() >= 3,
        "expected full trashinfo lines, got {info:?}"
    );
    assert!(!f.exists());
}

#[test]
fn empty_without_pin_clears_home_trash() {
    // trash-cli parity: default empty visits home trash (no --trash-dir).
    let sb = Sandbox::new("empty-default-home");
    sb.touch("home-only.txt");
    assert!(sb.run(&["put", "home-only.txt"]).status.success());
    assert!(sb.trash().join("files/home-only.txt").exists());
    // A bare empty also empties every volume trash of the host. Run it
    // only when no item outside this sandbox is listed, so nothing of
    // the host's can go.
    let listed = stdout_of(&sb.run(&["list"]));
    let root = sb.root.display().to_string();
    if let Some(foreign) = listed.lines().find(|l| !l.contains(&root)) {
        eprintln!("skipping: a bare empty would remove the host's '{foreign}'");
        return;
    }
    // Bare empty (no pin) must clear this sandbox home trash via XDG_DATA_HOME.
    let out = sb.run(&["empty"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(trash_names(&sb).is_empty());
}

#[test]
fn restore_force_missing_payload_keeps_dest() {
    let sb = Sandbox::new("restore-missing-payload");
    sb.touch("live.txt");
    assert!(sb.run(&["put", "live.txt"]).status.success());
    // Stale info, missing payload: must not destroy a recreated dest with -f.
    fs::remove_file(sb.trash().join("files/live.txt")).unwrap();
    fs::write(sb.work().join("live.txt"), b"survivor").unwrap();
    let out = sb.run(&["restore", "-f", "live.txt"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert_eq!(fs::read(sb.work().join("live.txt")).unwrap(), b"survivor");
}

#[test]
fn trash_dir_pin_rejects_non_trash_layout() {
    let sb = Sandbox::new("bad-pin");
    let fake = sb.root.join("project");
    fs::create_dir_all(fake.join("files")).unwrap();
    fs::create_dir_all(fake.join("info")).unwrap();
    fs::write(fake.join("files/secret"), b"do-not-wipe").unwrap();
    // Valid-looking names but we still require the layout — this IS valid layout.
    // Make info a symlink so validation fails.
    fs::remove_dir(fake.join("info")).unwrap();
    std::os::unix::fs::symlink("/tmp", fake.join("info")).unwrap();
    let pin = format!("--trash-dir={}", fake.display());
    let out = sb.run(&["empty", &pin]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr_of(&out));
    assert_eq!(fs::read(fake.join("files/secret")).unwrap(), b"do-not-wipe");
}

/// put refuses a trash whose `files/` or `info/` is a symbolic link, as
/// trash-cli 0.26.9 does, and the item stays where it was: the home trash
/// and a pinned one alike. The folder the link points at gets nothing.
#[test]
fn put_refuses_a_trash_with_a_symlinked_files_or_info_dir() {
    let sb = Sandbox::new("put-linked-sub");
    let elsewhere = sb.root.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let pinned = sb.root.join("pinned");
    for (sub, pin) in [
        ("files", false),
        ("info", false),
        ("files", true),
        ("info", true),
    ] {
        let trash = if pin { pinned.clone() } else { sb.trash() };
        let _ = fs::remove_dir_all(&trash);
        fs::create_dir_all(&trash).unwrap();
        std::os::unix::fs::symlink(&elsewhere, trash.join(sub)).unwrap();
        sb.touch("a.txt");
        let pin_arg = format!("--trash-dir={}", trash.display());
        let mut args = vec!["put"];
        if pin {
            args.push(&pin_arg);
        }
        args.push("a.txt");
        let out = sb.run(&args);
        assert!(
            !out.status.success(),
            "{sub} pin={pin}: {}",
            stdout_of(&out)
        );
        assert!(
            stderr_of(&out).contains("is a symbolic link"),
            "{sub} pin={pin}: {}",
            stderr_of(&out)
        );
        assert!(sb.work().join("a.txt").is_file(), "{sub} pin={pin}");
        assert_eq!(
            fs::read_dir(&elsewhere).unwrap().count(),
            0,
            "{sub} pin={pin}"
        );
    }
}

#[test]
fn trash_rm_refuses_star_without_force() {
    let sb = Sandbox::new("rm-star");
    sb.touch("a.txt");
    sb.run(&["put", "a.txt"]);
    let out = sb.run(&["rm", "*"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr_of(&out));
    assert_eq!(trash_names(&sb), vec!["a.txt"]);
}

#[test]
fn trash_rm_regex_matches_the_original_path() {
    let sb = Sandbox::new("rm-regex-paths");
    for f in ["build-12.log", "build-x.log", "keep.txt"] {
        sb.touch(f);
        assert!(sb.run(&["put", f]).status.success());
    }
    let out = sb.run(&["rm", "--regex", r"/build-[0-9]+\.log$"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let mut left = trash_names(&sb);
    left.sort();
    assert_eq!(left, ["build-x.log", "keep.txt"]);
    // A regex that matches every path needs --force, a bad one exits 2.
    let mass = sb.run(&["rm", "--regex", ".*"]);
    assert_eq!(mass.status.code(), Some(2), "{}", stderr_of(&mass));
    let bad = sb.run(&["rm", "--regex", "("]);
    assert_eq!(bad.status.code(), Some(2), "{}", stderr_of(&bad));
    left = trash_names(&sb);
    left.sort();
    assert_eq!(left, ["build-x.log", "keep.txt"]);
}

#[test]
fn trash_rm_match_takes_substring_and_exact() {
    let sb = Sandbox::new("rm-match-kinds");
    for f in ["old-a.txt", "old-b.txt", "new.txt"] {
        sb.touch(f);
        assert!(sb.run(&["put", f]).status.success());
    }
    let out = sb.run(&["rm", "-m", "substring", "/old-a"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    // The original path exactly as put recorded it.
    let info = fs::read_to_string(sb.trash().join("info/old-b.txt.trashinfo")).unwrap();
    let exact = info
        .lines()
        .find_map(|l| l.strip_prefix("Path="))
        .unwrap()
        .to_string();
    let out = sb.run(&["rm", "--match=exact", &exact]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    // Exact needs the whole path: a bare name matches nothing.
    let out = sb.run(&["rm", "--match=exact", "new.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(trash_names(&sb), ["new.txt"]);
    // A substring every path holds needs --force; a bad kind exits 2.
    let mass = sb.run(&["rm", "-m", "substring", "/"]);
    assert_eq!(mass.status.code(), Some(2), "{}", stderr_of(&mass));
    let bad = sb.run(&["rm", "--match=fuzzy", "x"]);
    assert_eq!(bad.status.code(), Some(2), "{}", stderr_of(&bad));
    assert_eq!(trash_names(&sb), ["new.txt"]);
}

#[test]
fn empty_dry_run_reports_space_for_tree() {
    let sb = Sandbox::new("empty-dry-du");
    let d = sb.work().join("big");
    fs::create_dir(&d).unwrap();
    // ~64 KiB payload so the summary cannot be "0 B" on any normal FS.
    fs::write(d.join("blob"), vec![b'z'; 64 * 1024]).unwrap();
    assert!(sb.run(&["put", "-r", "big"]).status.success());
    let out = sb.empty(&["--dry-run"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(sb.trash().join("files/big/blob").is_file());
    let err = stderr_of(&out);
    assert!(err.contains("Would remove 1 item"), "{err}");
    assert!(err.contains("reclaimable"), "{err}");
    // Must not claim zero reclaim for a large file.
    assert!(
        !err.contains("(0 B,"),
        "expected non-zero reclaim estimate: {err}"
    );
}

#[test]
fn empty_purges_orphaned_files() {
    let sb = Sandbox::new("empty-orphan");
    sb.touch("x");
    sb.run(&["put", "x"]);
    fs::remove_file(sb.trash().join("info/x.trashinfo")).unwrap();
    let out = sb.empty(&[]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(trash_names(&sb).is_empty());
}

#[test]
fn restore_single_match_roundtrip() {
    let sb = Sandbox::new("restore");
    let f = sb.touch("back.txt");
    sb.run(&["put", "back.txt"]);
    assert!(!f.exists());

    let out = sb.run(&["restore", "back.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(f.exists());
    assert_eq!(fs::read(&f).unwrap(), b"payload");
    assert!(trash_names(&sb).is_empty());
    assert!(!sb.trash().join("info/back.txt.trashinfo").exists());
}

#[test]
fn restore_interactive_picks_by_index_from_full_trash() {
    let sb = Sandbox::new("restore-pick");
    let a = sb.touch("a.txt");
    let b = sb.touch("b.txt");
    assert!(sb.run(&["put", "a.txt"]).status.success());
    assert!(sb.run(&["put", "b.txt"]).status.success());
    assert!(!a.exists() && !b.exists());

    // Bare `restore` lists every trashed item; pick index 0 via piped stdin.
    // --home-only: the host's volume trashes would add rows, and index 0
    // could restore one of their items.
    let list = sb.run(&["list", "--home-only"]);
    assert!(list.status.success(), "{}", stderr_of(&list));
    let list_out = stdout_of(&list);
    let n_lines = list_out.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(n_lines, 2, "list: {list_out}");

    let out = sb.run_with_stdin(&["restore", "--home-only"], "0\n");
    assert!(
        out.status.success(),
        "stdout={} stderr={}",
        stdout_of(&out),
        stderr_of(&out)
    );
    assert!(stdout_of(&out).contains("restored"), "{}", stdout_of(&out));
    let restored = a.exists() as u8 + b.exists() as u8;
    assert_eq!(restored, 1, "exactly one file restored after pick 0");
    assert_eq!(trash_names(&sb).len(), 1);
}

/// Backdate one trashinfo so ordering tests do not depend on put timing.
fn backdate_trashinfo(sb: &Sandbox, name: &str, date: &str) {
    let info = sb.trash().join(format!("info/{name}.trashinfo"));
    let text = fs::read_to_string(&info).unwrap();
    let out: Vec<String> = text
        .lines()
        .map(|l| {
            if l.starts_with("DeletionDate=") {
                format!("DeletionDate={date}")
            } else {
                l.to_string()
            }
        })
        .collect();
    fs::write(&info, out.join("\n") + "\n").unwrap();
}

/// The local time `secs` seconds ago, as a `.trashinfo` date.
#[cfg(unix)]
fn local_date_ago(secs: u64) -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - secs;
    // GNU date takes -d @EPOCH, BSD date -r EPOCH.
    for args in [
        vec!["-d".to_string(), format!("@{t}")],
        vec!["-r".into(), t.to_string()],
    ] {
        if let Ok(o) = Command::new("date")
            .args(&args)
            .arg("+%Y-%m-%dT%H:%M:%S")
            .output()
        {
            if o.status.success() {
                return String::from_utf8(o.stdout).unwrap().trim().to_string();
            }
        }
    }
    panic!("date cannot format an epoch");
}

/// `rm` and `empty` take ages in hours and minutes as well as days.
#[cfg(unix)]
#[test]
fn rm_and_empty_take_ages_in_hours_and_minutes() {
    let sb = Sandbox::new("age-hours");
    sb.touch("three-hours.txt");
    sb.touch("fresh.txt");
    sb.touch("ninety-min.txt");
    let put = sb.run(&["put", "three-hours.txt", "fresh.txt", "ninety-min.txt"]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    backdate_trashinfo(&sb, "three-hours.txt", &local_date_ago(3 * 3600));
    backdate_trashinfo(&sb, "ninety-min.txt", &local_date_ago(90 * 60));
    let pin = format!("--trash-dir={}", sb.trash().display());
    let out = sb.run(&["empty", &pin, "--older-than=2h", "-n", "--json"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let j = stdout_of(&out);
    assert!(
        j.contains(r#""removed":1,"#) && j.contains(r#""older_than_secs":7200"#),
        "{j}"
    );
    let out = sb.run(&["empty", &pin, "--older-than=1d", "-n", "--json"]);
    assert!(
        stdout_of(&out).contains(r#""removed":0,"#),
        "{}",
        stdout_of(&out)
    );
    assert!(
        stdout_of(&out).contains(r#""days":1,"#),
        "{}",
        stdout_of(&out)
    );
    // rm: older than 1h but newer than 2h30m takes only the 90-minute item.
    let out = sb.run(&[
        "rm",
        &pin,
        "-f",
        "--older-than=1h",
        "--newer-than=2h30m",
        "*",
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!sb.trash().join("files/ninety-min.txt").exists());
    assert!(sb.trash().join("files/three-hours.txt").exists());
    assert!(sb.trash().join("files/fresh.txt").exists());
    let out = sb.run(&["empty", &pin, "--older-than=2h", "--plain"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!sb.trash().join("files/three-hours.txt").exists());
    assert!(sb.trash().join("files/fresh.txt").exists());
    for bad in ["--older-than=2x", "--older-than=1.5h"] {
        assert_eq!(
            sb.run(&["rm", &pin, bad, "*"]).status.code(),
            Some(2),
            "{bad}"
        );
        assert_eq!(
            sb.run(&["empty", &pin, bad]).status.code(),
            Some(2),
            "{bad}"
        );
    }
}

/// `list` and `status` take the same age durations as `rm`.
#[cfg(unix)]
#[test]
fn list_and_status_take_ages_in_hours_and_minutes() {
    let sb = Sandbox::new("age-hours-list");
    sb.touch("three-hours.txt");
    sb.touch("fresh.txt");
    let put = sb.run(&["put", "three-hours.txt", "fresh.txt"]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    backdate_trashinfo(&sb, "three-hours.txt", &local_date_ago(3 * 3600));
    let pin = format!("--trash-dir={}", sb.trash().display());
    let out = sb.run(&["list", &pin, "--older-than=2h"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let listed = stdout_of(&out);
    assert!(
        listed.contains("three-hours.txt") && !listed.contains("fresh.txt"),
        "{listed}"
    );
    let out = sb.run(&["list", &pin, "--newer-than", "1h30m"]);
    let listed = stdout_of(&out);
    assert!(
        listed.contains("fresh.txt") && !listed.contains("three-hours.txt"),
        "{listed}"
    );
    let out = sb.run(&["status", &pin, "--older-than=2h", "--json"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains(r#""items":1,"#),
        "{}",
        stdout_of(&out)
    );
    let out = sb.run(&["status", &pin, "--older-than=4h", "--json"]);
    assert!(
        stdout_of(&out).contains(r#""items":0,"#),
        "{}",
        stdout_of(&out)
    );
    let out = sb.run(&["list", &pin, "--older-than=1"]);
    assert!(
        !stdout_of(&out).contains("three-hours.txt"),
        "{}",
        stdout_of(&out)
    );
    for sub in ["list", "status"] {
        assert_eq!(
            sb.run(&[sub, &pin, "--older-than=2x"]).status.code(),
            Some(2),
            "{sub}"
        );
    }
}

#[test]
fn restore_plain_order_defaults_oldest_first_and_flags_flip_it() {
    let sb = Sandbox::new("restore-order");
    let old = sb.touch("old.txt");
    let new = sb.touch("new.txt");
    assert!(sb.run(&["put", "old.txt", "new.txt"]).status.success());
    backdate_trashinfo(&sb, "old.txt", "2001-01-01T00:00:00");
    let pin = format!("--trash-dir={}", sb.trash().display());

    // Numbered table keeps oldest first: index 0 is the backdated item.
    let out = sb.run_with_stdin(&["restore", &pin, "--plain"], "0\n");
    assert!(out.status.success(), "{}", stderr_of(&out));
    let table = stdout_of(&out);
    let first = table.lines().next().unwrap_or("");
    assert!(first.contains("old.txt"), "{table}");
    assert!(old.exists() && !new.exists());

    // --newest-first flips the table: index 0 is now the latest item.
    assert!(sb.run(&["put", "old.txt"]).status.success());
    backdate_trashinfo(&sb, "old.txt", "2001-01-01T00:00:00");
    let out = sb.run_with_stdin(&["restore", &pin, "--plain", "--newest-first"], "0\n");
    assert!(out.status.success(), "{}", stderr_of(&out));
    let table = stdout_of(&out);
    let first = table.lines().next().unwrap_or("");
    assert!(first.contains("new.txt"), "{table}");
    assert!(new.exists() && !old.exists());
}

#[test]
fn restore_by_name_latest_json() {
    let sb = Sandbox::new("restore-name");
    let f = sb.touch("note.txt");
    assert!(sb.run(&["put", "note.txt"]).status.success());
    backdate_trashinfo(&sb, "note.txt", "2001-01-01T00:00:00");
    fs::write(&f, b"newer").unwrap();
    assert!(sb.run(&["put", "note.txt"]).status.success());
    assert_eq!(trash_names(&sb).len(), 2);
    let pin = format!("--trash-dir={}", sb.trash().display());

    // Two items share the name; without --latest the table goes to stderr
    // and stdout carries only the JSON summary.
    let out = sb.run_with_stdin(&["restore", &pin, "--json", "--name=note.txt"], "q\n");
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("note.txt"), "{}", stderr_of(&out));
    assert!(!f.exists());

    let out = sb.run(&["restore", &pin, "--json", "--latest", "--name=note.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let json = stdout_of(&out);
    assert!(json.starts_with("{\"restored\":[{\"original\":"), "{json}");
    assert!(
        json.trim_end().ends_with("\"failed\":0,\"skipped\":0}"),
        "{json}"
    );
    assert!(!json.contains("restored '"), "{json}");
    assert_eq!(fs::read(&f).unwrap(), b"newer");
    assert_eq!(trash_names(&sb), vec!["note.txt".to_string()]);

    let out = sb.run(&["restore", &pin, "--json", "--name=missing.txt"]);
    assert!(!out.status.success());
    assert_eq!(
        stdout_of(&out).trim(),
        "{\"restored\":[],\"failed\":0,\"skipped\":0}"
    );

    let out = sb.run(&["restore", "note.txt", "--name=note.txt"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn restore_dry_run_reports_and_moves_nothing() {
    let sb = Sandbox::new("restore-dry");
    let f = sb.touch("a.txt");
    assert!(sb.run(&["put", "a.txt"]).status.success());
    let pin = format!("--trash-dir={}", sb.trash().display());

    let out = sb.run(&["restore", &pin, "-n", "--name=a.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(
        stdout_of(&out).trim(),
        format!("would restore '{}'", f.display())
    );
    assert!(!f.exists());
    assert_eq!(trash_names(&sb), vec!["a.txt".to_string()]);

    // A blocked destination fails under the default policy, and renames
    // under --conflict=rename, still without moving anything.
    fs::write(&f, b"live").unwrap();
    let out = sb.run(&["restore", &pin, "--dry-run", "--name=a.txt"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stdout_of(&out).starts_with("would fail: "),
        "{}",
        stdout_of(&out)
    );
    let out = sb.run(&[
        "restore",
        &pin,
        "-n",
        "--conflict=rename",
        "--json",
        "--name=a.txt",
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains("as '"), "{}", stderr_of(&out));
    let json = stdout_of(&out);
    assert!(
        json.starts_with("{\"dry_run\":true,\"restored\":[{"),
        "{json}"
    );
    assert!(json.contains("a.1.txt"), "{json}");
    assert_eq!(fs::read(&f).unwrap(), b"live");
    assert!(!sb.work().join("a.1.txt").exists());
    assert_eq!(trash_names(&sb), vec!["a.txt".to_string()]);

    // --batch hands the dry run to undo.
    let out = sb.run(&["restore", &pin, "-n", "--batch=last", "--conflict=rename"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("would restore"),
        "{}",
        stdout_of(&out)
    );
    assert_eq!(trash_names(&sb), vec!["a.txt".to_string()]);
}

#[cfg(unix)]
#[test]
fn restore_glob_regex_ranges_to_and_conflicts() {
    let sb = Sandbox::new("restore-match");
    for n in ["a.rs", "b.rs", "c.txt", "d.txt"] {
        sb.touch(n);
    }
    assert!(sb
        .run(&["put", "a.rs", "b.rs", "c.txt", "d.txt"])
        .status
        .success());
    let pin = format!("--trash-dir={}", sb.trash().display());

    // --glob with --all restores every match, nothing else.
    let out = sb.run(&["restore", &pin, "--glob=*.rs", "--all"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(sb.work().join("a.rs").exists() && sb.work().join("b.rs").exists());
    assert!(!sb.work().join("c.txt").exists());

    // --to restores into another directory; a clash there gets renamed.
    let dest = sb.root.join("dest");
    fs::create_dir(&dest).unwrap();
    fs::write(dest.join("c.txt"), b"keep").unwrap();
    let to = format!("--to={}", dest.display());
    let out = sb.run(&[
        "restore",
        &pin,
        "--regex=c\\.txt$",
        &to,
        "--conflict=rename",
        "--json",
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let json = stdout_of(&out);
    assert!(json.contains("c.1.txt\""), "{json}");
    assert_eq!(fs::read(dest.join("c.txt")).unwrap(), b"keep");
    assert_eq!(fs::read(dest.join("c.1.txt")).unwrap(), b"payload");

    // skip leaves the item in the trash and still exits 0.
    fs::write(sb.work().join("d.txt"), b"live").unwrap();
    let out = sb.run(&["restore", &pin, "--name=d.txt", "--conflict=skip", "--json"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("\"skipped\":1"),
        "{}",
        stdout_of(&out)
    );
    assert_eq!(trash_names(&sb), vec!["d.txt".to_string()]);

    // A range on stdin restores several rows of the numbered table.
    for n in ["e1", "e2", "e3"] {
        sb.touch(n);
    }
    assert!(sb.run(&["put", "e1", "e2", "e3"]).status.success());
    fs::remove_file(sb.work().join("d.txt")).unwrap();
    let out = sb.run_with_stdin(&["restore", &pin, "--plain"], "0-1,3\n");
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(trash_names(&sb).len(), 1, "{}", stdout_of(&out));

    let bad = sb.run(&["restore", &pin, "--conflict=maybe"]);
    assert_eq!(bad.status.code(), Some(2));
    let bad = sb.run(&["restore", &pin, "--glob=*", "--regex=x"]);
    assert_eq!(bad.status.code(), Some(2));
}

#[cfg(unix)]
#[test]
fn restore_match_reads_path_as_a_kind() {
    let sb = Sandbox::new("restore-match-kinds");
    for n in ["old-a.txt", "old-b.txt", "new.txt"] {
        sb.touch(n);
    }
    assert!(sb
        .run(&["put", "old-a.txt", "old-b.txt", "new.txt"])
        .status
        .success());
    let pin = format!("--trash-dir={}", sb.trash().display());

    // A substring, in the -m KIND form trashy takes.
    let out = sb.run(&["restore", &pin, "-m", "substring", "/old-a"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(sb.work().join("old-a.txt").exists());

    // The whole original path, as put recorded it.
    let info = fs::read_to_string(sb.trash().join("info/old-b.txt.trashinfo")).unwrap();
    let exact = info
        .lines()
        .find_map(|l| l.strip_prefix("Path="))
        .unwrap()
        .to_string();
    let out = sb.run(&["restore", &pin, "--match=exact", &exact]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(sb.work().join("old-b.txt").exists());
    assert_eq!(trash_names(&sb), ["new.txt"]);

    // No PATTERN, a clash with --glob, or a bad kind exit 2.
    for bad in [
        vec!["restore", pin.as_str(), "--match=regex"],
        vec!["restore", pin.as_str(), "--match=glob", "--glob=*", "x"],
        vec!["restore", pin.as_str(), "--match=fuzzy", "x"],
    ] {
        let out = sb.run(&bad);
        assert_eq!(out.status.code(), Some(2), "{bad:?}: {}", stderr_of(&out));
    }
    assert_eq!(trash_names(&sb), ["new.txt"]);
}

#[cfg(unix)]
#[test]
fn list_size_and_sort() {
    let sb = Sandbox::new("list-sort");
    fs::write(sb.work().join("big"), vec![1u8; 64 * 1024]).unwrap();
    fs::write(sb.work().join("small"), b"x").unwrap();
    assert!(sb.run(&["put", "big", "small"]).status.success());
    let pin = format!("--trash-dir={}", sb.trash().display());

    let out = stdout_of(&sb.run(&["list", &pin, "--sort=size"]));
    let rows: Vec<&str> = out.lines().collect();
    assert_eq!(rows.len(), 2, "{out}");
    assert!(
        rows[0].ends_with("/big") && rows[0].contains("KiB"),
        "{out}"
    );
    let out = stdout_of(&sb.run(&["list", &pin, "--sort=name", "--reverse"]));
    assert!(
        out.lines().next().unwrap_or("").ends_with("/small"),
        "{out}"
    );
    let json = stdout_of(&sb.run(&["list", &pin, "--size", "--json"]));
    assert!(json.contains("\"size\":"), "{json}");
    assert_eq!(sb.run(&["list", "--sort=colour"]).status.code(), Some(2));
}

#[test]
fn list_status_and_rm_filter_by_size() {
    let sb = Sandbox::new("size-filter");
    fs::write(sb.work().join("big"), vec![1u8; 64 * 1024]).unwrap();
    fs::write(sb.work().join("small"), b"x").unwrap();
    assert!(sb.run(&["put", "big", "small"]).status.success());
    // A payload with no .trashinfo has a size too, so a size filter keeps it.
    fs::write(
        sb.trash().join("files").join("stray"),
        vec![2u8; 200 * 1024],
    )
    .unwrap();
    let pin = format!("--trash-dir={}", sb.trash().display());
    let names = |args: &[&str]| -> Vec<String> {
        let out = sb.run(args);
        assert!(out.status.success(), "{args:?}: {}", stderr_of(&out));
        let mut v: Vec<String> = stdout_of(&out)
            .lines()
            .map(|l| l.rsplit(['/', '\\']).next().unwrap_or("").to_string())
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        names(&["list", &pin, "--larger-than=32K"]),
        ["big", "stray"]
    );
    assert_eq!(names(&["list", &pin, "--smaller-than", "32K"]), ["small"]);
    assert_eq!(
        names(&["list", &pin, "--larger-than=32K", "--smaller-than=100K"]),
        ["big"]
    );
    // The size column stays opt-in; the filter alone does not add it.
    let out = stdout_of(&sb.run(&["list", &pin, "--larger-than=100K"]));
    assert!(!out.contains("KiB"), "{out}");
    let json = stdout_of(&sb.run(&["status", &pin, "--json", "--larger-than=32K"]));
    assert!(json.contains("\"total_items\": 2"), "{json}");
    assert!(json.contains("\"orphans\":1"), "{json}");
    let dry = sb.run(&["rm", &pin, "-n", "--smaller-than=32K", "--force", "*"]);
    assert_eq!(stdout_of(&dry).lines().count(), 1, "{}", stdout_of(&dry));
    assert!(stdout_of(&dry).trim_end().ends_with("small"));
    let gone = sb.run(&["rm", &pin, "--larger-than=100K", "--force", "*"]);
    assert!(gone.status.success(), "{}", stderr_of(&gone));
    assert_eq!(names(&["list", &pin]), ["big", "small"]);
    for bad in [
        &["list", "--larger-than=lots"][..],
        &["list", "--smaller-than"],
        &["status", "--larger-than=1M", "--smaller-than=1M"],
        &["rm", "--larger-than=-5", "x"],
    ] {
        assert_eq!(sb.run(bad).status.code(), Some(2), "{bad:?}");
    }
}

#[test]
fn restore_cwd_only_skips_items_outside_cwd() {
    let sb = Sandbox::new("restore-cwd");
    let in_work = sb.touch("local.txt");
    let outer = sb.root.join("outside.txt");
    fs::write(&outer, b"outer").unwrap();
    assert!(sb.run(&["put", "local.txt"]).status.success());
    // Absolute path so original is outside work/.
    assert!(common::rtrash(&sb.root)
        .args(["put", outer.to_str().unwrap()])
        .current_dir(sb.work())
        .status()
        .unwrap()
        .success());
    assert!(!in_work.exists());
    assert!(!outer.exists());

    // --cwd-only: only local.txt (under work/) is selectable → single match auto-restores.
    let out = sb.run(&["restore", "--cwd-only"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(in_work.exists(), "local restored");
    assert!(!outer.exists(), "outer still trashed with --cwd-only");
}

#[test]
fn restore_refuses_overwrite_without_force() {
    let sb = Sandbox::new("restore-clash");
    sb.touch("c.txt");
    sb.run(&["put", "c.txt"]);
    fs::write(sb.work().join("c.txt"), b"newer").unwrap();

    let out = sb.run(&["restore", "c.txt"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr_of(&out).contains("refusing to overwrite"));
    assert_eq!(fs::read(sb.work().join("c.txt")).unwrap(), b"newer");

    let out = sb.run(&["restore", "-f", "c.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(sb.work().join("c.txt")).unwrap(), b"payload");
}

#[test]
fn force_interactive_last_wins_like_gnu_rm() {
    let sb = Sandbox::new("fi-wins");
    // -if: force last → ignore missing, exit 0 (GNU rm last-wins).
    let out = sb.run(&["put", "-if", "nope"]);
    assert!(out.status.success(), "{}", stderr_of(&out));

    // -fi: interactive last clears force → missing still errors.
    let out = sb.run(&["put", "-fi", "nope"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains("No such file or directory"));
}

#[test]
fn restore_force_replaces_directory_at_dest() {
    let sb = Sandbox::new("restore-force-dir");
    sb.touch("x");
    assert!(sb.run(&["put", "x"]).status.success());
    // Recreate original path as a non-empty directory; rename alone cannot replace it.
    fs::create_dir(sb.work().join("x")).unwrap();
    fs::write(sb.work().join("x/inner"), b"blocker").unwrap();

    let out = sb.run(&["restore", "-f", "x"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(sb.work().join("x")).unwrap(), b"payload");
    assert!(!sb.work().join("x/inner").exists());
}

#[test]
fn multicall_names_dispatch() {
    let sb = Sandbox::new("multicall");
    let bindir = sb.root.join("bin");
    fs::create_dir_all(&bindir).unwrap();
    for name in [
        "rm",
        "trash",
        "trash-put",
        "trash-empty",
        "trash-list",
        "trash-restore",
        "trash-rm",
    ] {
        std::os::unix::fs::symlink(bin(), bindir.join(name)).unwrap();
    }
    let run_as = |name: &str, args: &[&str]| {
        Command::new(bindir.join(name))
            .args(args)
            .current_dir(sb.work())
            .env("XDG_DATA_HOME", sb.root.join("xdg"))
            .env("HOME", &sb.root)
            .output()
            .unwrap()
    };

    sb.touch("m.txt");
    assert!(run_as("rm", &["m.txt"]).status.success());
    assert!(!sb.work().join("m.txt").exists());
    assert_eq!(trash_names(&sb), vec!["m.txt"]);

    let out = run_as("trash-list", &[]);
    assert!(stdout_of(&out).contains("m.txt"));

    let pin = format!("--trash-dir={}", sb.trash().display());
    assert!(run_as("trash-empty", &[&pin]).status.success());
    assert!(trash_names(&sb).is_empty());

    // `trash` argv0 is an alias for put (trash-cli style).
    sb.touch("via-trash.txt");
    assert!(run_as("trash", &["via-trash.txt"]).status.success());
    assert_eq!(trash_names(&sb), vec!["via-trash.txt"]);

    // trash-rm permanently deletes matching trash entries (not restore).
    let out = run_as("trash-rm", &["via-trash.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(trash_names(&sb).is_empty());
}

/// Records `list` leaves out (one that is not a regular file, one past
/// the record cap, one whose `Path=` climbs out of its volume) add one
/// stderr hint naming `status --check` after the same stdout, and a clean
/// trash gets no hint. trash-list prints trash-cli's own line for the
/// directory, which trash-cli cannot open either, and counts the other
/// two, which trash-cli would list, in its hint.
#[test]
fn list_hints_once_at_records_it_leaves_out() {
    let sb = Sandbox::new("list-unlisted");
    let uid = unsafe { libc::getuid() };
    let vol = sb.root.join("vol");
    let trash_root = vol.join(format!(".Trash-{uid}"));
    fs::create_dir_all(trash_root.join("files")).unwrap();
    fs::create_dir_all(trash_root.join("info")).unwrap();
    fs::write(trash_root.join("files/ok.txt"), b"payload").unwrap();
    fs::write(
        trash_root.join("info/ok.txt.trashinfo"),
        "[Trash Info]\nPath=docs/ok.txt\nDeletionDate=2026-01-02T03:04:05\n",
    )
    .unwrap();
    let pin = format!("--trash-dir={}", trash_root.display());
    let bindir = sb.root.join("bin");
    fs::create_dir_all(&bindir).unwrap();
    std::os::unix::fs::symlink(bin(), bindir.join("trash-list")).unwrap();
    let trash_list = || {
        Command::new(bindir.join("trash-list"))
            .arg(&pin)
            .current_dir(sb.work())
            .env("XDG_DATA_HOME", sb.root.join("xdg"))
            .env("HOME", &sb.root)
            .output()
            .unwrap()
    };
    let clean = sb.run(&["list", &pin]);
    assert!(clean.status.success(), "{}", stderr_of(&clean));
    assert!(
        stdout_of(&clean).contains("docs/ok.txt"),
        "{}",
        stdout_of(&clean)
    );
    assert_eq!(stderr_of(&clean), "", "a clean trash gets no hint");
    let clean_cli = trash_list();

    let info = trash_root.join("info");
    let bad = info.join("bad.txt.trashinfo");
    fs::create_dir(&bad).unwrap();
    let big = info.join("big.txt.trashinfo");
    fs::write(&big, vec![b'x'; (1 << 20) + 1]).unwrap();
    let out_of_volume = info.join("up.txt.trashinfo");
    fs::write(
        &out_of_volume,
        "[Trash Info]\nPath=../../up.txt\nDeletionDate=2026-01-02T03:04:05\n",
    )
    .unwrap();

    let out = sb.run(&["list", &pin]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(
        out.stdout, clean.stdout,
        "stdout is the same as for the clean trash"
    );
    let hint = "rtrash list: 3 trash records could not be listed; \
                `rtrash status --check` shows them";
    assert_eq!(
        stderr_of(&out),
        format!("{hint}\n"),
        "exactly one hint line"
    );
    let out = sb.run(&["list", "--json", &pin]);
    assert_eq!(
        common::lines_ending(&out, "shows them"),
        1,
        "{}",
        stderr_of(&out)
    );

    let cli = trash_list();
    assert_eq!(cli.status.code(), clean_cli.status.code());
    assert_eq!(
        cli.stdout, clean_cli.stdout,
        "trash-list stdout is unchanged"
    );
    assert_eq!(stderr_of(&clean_cli), "");
    let err = stderr_of(&cli);
    let lines: Vec<&str> = err.lines().collect();
    assert_eq!(lines.len(), 2, "{err}");
    assert!(lines[0].starts_with("[Errno 21] "), "{err}");
    assert!(
        lines[0].ends_with(&format!(": '{}'", bad.display())),
        "{err}"
    );
    assert_eq!(
        lines[1],
        "trash-list: 2 trash records could not be listed; \
         `rtrash status --check` shows them"
    );

    // The hint's command names each record it counted.
    let check = sb.run(&["status", "--check", &pin]);
    let report = format!("{}{}", stdout_of(&check), stderr_of(&check));
    for record in [&bad, &big, &out_of_volume] {
        let name = record.file_name().unwrap().to_string_lossy();
        assert!(report.contains(&*name), "{name} missing from: {report}");
    }

    fs::remove_dir(&bad).unwrap();
    fs::remove_file(&big).unwrap();
    fs::remove_file(&out_of_volume).unwrap();
    let out = sb.run(&["list", &pin]);
    assert_eq!(out.stdout, clean.stdout);
    assert_eq!(stderr_of(&out), "", "no hint once the trash is clean");
}

/// A record with no `Path=` line names no original: `status --check`
/// calls it a bad record and trash-cli's trash-list leaves it out. list
/// leaves it out too and counts it in its hint, with the same stdout as
/// without it; trash-list prints the same rows and trash-cli's parse
/// error line in place of the hint.
#[test]
fn list_leaves_out_a_record_with_no_path_and_counts_it() {
    let sb = Sandbox::new("list-pathless");
    let t = sb.trash();
    fs::create_dir_all(t.join("files")).unwrap();
    fs::create_dir_all(t.join("info")).unwrap();
    let good = sb.work().join("good.txt");
    fs::write(t.join("files/good.txt"), b"payload").unwrap();
    fs::write(
        t.join("info/good.txt.trashinfo"),
        format!(
            "[Trash Info]\nPath={}\nDeletionDate=2026-01-02T03:04:05\n",
            good.display()
        ),
    )
    .unwrap();
    let pin = format!("--trash-dir={}", t.display());
    let bindir = sb.root.join("bin");
    fs::create_dir_all(&bindir).unwrap();
    std::os::unix::fs::symlink(bin(), bindir.join("trash-list")).unwrap();
    let trash_list = || {
        Command::new(bindir.join("trash-list"))
            .arg(&pin)
            .current_dir(sb.work())
            .env("XDG_DATA_HOME", sb.root.join("xdg"))
            .env("HOME", &sb.root)
            .output()
            .unwrap()
    };
    let clean = sb.run(&["list", &pin]);
    assert!(clean.status.success(), "{}", stderr_of(&clean));
    assert!(
        stdout_of(&clean).contains("good.txt"),
        "{}",
        stdout_of(&clean)
    );
    assert_eq!(stderr_of(&clean), "");
    let clean_cli = trash_list();

    let record = t.join("info/nopath.txt.trashinfo");
    fs::write(t.join("files/nopath.txt"), b"payload").unwrap();
    fs::write(&record, "[Trash Info]\nDeletionDate=2026-01-02T03:04:05\n").unwrap();
    let out = sb.run(&["list", &pin]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(
        out.stdout, clean.stdout,
        "the record is left out of the rows"
    );
    let hint = "rtrash list: 1 trash record could not be listed; \
                `rtrash status --check` shows it";
    assert_eq!(stderr_of(&out), format!("{hint}\n"));
    let cli = trash_list();
    assert_eq!(cli.status.code(), clean_cli.status.code());
    assert_eq!(cli.stdout, clean_cli.stdout, "trash-cli leaves it out too");
    assert_eq!(stderr_of(&clean_cli), "");
    assert_eq!(
        stderr_of(&cli),
        format!("Parse Error: {}: Unable to parse Path.\n", record.display()),
        "trash-cli's line, and no hint for the same record"
    );
    let check = sb.run(&["status", "--check", &pin]);
    let report = format!("{}{}", stdout_of(&check), stderr_of(&check));
    assert!(report.contains("bad record:"), "{report}");
    assert!(
        report.contains("nopath.txt.trashinfo: no Path line"),
        "{report}"
    );
}

/// trash-list prints trash-cli's stderr line for each record trash-cli
/// reports (no `Path=` line, a record it cannot open), in path order and
/// before the rows, and its hint counts only the records trash-cli would
/// list (an empty `Path=`, one over the size cap). `rtrash list` prints
/// no such lines and counts all of them. Each record is reported once.
#[test]
fn trash_list_prints_trash_cli_lines_and_hints_the_rest() {
    let sb = Sandbox::new("trash-list-errors");
    let t = sb.trash();
    let info = t.join("info");
    fs::create_dir_all(t.join("files")).unwrap();
    fs::create_dir_all(&info).unwrap();
    let good = sb.work().join("good.txt");
    fs::write(t.join("files/good.txt"), b"payload").unwrap();
    fs::write(
        info.join("good.txt.trashinfo"),
        format!(
            "[Trash Info]\nPath={}\nDeletionDate=2026-01-02T03:04:05\n",
            good.display()
        ),
    )
    .unwrap();
    let pin = format!("--trash-dir={}", t.display());
    let bindir = sb.root.join("bin");
    fs::create_dir_all(&bindir).unwrap();
    std::os::unix::fs::symlink(bin(), bindir.join("trash-list")).unwrap();
    let trash_list = || {
        Command::new(bindir.join("trash-list"))
            .arg(&pin)
            .current_dir(sb.work())
            .env("XDG_DATA_HOME", sb.root.join("xdg"))
            .env("HOME", &sb.root)
            .output()
            .unwrap()
    };
    let clean = trash_list();
    assert!(clean.status.success(), "{}", stderr_of(&clean));
    assert_eq!(stderr_of(&clean), "");

    // trash-cli reports these three.
    let nopath = info.join("nopath.txt.trashinfo");
    fs::write(&nopath, "[Trash Info]\nDeletionDate=2026-01-02T03:04:05\n").unwrap();
    let dir = info.join("d.trashinfo");
    fs::create_dir(&dir).unwrap();
    let dangling = info.join("dl.trashinfo");
    std::os::unix::fs::symlink(sb.root.join("nowhere"), &dangling).unwrap();
    // trash-cli would list these two; only the hint counts them.
    fs::write(
        info.join("empty.txt.trashinfo"),
        "[Trash Info]\nPath=\nDeletionDate=2026-01-02T03:04:05\n",
    )
    .unwrap();
    fs::write(info.join("big.txt.trashinfo"), vec![b'x'; (1 << 20) + 1]).unwrap();

    let cli = trash_list();
    assert!(cli.status.success(), "{}", stderr_of(&cli));
    assert_eq!(cli.stdout, clean.stdout, "the rows stay the same");
    let err = stderr_of(&cli);
    let lines: Vec<&str> = err.lines().collect();
    assert_eq!(lines.len(), 4, "{err}");
    assert!(lines[0].starts_with("[Errno 21] "), "{err}");
    assert!(
        lines[0].ends_with(&format!(": '{}'", dir.display())),
        "{err}"
    );
    assert_eq!(
        lines[1],
        format!(
            "[Errno 2] No such file or directory: '{}'",
            dangling.display()
        )
    );
    assert_eq!(
        lines[2],
        format!("Parse Error: {}: Unable to parse Path.", nopath.display())
    );
    assert_eq!(
        lines[3],
        "trash-list: 2 trash records could not be listed; \
         `rtrash status --check` shows them"
    );

    let out = sb.run(&["list", &pin]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(out.stdout, clean.stdout);
    assert_eq!(
        stderr_of(&out),
        "rtrash list: 5 trash records could not be listed; \
         `rtrash status --check` shows them\n"
    );
}

/// Under the trash-list name, `--size` and trash-cli's hidden `--files`
/// keep trash-cli's rows: the payload's `st_size` in bytes in place of
/// the date (a directory's own size, 0 for a dangling link), and
/// ` -> FILE` naming the item in the trash. `rtrash list` keeps its own
/// rows and takes no `--files`.
#[test]
fn trash_list_size_and_files_keep_trash_cli_rows() {
    let sb = Sandbox::new("trash-list-size-files");
    let t = sb.trash();
    let files = t.join("files");
    fs::create_dir_all(&files).unwrap();
    fs::create_dir_all(t.join("info")).unwrap();
    let w = sb.work();
    let record = |name: &str, second: u32| {
        fs::write(
            t.join(format!("info/{name}.trashinfo")),
            format!(
                "[Trash Info]\nPath={}\nDeletionDate=2026-01-02T03:04:0{second}\n",
                w.join(name).display()
            ),
        )
        .unwrap();
    };
    fs::write(files.join("a.txt"), b"hello\n").unwrap();
    record("a.txt", 1);
    fs::create_dir_all(files.join("d/sub")).unwrap();
    fs::write(files.join("d/sub/big"), vec![0u8; 5000]).unwrap();
    record("d", 2);
    std::os::unix::fs::symlink(sb.root.join("nowhere"), files.join("dl")).unwrap();
    record("dl", 3);
    std::os::unix::fs::symlink("a.txt", files.join("gl")).unwrap();
    record("gl", 4);
    let dir_size = fs::metadata(files.join("d")).unwrap().len();

    let pin = format!("--trash-dir={}", t.display());
    let bindir = sb.root.join("bin");
    fs::create_dir_all(&bindir).unwrap();
    std::os::unix::fs::symlink(bin(), bindir.join("trash-list")).unwrap();
    let trash_list = |args: &[&str]| {
        Command::new(bindir.join("trash-list"))
            .arg(&pin)
            .args(args)
            .current_dir(&w)
            .env("XDG_DATA_HOME", sb.root.join("xdg"))
            .env("HOME", &sb.root)
            .output()
            .unwrap()
    };
    let (wd, fd) = (w.display(), files.display());
    let sizes = format!("6 {wd}/a.txt\n{dir_size} {wd}/d\n0 {wd}/dl\n6 {wd}/gl\n");
    let out = trash_list(&["--size"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(stdout_of(&out), sizes);
    assert_eq!(stderr_of(&out), "");
    let dated: Vec<String> = ["a.txt", "d", "dl", "gl"]
        .iter()
        .zip(1..)
        .map(|(n, s)| format!("2026-01-02 03:04:0{s} {wd}/{n} -> {fd}/{n}\n"))
        .collect();
    let out = trash_list(&["--files"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(stdout_of(&out), dated.concat());
    let both: String = sizes
        .lines()
        .zip(["a.txt", "d", "dl", "gl"])
        .map(|(l, n)| format!("{l} -> {fd}/{n}\n"))
        .collect();
    let out = trash_list(&["--size", "--files"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(stdout_of(&out), both);

    // --files is a plain-row shape; JSON keeps its byte size field.
    let out = trash_list(&["--files", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr_of(&out).contains("--files works on plain rows"));
    let out = trash_list(&["--size", "--json"]);
    assert!(stdout_of(&out).contains("\"size\":"), "{}", stdout_of(&out));
    // rtrash list keeps its date and disk usage column, and no --files.
    let out = sb.run(&["list", &pin, "--size"]);
    let first = stdout_of(&out).lines().next().unwrap_or("").to_string();
    assert!(first.starts_with("2026-01-02 03:04:01 "), "{first}");
    assert!(first.ends_with(&format!("{wd}/a.txt")), "{first}");
    let out = sb.run(&["list", &pin, "--files"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr_of(&out).contains("unrecognized option '--files'"));

    // A record whose payload is gone: trash-cli's --size stops with a
    // traceback; trash-list names it, lists the rest and exits 1.
    record("gone", 5);
    let out = trash_list(&["--size"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stdout_of(&out), sizes);
    let err = stderr_of(&out);
    let want = format!("trash-list: cannot read the size of {fd}/gone: ");
    assert!(err.starts_with(&want) && err.lines().count() == 1, "{err}");
    let out = trash_list(&["--files"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let gone = format!("2026-01-02 03:04:05 {wd}/gone -> {fd}/gone\n");
    assert_eq!(stdout_of(&out), dated.concat() + &gone);
}

#[test]
fn trash_cli_command_lines_work_under_the_trash_names() {
    let sb = Sandbox::new("trash-cli-lines");
    let bindir = sb.root.join("bin");
    fs::create_dir_all(&bindir).unwrap();
    for name in ["trash-put", "trash-empty", "trash-list", "trash-rm"] {
        std::os::unix::fs::symlink(bin(), bindir.join(name)).unwrap();
    }
    let run_as = |name: &str, args: &[&str], stdin: &str| {
        use std::io::Write;
        let mut child = Command::new(bindir.join(name))
            .args(args)
            .current_dir(sb.work())
            .env("XDG_DATA_HOME", sb.root.join("xdg"))
            .env("HOME", &sb.root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let t = sb.trash().display().to_string();
    sb.touch("a");
    let out = run_as("trash-put", &["--trash-dir", &t, "a"], "");
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(trash_names(&sb), vec!["a"]);
    // trash-cli exits 74 when a file cannot be trashed.
    let out = run_as("trash-put", &["missing"], "");
    assert_eq!(out.status.code(), Some(74));

    let out = run_as("trash-list", &["--trash-dir", &t], "");
    assert!(stdout_of(&out).contains("/a"), "{}", stdout_of(&out));
    let out = run_as("trash-list", &["--trash-dirs", "--trash-dir", &t], "");
    assert_eq!(stdout_of(&out).trim(), t);
    let out = run_as("trash-list", &["--volumes"], "");
    // Linux lists the mounts with the root; macOS, home-only, lists none.
    assert!(out.status.success(), "{}", stderr_of(&out));
    let vols = stdout_of(&out);
    assert!(vols.lines().all(|l| l.starts_with('/')), "{vols}");
    #[cfg(target_os = "linux")]
    assert!(vols.lines().any(|l| l == "/"), "{vols}");
    let out = run_as("trash-list", &["--print-completion", "bash"], "");
    assert!(stdout_of(&out).contains("trash-restore"));
    let out = run_as("trash-list", &["--print-completion=tcsh"], "");
    assert_eq!(out.status.code(), Some(2));

    // -i asks first; anything but y keeps the trash.
    let out = run_as("trash-empty", &["-i", "--trash-dir", &t], "n\n");
    assert!(stderr_of(&out).contains("Proceed? (y/N)"));
    // Each trash line names its size and item count.
    let err = stderr_of(&out);
    assert!(err.contains(", 1 item)"), "{err}");
    assert_eq!(trash_names(&sb), vec!["a"]);
    let out = run_as("trash-empty", &["-i", "--trash-dir", &t], "y\n");
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(trash_names(&sb).is_empty());

    let out = run_as("trash-rm", &["--trash-dir", &t, "nothing"], "");
    assert!(out.status.success(), "{}", stderr_of(&out));
}

#[test]
fn trash_restore_name_follows_trash_cli() {
    let sb = Sandbox::new("trash-restore-cli");
    let bindir = sb.root.join("bin");
    fs::create_dir_all(&bindir).unwrap();
    std::os::unix::fs::symlink(bin(), bindir.join("trash-restore")).unwrap();
    let run_in = |dir: &std::path::Path, args: &[&str], stdin: &str| {
        use std::io::Write;
        let mut child = Command::new(bindir.join("trash-restore"))
            .args(args)
            .current_dir(dir)
            .env("XDG_DATA_HOME", sb.root.join("xdg"))
            .env("HOME", &sb.root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    fs::create_dir(sb.work().join("sub")).unwrap();
    sb.touch("x");
    sb.touch("sub/y");
    assert!(sb.run(&["put", "x", "sub/y"]).status.success());
    let t = format!("--trash-dir={}", sb.trash().display());

    // Outside the trashed paths, nothing is in scope.
    let other = sb.root.join("other");
    fs::create_dir(&other).unwrap();
    let out = run_in(&other, &[&t], "");
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout_of(&out).starts_with("No files trashed from current dir"));

    // From the work dir, both items are offered, by path, one space apart.
    let out = run_in(&sb.work(), &[&t, "--sort", "path"], "");
    assert!(out.status.success(), "{}", stderr_of(&out));
    let s = stdout_of(&out);
    let rows: Vec<&str> = s.lines().filter(|l| l.starts_with("   ")).collect();
    assert_eq!(rows.len(), 2, "{s}");
    assert!(
        rows[0].starts_with("   0 2") && rows[0].ends_with("/sub/y"),
        "{s}"
    );
    assert!(rows[1].ends_with("/x"), "{s}");
    assert!(s.contains("No files were restored"));

    // PATH is a scope, and a single match still asks.
    let sub = sb.work().join("sub");
    let out = run_in(&sb.work(), &[&t, sub.to_str().unwrap()], "0\n");
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(stdout_of(&out).contains("   0 "));
    assert!(sub.join("y").exists());
    assert!(!sb.work().join("x").exists());

    // --overwrite replaces an existing file, as -f does.
    fs::write(sb.work().join("x"), b"new").unwrap();
    let out = run_in(&sb.work(), &[&t, "--overwrite"], "0\n");
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_ne!(fs::read(sb.work().join("x")).unwrap(), b"new");
}

#[test]
fn trash_rm_removes_match_keeps_other() {
    let sb = Sandbox::new("trash-rm-select");
    sb.touch("keep.dat");
    sb.touch("drop.o");
    sb.touch("also.o");
    assert!(sb
        .run(&["put", "keep.dat", "drop.o", "also.o"])
        .status
        .success());
    assert_eq!(trash_names(&sb).len(), 3);

    // --home-only: a bare rm would also delete the host's trashed *.o.
    let out = sb.run(&["rm", "--home-only", "*.o"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let mut names = trash_names(&sb);
    names.sort();
    assert_eq!(names, vec!["keep.dat"], "only non-matching entry remains");
    assert!(sb.trash().join("info/keep.dat.trashinfo").exists());
    assert!(!sb.trash().join("info/drop.o.trashinfo").exists());
    assert!(!sb.trash().join("files/drop.o").exists());
}

#[test]
fn trash_rm_literal_basename() {
    let sb = Sandbox::new("trash-rm-literal");
    sb.touch("foo");
    sb.touch("foobar");
    assert!(sb.run(&["put", "foo", "foobar"]).status.success());
    let out = sb.run(&["rm", "--home-only", "foo"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let mut names = trash_names(&sb);
    names.sort();
    assert_eq!(names, vec!["foobar"]);
}

#[test]
fn list_and_restore_respect_trash_dir_pin() {
    let sb = Sandbox::new("pin-list-restore");
    let f = sb.touch("pinned.txt");
    assert!(sb.run(&["put", "pinned.txt"]).status.success());

    let pin = format!("--trash-dir={}", sb.trash().display());
    let out = sb.run(&["list", &pin]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains(&f.display().to_string()),
        "{}",
        stdout_of(&out)
    );

    // Pin to an empty foreign trash dir: list must not show the real entry.
    let foreign = sb.root.join("other-trash");
    fs::create_dir_all(foreign.join("files")).unwrap();
    fs::create_dir_all(foreign.join("info")).unwrap();
    let foreign_pin = format!("--trash-dir={}", foreign.display());
    let out = sb.run(&["list", &foreign_pin]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        !stdout_of(&out).contains("pinned.txt"),
        "pinned foreign trash must hide home entries: {}",
        stdout_of(&out)
    );

    // Restore with the correct pin brings the file back.
    let out = sb.run(&["restore", &pin, "pinned.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(f.exists());
    assert_eq!(fs::read(&f).unwrap(), b"payload");
}

/// Volume trash stores relative Path=; --trash-dir pins must infer topdir so
/// list/restore/rm agree with unpinned volume entries (absolute originals).
#[test]
fn volume_trash_pin_resolves_relative_path() {
    let sb = Sandbox::new("volume-pin");
    let uid = unsafe { libc::getuid() };
    let vol = sb.root.join("vol");
    let trash_root = vol.join(format!(".Trash-{uid}"));
    fs::create_dir_all(trash_root.join("files")).unwrap();
    fs::create_dir_all(trash_root.join("info")).unwrap();
    fs::create_dir_all(vol.join("docs")).unwrap();

    let seed = |payload: &[u8]| {
        fs::write(trash_root.join("files/rel.txt"), payload).unwrap();
        fs::write(
            trash_root.join("info/rel.txt.trashinfo"),
            "[Trash Info]\nPath=docs/rel.txt\nDeletionDate=2026-01-02T03:04:05\n",
        )
        .unwrap();
    };
    seed(b"vol-payload");

    let pin = format!("--trash-dir={}", trash_root.display());
    let expect_abs = vol.join("docs/rel.txt");
    let expect_s = expect_abs.display().to_string();

    let out = sb.run(&["list", &pin]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let listed = stdout_of(&out);
    assert!(
        listed.contains(&expect_s),
        "pinned list must resolve relative Path= to absolute under topdir, got: {listed}"
    );
    // Unresolved relative path would appear as a bare "docs/rel.txt" token.
    let only_relative = listed.lines().any(|l| {
        l.ends_with(" docs/rel.txt") || l == "docs/rel.txt" || l.ends_with("\tdocs/rel.txt")
    });
    assert!(
        !only_relative,
        "must not list unresolved relative Path= alone: {listed}"
    );

    let out = sb.run(&["restore", &pin, &expect_s]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        expect_abs.is_file(),
        "restore must land at absolute original"
    );
    assert_eq!(fs::read(&expect_abs).unwrap(), b"vol-payload");
    assert!(!trash_root.join("files/rel.txt").exists());

    // trash-rm by full absolute original must match after topdir resolve.
    seed(b"again");
    let _ = fs::remove_file(&expect_abs);
    let out = sb.run(&["rm", &pin, &expect_s]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!trash_root.join("files/rel.txt").exists());
    assert!(!trash_root.join("info/rel.txt.trashinfo").exists());
}

/// A volume trash restores only into its own volume, as trash-cli 0.26.9
/// does: a record whose Path= climbs out with `..`, or names an absolute
/// path elsewhere, is left out of the list (counted in the hint) and never
/// restored. put refuses to fill a pinned volume trash with an item from
/// outside its volume, since that record would need such a Path=.
#[test]
fn volume_trash_keeps_restores_and_puts_inside_its_volume() {
    let sb = Sandbox::new("volume-outside");
    let uid = unsafe { libc::getuid() };
    let vol = sb.root.join("vol");
    let outside = sb.root.join("outside");
    let trash_root = vol.join(format!(".Trash-{uid}"));
    fs::create_dir_all(trash_root.join("files")).unwrap();
    fs::create_dir_all(trash_root.join("info")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let abs = outside.join("abs.txt");
    let climb = outside.join("climb.txt");
    let records = [
        ("ok.txt", "ok.txt".to_string()),
        ("abs.txt", abs.display().to_string()),
        ("climb.txt", "../outside/climb.txt".to_string()),
    ];
    for (name, path) in &records {
        fs::write(trash_root.join("files").join(name), b"payload").unwrap();
        fs::write(
            trash_root.join("info").join(format!("{name}.trashinfo")),
            format!("[Trash Info]\nPath={path}\nDeletionDate=2026-01-02T03:04:05\n"),
        )
        .unwrap();
    }
    let pin = format!("--trash-dir={}", trash_root.display());

    let out = sb.run(&["list", &pin]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let listed = stdout_of(&out);
    assert!(
        listed.contains(&vol.join("ok.txt").display().to_string()),
        "{listed}"
    );
    assert!(
        !listed.contains("abs.txt") && !listed.contains("climb.txt"),
        "{listed}"
    );
    assert!(
        stderr_of(&out).contains("2 trash records could not be listed"),
        "{}",
        stderr_of(&out)
    );

    for target in [&abs, &climb] {
        let out = sb.run(&["restore", &pin, target.to_str().unwrap()]);
        assert!(!out.status.success(), "{}", stdout_of(&out));
        assert!(!target.exists(), "{} was restored", target.display());
    }
    assert!(trash_root.join("files/abs.txt").exists());
    assert!(trash_root.join("files/climb.txt").exists());
    let ok = vol.join("ok.txt");
    let out = sb.run(&["restore", &pin, ok.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(ok.is_file());

    let item = sb.touch("from-work.txt");
    let out = sb.run(&["put", &pin, "from-work.txt"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("keeps only items from"),
        "{}",
        stderr_of(&out)
    );
    assert!(item.exists());
    assert!(!trash_root.join("info/from-work.txt.trashinfo").exists());
}

/// Multi-file directory payload must be fully removed via the shipped empty
/// path (fastdelete / full wipe), not only a single top-level name.
#[test]
fn empty_removes_deep_directory_payload() {
    let sb = Sandbox::new("empty-deep");
    let root = sb.work().join("deep");
    fs::create_dir_all(root.join("a/b/c")).unwrap();
    for i in 0..50 {
        fs::write(root.join(format!("a/b/c/f{i}")), format!("body{i}")).unwrap();
    }
    fs::write(root.join("a/leaf"), b"leaf").unwrap();
    assert!(sb.run(&["put", "-r", "deep"]).status.success());
    assert!(sb.trash().join("files/deep/a/b/c/f0").is_file());

    let out = sb.empty(&[]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(trash_names(&sb).is_empty());
    assert!(!sb.trash().join("files/deep").exists());
    assert!(fs::read_dir(sb.trash().join("info"))
        .unwrap()
        .next()
        .is_none());
}

#[test]
fn directory_put_writes_directorysizes_empty_prunes() {
    let sb = Sandbox::new("dirsizes");
    fs::create_dir(sb.work().join("tree")).unwrap();
    fs::write(sb.work().join("tree/a"), b"aaaa").unwrap();
    fs::write(sb.work().join("tree/b"), b"bbbbbbbb").unwrap();
    assert!(sb.run(&["put", "-r", "tree"]).status.success());

    let cache = sb.trash().join("directorysizes");
    assert!(cache.is_file(), "directory put must write directorysizes");
    let body = fs::read_to_string(&cache).unwrap();
    // FreeDesktop: "size mtime percent-encoded-name"
    let line = body.lines().next().expect("at least one cache line");
    let parts: Vec<&str> = line.split_whitespace().collect();
    assert!(parts.len() >= 3, "expected size mtime name, got {line:?}");
    let size: u64 = parts[0].parse().expect("size");
    // The spec asks for `du -B1` usage: blocks, the directory's own included.
    use std::os::unix::fs::MetadataExt;
    let payload = sb.trash().join("files/tree");
    let blocks = |p: &std::path::Path| fs::symlink_metadata(p).unwrap().blocks() * 512;
    let want = blocks(&payload) + blocks(&payload.join("a")) + blocks(&payload.join("b"));
    assert_eq!(
        size, want,
        "block usage of the tree, not the 12 payload bytes"
    );
    assert_eq!(parts[parts.len() - 1], "tree");

    // File-only put must not invent a directorysizes line for that file name.
    sb.touch("solo.txt");
    assert!(sb.run(&["put", "solo.txt"]).status.success());
    let body2 = fs::read_to_string(&cache).unwrap();
    assert!(
        !body2.split_whitespace().any(|t| t == "solo.txt"),
        "file put must not add directorysizes for files: {body2}"
    );

    let out = sb.empty(&[]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        !cache.exists(),
        "full empty must prune/remove directorysizes"
    );
}

#[test]
fn completions_stdout_matches_embedded_markers() {
    let out = common::rtrash_real(common::Real::NoTrash)
        .args(["completions", "bash"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = stdout_of(&out);
    assert!(s.contains("complete -F _rtrash_main rtrash"));
    assert!(s.contains("--home-only"));

    let out = common::rtrash_real(common::Real::NoTrash)
        .args(["completions", "zsh"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(stdout_of(&out).contains("#compdef"));

    let out = common::rtrash_real(common::Real::NoTrash)
        .args(["completions", "fish"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let fish = stdout_of(&out);
    assert!(fish.contains("complete -c rtrash"));
    assert!(fish.contains("home-only"));
    assert!(fish.contains("dry-run"));
    assert!(fish.contains("status"));
}

#[test]
fn man_subcommand_prints_page() {
    let out = common::rtrash_real(common::Real::NoTrash)
        .args(["man"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = stdout_of(&out);
    assert!(s.contains(".TH RTRASH 1"));
    assert!(s.contains("setup"));
}

#[cfg(target_os = "linux")]
#[test]
fn setup_prefix_keeps_a_name_that_is_not_utf8() {
    use std::os::unix::ffi::OsStrExt;
    let top = std::env::temp_dir().join(format!("rtrash-setup-bytes-{}", std::process::id()));
    let _ = fs::remove_dir_all(&top);
    fs::create_dir_all(&top).unwrap();
    let prefix = top.join(std::ffi::OsStr::from_bytes(b"p\xe9"));
    let mut arg = b"--prefix=".to_vec();
    arg.extend_from_slice(prefix.as_os_str().as_bytes());
    let out = common::rtrash(&common::scratch_dir("setup-home", "bytes"))
        .arg("setup")
        .arg(std::ffi::OsStr::from_bytes(&arg))
        .arg("-f")
        .output()
        .unwrap();
    assert!(out.status.success(), "setup failed: {}", stderr_of(&out));
    assert!(prefix.join("share/man/man1/rtrash.1").is_file());
    let _ = fs::remove_dir_all(&top);
}

#[test]
fn setup_installs_under_prefix() {
    let prefix = std::env::temp_dir().join(format!(
        "rtrash-setup-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&prefix);
    let prefix_s = prefix.display().to_string();
    let out = common::rtrash(&common::scratch_dir("setup-home", "prefix"))
        .args(["setup", &format!("--prefix={prefix_s}"), "-f", "-v"])
        .output()
        .unwrap();
    assert!(out.status.success(), "setup failed: {}", stderr_of(&out));
    let bash = prefix.join("share/bash-completion/completions/rtrash");
    let zsh = prefix.join("share/zsh/site-functions/_rtrash");
    let man = prefix.join("share/man/man1/rtrash.1");
    assert!(bash.is_file(), "missing {bash:?}");
    assert!(zsh.is_file(), "missing {zsh:?}");
    assert!(man.is_file(), "missing {man:?}");
    let fish = prefix.join("share/fish/vendor_completions.d/rtrash.fish");
    assert!(fish.is_file(), "missing {fish:?}");
    assert!(prefix.join("bin/trash-put").symlink_metadata().is_ok());
    assert!(fs::read_to_string(&bash).unwrap().contains("setup"));
    assert!(fs::read_to_string(&fish).unwrap().contains("dry-run"));
    // Fish only autoloads <command>.fish — multi-call names need their own files.
    for name in [
        "trash",
        "trash-put",
        "trash-empty",
        "trash-list",
        "trash-restore",
        "trash-rm",
    ] {
        let p = prefix.join(format!("share/fish/vendor_completions.d/{name}.fish"));
        assert!(
            p.symlink_metadata().is_ok() || p.is_file(),
            "setup must install fish multi-call completion {p:?}"
        );
        if p.symlink_metadata()
            .ok()
            .is_some_and(|m| m.file_type().is_symlink())
        {
            assert_eq!(
                fs::read_link(&p).unwrap().as_os_str(),
                "rtrash.fish",
                "fish multi-call link should point at shared rtrash.fish"
            );
        }
    }
    let _ = fs::remove_dir_all(&prefix);
}

#[test]
fn help_mentions_setup() {
    let out = common::rtrash_real(common::Real::NoTrash)
        .args(["--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = stdout_of(&out);
    assert!(s.contains("setup"));
    assert!(s.contains("completions"));
}

#[test]
fn status_uses_directorysizes_cache_when_valid() {
    let sb = Sandbox::new("status-dircache");
    let d = sb.work().join("cachedir");
    fs::create_dir(&d).unwrap();
    fs::write(d.join("inner"), vec![b'q'; 2048]).unwrap();
    assert!(sb.run(&["put", "-r", "cachedir"]).status.success());
    let info = sb.trash().join("info/cachedir.trashinfo");
    assert!(info.is_file());
    // Inject an unmistakable cached size with matching .trashinfo mtime.
    use std::os::unix::fs::MetadataExt;
    let mtime = fs::metadata(&info).unwrap().mtime();
    fs::write(
        sb.trash().join("directorysizes"),
        format!("777000000 {mtime} cachedir\n"),
    )
    .unwrap();
    let out = sb.run(&["status", "--home-only"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    // Only the Total line: the rows carry the sandbox path, whose process
    // id may hold any digits (a pid with 741 in it broke this test).
    let total = |s: &str| {
        s.lines()
            .find(|l| l.starts_with("Total:"))
            .unwrap_or("")
            .to_string()
    };
    let s = total(&stdout_of(&out));
    // 777000000 B ≈ 741.0 MiB via format_bytes — must appear when cache is used.
    assert_eq!(
        s, "Total: 1 item (741.0 MiB)",
        "status must use the cache line"
    );
    // Corrupt mtime → fallback walk; total should shrink below the forged cache.
    fs::write(
        sb.trash().join("directorysizes"),
        format!("777000000 {} cachedir\n", mtime.saturating_sub(12345)),
    )
    .unwrap();
    let out2 = sb.run(&["status", "--home-only"]);
    assert!(out2.status.success(), "{}", stderr_of(&out2));
    let s2 = total(&stdout_of(&out2));
    // Fallback walk of a 2 KiB file must not still report ~741 MiB.
    assert!(
        s2.starts_with("Total: 1 item (") && s2.ends_with(" KiB)"),
        "stale cache must not dominate status: {s2}"
    );
}

#[test]
fn status_writes_missing_directorysizes_back_and_drops_gone_lines() {
    let sb = Sandbox::new("status-writeback");
    let d = sb.work().join("walked");
    fs::create_dir(&d).unwrap();
    fs::write(d.join("inner"), vec![b'q'; 5000]).unwrap();
    assert!(sb.run(&["put", "-r", "walked"]).status.success());
    let cache = sb.trash().join("directorysizes");
    let first = fs::read_to_string(&cache).unwrap();
    // A trash written by another tool: no line for `walked`, and a line for
    // a directory that a third tool already removed.
    fs::write(&cache, "4096 1 long%20gone\n").unwrap();
    let out = sb.run(&["status", "--home-only"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let body = fs::read_to_string(&cache).unwrap();
    assert_eq!(
        body, first,
        "the walked size returns and the gone line leaves"
    );
}

#[test]
fn empty_dry_run_uses_directorysizes_for_dir_payload() {
    let sb = Sandbox::new("empty-dry-dircache");
    let d = sb.work().join("drytree");
    fs::create_dir(&d).unwrap();
    fs::write(d.join("blob"), vec![b'z'; 1024]).unwrap();
    assert!(sb.run(&["put", "-r", "drytree"]).status.success());
    use std::os::unix::fs::MetadataExt;
    let info = sb.trash().join("info/drytree.trashinfo");
    let mtime = fs::metadata(&info).unwrap().mtime();
    fs::write(
        sb.trash().join("directorysizes"),
        format!("555000000 {mtime} drytree\n"),
    )
    .unwrap();
    let out = sb.empty(&["--dry-run"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(err.contains("Would remove"), "{err}");
    assert!(
        err.contains("MiB") || err.contains("GiB") || err.contains("555"),
        "dry-run reclaim must use directorysizes when valid: {err}"
    );
}

#[test]
fn put_trash_dir_pin_creates_layout_and_isolates() {
    let sb = Sandbox::new("put-pin");
    // Fresh pin path under sandbox (not yet a trash root).
    let pin_root = sb.root.join("custom-trash");
    let pin = format!("--trash-dir={}", pin_root.display());
    sb.touch("pinned.txt");
    let out = sb.run(&["put", &pin, "pinned.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(pin_root.join("files/pinned.txt").is_file());
    assert!(pin_root.join("info/pinned.txt.trashinfo").is_file());
    // Home XDG trash in this sandbox must stay empty for this put.
    let home_files = sb.trash().join("files");
    let home_count = fs::read_dir(&home_files).map(|rd| rd.count()).unwrap_or(0);
    assert_eq!(home_count, 0, "home trash should be empty after pin put");
    let listed = sb.run(&["list", &pin]);
    assert!(listed.status.success(), "{}", stderr_of(&listed));
    assert!(
        stdout_of(&listed).contains("pinned.txt"),
        "{}",
        stdout_of(&listed)
    );
}

#[test]
fn empty_json_and_older_than_flag() {
    let sb = Sandbox::new("empty-json");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("e.txt");
    assert!(sb.run(&["put", "e.txt"]).status.success());
    let dry = sb.run(&["empty", &pin, "--dry-run", "--json"]);
    assert!(dry.status.success(), "{}", stderr_of(&dry));
    let s = stdout_of(&dry);
    assert!(
        s.contains("\"dry_run\":true") || s.contains("\"dry_run\": true"),
        "{s}"
    );
    assert!(
        s.contains("\"removed\":1") || s.contains("\"removed\": 1"),
        "{s}"
    );
    assert!(s.contains("\"bytes\":"), "{s}");
    // Still present after dry-run.
    assert!(sb.trash().join("files/e.txt").is_file());
    let real = sb.run(&["empty", &pin, "--json", "--older-than=0"]);
    assert!(real.status.success(), "{}", stderr_of(&real));
    let r = stdout_of(&real);
    assert!(
        r.contains("\"dry_run\":false") || r.contains("\"dry_run\": false"),
        "{r}"
    );
    assert!(r.contains("\"days\":0") || r.contains("\"days\": 0"), "{r}");
    assert!(!sb.trash().join("files/e.txt").exists());
}

#[test]
fn rm_json_and_age_filters() {
    let sb = Sandbox::new("rm-json");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("keep.c");
    sb.touch("drop.o");
    assert!(sb.run(&["put", "keep.c"]).status.success());
    assert!(sb.run(&["put", "drop.o"]).status.success());
    let j = sb.run(&["rm", &pin, "--json", "-n", "*.o"]);
    assert!(j.status.success(), "{}", stderr_of(&j));
    let s = stdout_of(&j);
    assert!(
        s.contains("\"removed\":1") || s.contains("\"removed\": 1"),
        "{s}"
    );
    assert!(s.contains("drop.o"), "{s}");
    assert!(!s.contains("keep.c"), "{s}");
    // dry-run keeps files
    assert!(sb.trash().join("files/drop.o").is_file());
    let gone = sb.run(&["rm", &pin, "--json", "*.o"]);
    assert!(gone.status.success(), "{}", stderr_of(&gone));
    assert!(!sb.trash().join("files/drop.o").exists());
    assert!(sb.trash().join("files/keep.c").is_file());
}

#[test]
fn list_last_keeps_the_tail_of_the_listing() {
    let sb = Sandbox::new("list-last");
    for n in ["a.txt", "b.txt", "c.txt"] {
        sb.touch(n);
        assert!(sb.run(&["put", n]).status.success());
    }
    backdate_trashinfo(&sb, "a.txt", "2001-01-01T00:00:00");
    backdate_trashinfo(&sb, "b.txt", "2002-01-01T00:00:00");
    let pin = format!("--trash-dir={}", sb.trash().display());
    let names = |args: &[&str]| -> Vec<String> {
        let mut all = vec!["list", pin.as_str()];
        all.extend_from_slice(args);
        let out = sb.run(&all);
        assert!(out.status.success(), "{}", stderr_of(&out));
        stdout_of(&out)
            .lines()
            .map(|l| l.rsplit('/').next().unwrap_or("").to_string())
            .collect()
    };
    assert_eq!(names(&["--last=2"]), ["b.txt", "c.txt"]);
    assert_eq!(names(&["--last=1", "--reverse"]), ["a.txt"]);
    assert_eq!(names(&["--last=9"]).len(), 3);
    assert!(names(&["--last=0"]).is_empty());
    assert_eq!(names(&["--last=1", "--sort=name"]), ["c.txt"]);
    let out = sb.run(&["list", &pin, "--last=x"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn directory_scopes_list_and_restore_to_a_gone_folder() {
    let sb = Sandbox::new("directory");
    let proj = sb.work().join("proj");
    fs::create_dir_all(&proj).unwrap();
    fs::write(proj.join("in.txt"), b"in").unwrap();
    sb.touch("out.txt");
    assert!(sb.run(&["put", "proj/in.txt", "out.txt"]).status.success());
    fs::remove_dir(&proj).unwrap();
    let pin = format!("--trash-dir={}", sb.trash().display());
    let dir = format!("--directory={}", proj.display());

    let out = sb.run(&["list", &pin, &dir]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.contains("in.txt"), "{text}");
    // A relative DIR counts from here.
    let out = sb.run(&["list", &pin, "--directory=proj"]);
    assert_eq!(stdout_of(&out).lines().count(), 1, "{}", stdout_of(&out));
    assert_eq!(
        sb.run(&["list", &pin, &dir, "--cwd-only"]).status.code(),
        Some(2)
    );

    // restore: the one item from the gone folder restores, recreating it.
    let out = sb.run(&["restore", &pin, &dir]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(proj.join("in.txt")).unwrap(), b"in");
    assert!(!sb.work().join("out.txt").exists());
    assert_eq!(
        sb.run(&["restore", &pin, "--directory="]).status.code(),
        Some(2)
    );
}

#[test]
fn list_json_and_age_filters() {
    let sb = Sandbox::new("list-json");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("new.txt");
    assert!(sb.run(&["put", "new.txt"]).status.success());
    // Write an old-looking trashinfo by putting then rewriting date.
    sb.touch("old.txt");
    assert!(sb.run(&["put", "old.txt"]).status.success());
    let info = sb.trash().join("info/old.txt.trashinfo");
    let body = fs::read_to_string(&info).unwrap();
    let body = body
        .lines()
        .map(|l| {
            if l.starts_with("DeletionDate=") {
                "DeletionDate=2001-01-01T00:00:00".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(&info, body).unwrap();

    let j = sb.run(&["list", &pin, "--json"]);
    assert!(j.status.success(), "{}", stderr_of(&j));
    let s = stdout_of(&j);
    assert!(s.trim_start().starts_with('['), "{s}");
    assert!(s.contains("\"original\""), "{s}");
    assert!(s.contains("new.txt") || s.contains("old.txt"), "{s}");

    let old_only = sb.run(&["list", &pin, "--older-than=30"]);
    assert!(old_only.status.success(), "{}", stderr_of(&old_only));
    let o = stdout_of(&old_only);
    assert!(o.contains("old.txt"), "expected old.txt in older-than: {o}");
    assert!(
        !o.contains("new.txt"),
        "new.txt should not match older-than=30: {o}"
    );

    let new_only = sb.run(&["list", &pin, "--newer-than=30"]);
    assert!(new_only.status.success(), "{}", stderr_of(&new_only));
    let n = stdout_of(&new_only);
    assert!(n.contains("new.txt"), "expected new.txt in newer-than: {n}");
    assert!(
        !n.contains("old.txt"),
        "old.txt should not match newer-than=30: {n}"
    );
}

#[test]
fn status_json_and_age_filter() {
    let sb = Sandbox::new("status-json");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("a.txt");
    assert!(sb.run(&["put", "a.txt"]).status.success());
    let out = sb.run(&["status", &pin, "--json"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let s = stdout_of(&out);
    assert!(s.contains("\"total_items\""), "{s}");
    assert!(s.contains("\"total_bytes\""), "{s}");
    assert!(s.contains("\"roots\""), "{s}");
    // recent item: older-than large window still counts; older-than=0 with future?
    // older-than=0 means cutoff=now, so items with t<=now are old (all real past items).
    let filtered = sb.run(&["status", &pin, "--json", "--newer-than=0"]);
    assert!(filtered.status.success(), "{}", stderr_of(&filtered));
    // With newer-than=0, cutoff=now, only t>now — typically 0 items for past dates.
    // Use newer-than=36500 (huge) so recent put is included.
    let recent = sb.run(&["status", &pin, "--json", "--newer-than=36500"]);
    assert!(recent.status.success(), "{}", stderr_of(&recent));
    let r = stdout_of(&recent);
    assert!(
        r.contains("\"total_items\": 1") || r.contains("\"total_items\":1"),
        "{r}"
    );
}

fn batch_of(sb: &Sandbox, name: &str) -> String {
    let body = fs::read_to_string(sb.trash().join(format!("info/{name}.trashinfo"))).unwrap();
    body.lines()
        .find_map(|l| l.strip_prefix("X-Rtrash-Batch="))
        .expect("put writes a batch key")
        .to_string()
}

#[test]
fn undo_restores_the_last_put_run_and_only_it() {
    let sb = Sandbox::new("undo");
    sb.touch("first.txt");
    assert!(sb.run(&["put", "first.txt"]).status.success());
    sb.touch("a.txt");
    fs::create_dir(sb.work().join("dir")).unwrap();
    sb.touch("dir/inner");
    assert!(sb.run(&["put", "-r", "a.txt", "dir"]).status.success());
    // An item from another tool carries no batch key and must stay put.
    fs::write(sb.trash().join("files/gio.txt"), b"g").unwrap();
    fs::write(
        sb.trash().join("info/gio.txt.trashinfo"),
        format!(
            "[Trash Info]\nPath={}\nDeletionDate=2099-01-01T00:00:00\n",
            sb.work().join("gio.txt").display()
        ),
    )
    .unwrap();
    let run2 = batch_of(&sb, "a.txt");
    assert_eq!(batch_of(&sb, "dir"), run2, "one run, one id");
    assert_ne!(batch_of(&sb, "first.txt"), run2);

    let listed = stdout_of(&sb.run(&["undo", "--list", "--home-only"]));
    let first_line = listed.lines().next().unwrap_or_default();
    assert!(first_line.starts_with(&run2), "{listed}");
    assert!(first_line.contains("2 items"), "{listed}");
    assert_eq!(listed.lines().count(), 2, "gio item is no run: {listed}");

    let json = stdout_of(&sb.run(&["list", "--json", "--home-only"]));
    assert!(json.contains(&format!("\"batch\":\"{run2}\"")), "{json}");
    assert!(json.contains("\"batch\":null"), "{json}");

    let dry = sb.run(&["undo", "-n", "--home-only"]);
    assert!(dry.status.success(), "{}", stderr_of(&dry));
    assert!(!sb.work().join("a.txt").exists(), "dry run changes nothing");

    let out = sb.run(&["undo", "--home-only", "--json"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let s = stdout_of(&out);
    assert!(s.starts_with(&format!("{{\"batch\":\"{run2}\"")), "{s}");
    assert!(s.contains("\"failed\":0"), "{s}");
    assert!(sb.work().join("a.txt").is_file());
    assert!(sb.work().join("dir/inner").is_file());
    assert!(!sb.work().join("first.txt").exists());
    assert!(sb.trash().join("files/gio.txt").exists());

    // The next undo takes the earlier run, by id here.
    let run1 = batch_of(&sb, "first.txt");
    let out = sb.run(&["undo", "--home-only", &run1]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(sb.work().join("first.txt").is_file());

    // Nothing rtrash trashed is left.
    let out = sb.run(&["undo", "--home-only"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).contains("no put run"),
        "{}",
        stderr_of(&out)
    );
}

/// `undo` appends one `undo` line to the history of the trash its items
/// came back from, as restore and the Finder undo do. A dry run, an
/// unknown run id, a run the scope refuses, a run whose every path is
/// taken and an undo with no run left add none.
#[test]
fn undo_appends_one_undo_line_to_the_trash_history() {
    let sb = Sandbox::new("undo-history");
    sb.touch("uh-a.txt");
    sb.touch("uh-b.txt");
    assert!(sb.run(&["put", "uh-a.txt", "uh-b.txt"]).status.success());
    let hist = sb.trash().join("rtrash-history");
    let lines = || -> Vec<String> {
        let body = fs::read_to_string(&hist).unwrap_or_default();
        body.lines().map(String::from).collect()
    };
    let put = lines();
    assert_eq!(put.len(), 1, "{put:?}");
    assert!(
        put[0].contains(" put ") && put[0].ends_with(" 2"),
        "{put:?}"
    );
    let (info, files) = (sb.trash().join("info"), sb.trash().join("files"));
    let watch = [hist.as_path(), info.as_path(), files.as_path()];
    let elsewhere = sb.root.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let scope = elsewhere.display().to_string();
    let undo = ["undo", "--home-only"];
    let dry = ["undo", "--home-only", "-n"];
    let unknown = ["undo", "--home-only", "no-such-run"];
    common::assert_refused_unchanged(&dry, &watch, 0, &[], || sb.run(&dry));
    common::assert_refused_unchanged(&unknown, &watch, 1, &[], || sb.run(&unknown));
    common::assert_refused_unchanged(&"scope", &watch, 2, &[], || {
        sb.run_env(&undo, &[("RTRASH_ALLOW_ROOTS", scope.as_str())])
    });
    // Both paths taken again: nothing comes back, so nothing is logged.
    fs::write(sb.work().join("uh-a.txt"), b"new").unwrap();
    fs::write(sb.work().join("uh-b.txt"), b"new").unwrap();
    common::assert_refused_unchanged(&"taken", &watch, 1, &[], || sb.run(&undo));
    fs::remove_file(sb.work().join("uh-a.txt")).unwrap();
    fs::remove_file(sb.work().join("uh-b.txt")).unwrap();

    let out = sb.run(&undo);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    assert!(sb.work().join("uh-a.txt").is_file() && sb.work().join("uh-b.txt").is_file());
    let got = lines();
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[0], put[0]);
    let fields: Vec<&str> = got[1].split(' ').collect();
    assert_eq!(fields.len(), 4, "{got:?}");
    assert_eq!((fields[1], fields[3]), ("undo", "0"), "{got:?}");
    assert!(fields[0].parse::<i64>().is_ok() && fields[2].parse::<u64>().is_ok());
    // No run is left, so a second undo logs nothing either.
    common::assert_refused_unchanged(&"again", &watch, 1, &[], || sb.run(&undo));
}

#[test]
fn undo_keeps_a_recreated_path_without_force() {
    let sb = Sandbox::new("undo-conflict");
    sb.touch("x.txt");
    sb.touch("y.txt");
    assert!(sb.run(&["put", "x.txt", "y.txt"]).status.success());
    fs::write(sb.work().join("x.txt"), b"new").unwrap();
    let out = sb.run(&["undo", "--home-only"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).contains("1 restored, 1 failed"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(fs::read(sb.work().join("x.txt")).unwrap(), b"new");
    assert!(sb.work().join("y.txt").is_file());
    // With -f the rest of the run comes back over the new file.
    let out = sb.run(&["undo", "--home-only", "-f"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(sb.work().join("x.txt")).unwrap(), b"payload");
}

#[test]
fn undo_conflict_skips_or_renames_taken_paths() {
    let sb = Sandbox::new("undo-policy");
    sb.touch("x.txt");
    sb.touch("y.txt");
    assert!(sb.run(&["put", "x.txt", "y.txt"]).status.success());
    fs::write(sb.work().join("x.txt"), b"new").unwrap();

    let bad = sb.run(&["undo", "--home-only", "--conflict=bogus"]);
    assert_eq!(bad.status.code(), Some(2));

    let dry = sb.run(&["undo", "--home-only", "-n", "--conflict=rename"]);
    assert!(dry.status.success(), "{}", stderr_of(&dry));
    let said = stdout_of(&dry);
    assert!(said.contains("x.txt' as '"), "{said}");
    assert!(said.contains("x.1.txt'"), "{said}");
    assert!(
        !sb.work().join("x.1.txt").exists(),
        "dry run changes nothing"
    );
    let dry = sb.run(&["undo", "--home-only", "-n", "--conflict=skip"]);
    assert!(
        stdout_of(&dry).contains("would skip"),
        "{}",
        stdout_of(&dry)
    );

    // Skip brings back the free path and leaves the taken one in the trash.
    let out = sb.run(&["undo", "--home-only", "--conflict=skip", "--json"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let s = stdout_of(&out);
    assert!(s.contains("\"failed\":0,\"skipped\":1"), "{s}");
    assert!(sb.work().join("y.txt").is_file());
    assert_eq!(fs::read(sb.work().join("x.txt")).unwrap(), b"new");

    // Rename restores the rest of the run next to the new file.
    let out = sb.run(&["undo", "--home-only", "--conflict=rename", "--json"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let s = stdout_of(&out);
    assert!(s.contains("\"restored_to\":\""), "{s}");
    assert!(s.contains("x.1.txt\""), "{s}");
    assert_eq!(fs::read(sb.work().join("x.txt")).unwrap(), b"new");
    assert_eq!(fs::read(sb.work().join("x.1.txt")).unwrap(), b"payload");
}

/// Trash `name` (`len` bytes) dated `days` before 2020-01-28.
fn trash_aged(sb: &Sandbox, name: &str, len: usize, days: u32) {
    fs::write(sb.work().join(name), vec![b'x'; len]).unwrap();
    assert!(sb.run(&["put", name]).status.success());
    let info = sb.trash().join(format!("info/{name}.trashinfo"));
    let body = fs::read_to_string(&info).unwrap();
    assert!(days < 28);
    let day = 28 - days;
    let dated: String = body
        .lines()
        .map(|l| {
            if l.starts_with("DeletionDate=") {
                format!("DeletionDate=2020-01-{day:02}T00:00:00\n")
            } else {
                format!("{l}\n")
            }
        })
        .collect();
    fs::write(&info, dated).unwrap();
}

fn names_in_trash(sb: &Sandbox) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(sb.trash().join("files"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn empty_max_size_removes_oldest_or_largest_until_it_fits() {
    let sb = Sandbox::new("maxsize");
    trash_aged(&sb, "old", 40_000, 20);
    trash_aged(&sb, "mid", 40_000, 10);
    trash_aged(&sb, "new", 40_000, 1);
    // Three items of about 44 KiB each (blocks plus the .trashinfo).
    let dry = sb.empty(&["--max-size=100K", "-n", "--json"]);
    assert!(dry.status.success(), "{}", stderr_of(&dry));
    let s = stdout_of(&dry);
    assert!(
        s.contains("\"removed\":1") && s.contains("\"max_size\":102400"),
        "{s}"
    );
    assert_eq!(names_in_trash(&sb).len(), 3, "dry run removes nothing");

    let out = sb.empty(&["--max-size=100K"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(names_in_trash(&sb), ["mid", "new"]);
    assert!(!sb.trash().join("info/old.trashinfo").exists());

    trash_aged(&sb, "huge", 200_000, 25);
    let out = sb.empty(&["--max-size=100K", "--prune=largest"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(names_in_trash(&sb), ["mid", "new"], "largest goes first");

    // A budget the trash already fits removes nothing, and never all.
    let out = sb.empty(&["--max-size=1G"]);
    assert!(out.status.success());
    assert_eq!(names_in_trash(&sb).len(), 2);
    let bad = sb.empty(&["--max-size=lots"]);
    assert_eq!(bad.status.code(), Some(2));
}

#[test]
fn put_prunes_to_rtrash_max_size_but_keeps_its_own_items() {
    let sb = Sandbox::new("putprune");
    trash_aged(&sb, "old1", 40_000, 20);
    trash_aged(&sb, "old2", 40_000, 10);
    fs::write(sb.work().join("fresh"), vec![b'y'; 150_000]).unwrap();
    let out = common::rtrash(&sb.root)
        .args(["put", "fresh"])
        .current_dir(sb.work())
        .env("RTRASH_MAX_SIZE", "100K")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    // The fresh item alone exceeds the budget; everything older goes, it stays.
    assert_eq!(names_in_trash(&sb), ["fresh"]);
    assert!(
        stderr_of(&out).contains("removed 2 items"),
        "{}",
        stderr_of(&out)
    );
}

#[cfg(unix)]
#[test]
fn finder_reads_its_records_from_a_small_regular_file_only() {
    let sb = Sandbox::new("finder-ds-store");
    let trash = sb.root.join(".Trash");
    fs::create_dir(&trash).unwrap();
    fs::write(trash.join("item"), b"x").unwrap();
    let ds = trash.join(".DS_Store");
    // Nothing writes to the named pipe, so reading it waits forever. A
    // link to /dev/zero never ends: the command must finish, and where the
    // platform can measure it the children stay small.
    assert!(Command::new("mkfifo").arg(&ds).status().unwrap().success());
    for zero in [false, true] {
        if zero {
            fs::remove_file(&ds).unwrap();
            std::os::unix::fs::symlink("/dev/zero", &ds).unwrap();
        }
        let out = sb.run_within(20, &["finder", "list"]);
        assert!(out.status.success(), "{}", stderr_of(&out));
        assert!(stdout_of(&out).contains("item"), "{}", stdout_of(&out));
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    assert!(peak_child_rss_mib() < 512, "{} MiB", peak_child_rss_mib());
}

#[cfg(unix)]
#[test]
fn finder_list_and_restore_use_put_back_records() {
    use rtrash::ds_store::{write, Record, Value};
    let sb = Sandbox::new("finder");
    let trash = sb.root.join(".Trash");
    fs::create_dir(&trash).unwrap();
    // Finder renames a clashing item inside the trash and records the
    // original folder (volume-relative) and name.
    let folder = sb.work().strip_prefix("/").unwrap().display().to_string() + "/";
    let rec = |name: &str, code: &[u8; 4], s: &str| Record {
        name: name.into(),
        code: *code,
        value: Value::Ustr(s.into()),
    };
    fs::write(trash.join("note 12.00.01.txt"), b"one").unwrap();
    fs::create_dir(trash.join("dir")).unwrap();
    fs::write(trash.join("dir/inner"), b"in").unwrap();
    fs::write(trash.join("orphan"), b"?").unwrap();
    fs::write(
        trash.join(".DS_Store"),
        write(&[
            rec("note 12.00.01.txt", b"ptbL", &folder),
            rec("note 12.00.01.txt", b"ptbN", "note.txt"),
            rec("dir", b"ptbL", &format!("{folder}sub/")),
        ]),
    )
    .unwrap();

    // The default trash is ~/.Trash.
    let out = sb.run(&["finder", "list"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let s = stdout_of(&out);
    let note = sb.work().join("note.txt");
    assert!(
        s.contains(&format!("note 12.00.01.txt -> {}", note.display())),
        "{s}"
    );
    assert!(s.contains("orphan -> ?"), "{s}");
    assert!(!s.contains(".DS_Store"), "{s}");

    let json = stdout_of(&sb.run(&["finder", "list", "--json"]));
    assert!(
        json.contains("{\"name\":\"orphan\",\"original\":null}"),
        "{json}"
    );

    // Restore by original path; the clash policy applies as in restore.
    fs::write(&note, b"live").unwrap();
    let out = sb.run(&["finder", "restore", &note.display().to_string()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr_of(&out).contains("refusing to overwrite"));
    let out = sb.run(&[
        "finder",
        "restore",
        "--conflict=rename",
        "--json",
        &note.display().to_string(),
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(sb.work().join("note.1.txt")).unwrap(), b"one");
    assert_eq!(fs::read(&note).unwrap(), b"live");

    // --all restores what has a record and leaves the orphan.
    let out = sb.run(&["finder", "restore", "--all"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(sb.work().join("sub/dir/inner")).unwrap(), b"in");
    assert!(trash.join("orphan").exists());
    let out = sb.run(&["finder", "restore", "orphan"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr_of(&out).contains("no Put Back record"));

    #[cfg(not(target_os = "macos"))]
    {
        let out = sb.run(&["finder", "put", "x"]);
        assert_eq!(out.status.code(), Some(1));
        assert!(stderr_of(&out).contains("macOS only"));
    }
    assert_eq!(sb.run(&["finder"]).status.code(), Some(2));
}

/// Write `body` as the sandbox's config file.
fn write_config(sb: &Sandbox, body: &str) {
    let dir = sb.root.join(".config/rtrash");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config.toml"), body).unwrap();
}

#[test]
fn config_file_sets_list_defaults_policy_and_protected_paths() {
    let sb = Sandbox::new("config");
    trash_aged(&sb, "zz", 40_000, 20);
    trash_aged(&sb, "aa", 40_000, 10);
    write_config(
        &sb,
        &format!(
            "[list]\nsort = \"name\"\n\n[policy]\nmax_size = \"100K\"\n\n[protect]\npaths = [\"{}/keep*\"]\n",
            sb.work().display()
        ),
    );
    // list: name order from the config; an option still wins.
    let text = stdout_of(&sb.run(&["list"]));
    assert!(
        text.find("/aa").unwrap() < text.find("/zz").unwrap(),
        "{text}"
    );
    let text = stdout_of(&sb.run(&["list", "--sort=date"]));
    assert!(
        text.find("/zz").unwrap() < text.find("/aa").unwrap(),
        "{text}"
    );

    // protect.paths refuses a match and lets others go.
    fs::create_dir(sb.work().join("keepme")).unwrap();
    let out = sb.run(&["put", "-r", "keepme"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).contains("config protect.paths entry"),
        "{}",
        stderr_of(&out)
    );
    assert!(sb.work().join("keepme").exists());

    // policy.max_size prunes after put, naming its source.
    fs::write(sb.work().join("fresh"), vec![b'y'; 150_000]).unwrap();
    let out = sb.run(&["put", "fresh"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(names_in_trash(&sb), ["fresh"]);
    assert!(
        stderr_of(&out).contains("policy.max_size in the config file"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn environment_beats_the_config_file() {
    let sb = Sandbox::new("config-env");
    trash_aged(&sb, "old1", 40_000, 20);
    write_config(&sb, "[policy]\nmax_size = \"10K\"\n");
    fs::write(sb.work().join("fresh"), vec![b'y'; 50_000]).unwrap();
    // An empty RTRASH_MAX_SIZE turns the budget off.
    let out = common::rtrash(&sb.root)
        .args(["put", "fresh"])
        .current_dir(sb.work())
        .env("RTRASH_MAX_SIZE", "")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(names_in_trash(&sb), ["fresh", "old1"]);

    // RTRASH_CONFIG names another file; empty turns the file off.
    let other = sb.root.join("other.toml");
    fs::write(&other, "[list]\nsort = \"bogus\"\n").unwrap();
    let run = |cfg: &Path, args: &[&str]| {
        common::rtrash(&sb.root)
            .args(args)
            .current_dir(sb.work())
            .env("RTRASH_CONFIG", cfg)
            .output()
            .unwrap()
    };
    let out = run(&other, &["list"]);
    assert!(out.status.success());
    assert!(
        stderr_of(&out).contains("ignoring list.sort = 'bogus'"),
        "{}",
        stderr_of(&out)
    );
    let out = run(&other, &["config"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a bad value parses; only lines that cannot be read fail"
    );
    assert!(
        stdout_of(&out).contains("sort = \"bogus\""),
        "{}",
        stdout_of(&out)
    );
    assert_eq!(
        stdout_of(&run(&other, &["config", "--path"])).trim(),
        other.display().to_string()
    );
    fs::write(&other, "[list]\ncolour = 1\n").unwrap();
    let out = run(&other, &["config"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).contains("unknown key 'list.colour'"),
        "{}",
        stderr_of(&out)
    );
    let out = run(Path::new(""), &["config", "--path"]);
    assert!(stdout_of(&out).contains("off"), "{}", stdout_of(&out));
    let out = run(Path::new(""), &["config", "--default"]);
    assert!(
        stdout_of(&out).contains("# max_size = \"50G\""),
        "{}",
        stdout_of(&out)
    );
    assert_eq!(
        run(Path::new(""), &["config", "--bogus"]).status.code(),
        Some(2)
    );
}

/// Linux only: macOS file systems refuse names that are not UTF-8.
#[cfg(target_os = "linux")]
fn run_os(sb: &Sandbox, args: &[&std::ffi::OsStr]) -> Output {
    common::rtrash(&sb.root)
        .args(args)
        .current_dir(sb.work())
        .output()
        .expect("failed to run rtrash")
}

#[cfg(target_os = "linux")]
#[test]
fn names_that_are_not_utf8_put_list_rm_restore_and_empty() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let sb = Sandbox::new("nonutf8");
    let a = OsStr::from_bytes(b"name\xe9");
    let b = OsStr::from_bytes(b"name\xe8");
    let d = OsStr::from_bytes(b"d\xff");
    fs::write(sb.work().join(a), "a").unwrap();
    fs::write(sb.work().join(b), "bb").unwrap();
    fs::create_dir(sb.work().join(d)).unwrap();
    fs::write(sb.work().join(d).join("inner"), "z").unwrap();

    // Used to panic in argument parsing. Every later step names the home
    // trash, so items in a volume trash such as /dev/shm/.Trash-UID stay out.
    let put = run_os(&sb, &[OsStr::new("put"), OsStr::new("-r"), a, b, d]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    // Each name keeps its own bytes in the trash: no folding into U+FFFD.
    for n in [a, b, d] {
        assert!(sb.trash().join("files").join(n).exists(), "{n:?}");
        let mut info = n.to_os_string();
        info.push(".trashinfo");
        assert!(sb.trash().join("info").join(info).exists(), "{n:?}");
    }
    let sizes = fs::read_to_string(sb.trash().join("directorysizes")).unwrap();
    assert!(sizes.trim_end().ends_with(" d%FF"), "{sizes}");
    let list = stdout_of(&sb.run(&["list", "--home-only", "--json"]));
    assert_eq!(list.matches("\"name\":").count(), 3, "{list}");
    assert!(!list.contains('\u{10FFFF}'), "{list}");

    // rm by the raw name removes that item only.
    let rm = run_os(&sb, &[OsStr::new("rm"), OsStr::new("--home-only"), b]);
    assert!(rm.status.success(), "{}", stderr_of(&rm));
    assert!(!sb.trash().join("files").join(b).exists());
    assert!(sb.trash().join("files").join(a).exists());

    let restore = run_os(&sb, &[OsStr::new("restore"), OsStr::new("--home-only"), a]);
    assert!(restore.status.success(), "{}", stderr_of(&restore));
    assert_eq!(fs::read(sb.work().join(a)).unwrap(), b"a");

    // A days filter leaves recent items; a full empty takes the pair.
    let keep = sb.run(&["empty", "--home-only", "-f", "30"]);
    assert!(keep.status.success(), "{}", stderr_of(&keep));
    assert!(sb.trash().join("files").join(d).exists());
    let all = sb.run(&["empty", "--home-only", "-f"]);
    assert!(all.status.success(), "{}", stderr_of(&all));
    assert_eq!(fs::read_dir(sb.trash().join("files")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(sb.trash().join("info")).unwrap().count(), 0);
}

#[cfg(target_os = "linux")]
#[test]
fn trashinfo_with_raw_bytes_lists_restores_and_ages_out() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let sb = Sandbox::new("rawinfo");
    let files = sb.trash().join("files");
    let info = sb.trash().join("info");
    fs::create_dir_all(&files).unwrap();
    fs::create_dir_all(&info).unwrap();
    // Another tool wrote the path bytes unescaped; read_to_string dropped it.
    let mut body = b"[Trash Info]\nPath=".to_vec();
    body.extend_from_slice(sb.work().as_os_str().as_bytes());
    body.extend_from_slice(b"/raw\xfe\nDeletionDate=2020-01-01T00:00:00\n");
    fs::write(info.join("raw.trashinfo"), &body).unwrap();
    fs::write(files.join("raw"), "r").unwrap();

    let list = stdout_of(&sb.run(&["list", "--home-only"]));
    assert!(list.contains("/raw\u{fffd}"), "{list}");
    let name = OsStr::from_bytes(b"raw\xfe");
    let restore = run_os(
        &sb,
        &[OsStr::new("restore"), OsStr::new("--home-only"), name],
    );
    assert!(restore.status.success(), "{}", stderr_of(&restore));
    assert_eq!(fs::read(sb.work().join(name)).unwrap(), b"r");

    // Same file again, now aged out by `empty DAYS`.
    fs::write(info.join("raw.trashinfo"), &body).unwrap();
    fs::write(files.join("raw"), "r").unwrap();
    let empty = sb.run(&["empty", "--home-only", "-f", "1"]);
    assert!(empty.status.success(), "{}", stderr_of(&empty));
    assert!(!files.join("raw").exists());
    assert!(!info.join("raw.trashinfo").exists());
}

#[test]
fn missing_payload_lists_refuses_restore_and_empties() {
    let sb = Sandbox::new("ghost");
    let info = sb.trash().join("info");
    fs::create_dir_all(sb.trash().join("files")).unwrap();
    fs::create_dir_all(&info).unwrap();
    let body = format!(
        "[Trash Info]\nPath={}/ghost\nDeletionDate=2020-01-01T00:00:00\n",
        sb.work().display()
    );
    fs::write(info.join("ghost.trashinfo"), body).unwrap();

    assert!(stdout_of(&sb.run(&["list"])).contains("/ghost"));
    let restore = sb.run(&["restore", "ghost"]);
    assert_eq!(restore.status.code(), Some(1));
    assert!(
        stderr_of(&restore).contains("trash payload missing for 'ghost'"),
        "{}",
        stderr_of(&restore)
    );
    assert!(!sb.work().join("ghost").exists());
    let status = sb.run(&["status"]);
    assert!(status.status.success(), "{}", stderr_of(&status));
    let empty = sb.run(&["empty", "--home-only", "-f", "1"]);
    assert!(empty.status.success(), "{}", stderr_of(&empty));
    assert!(!info.join("ghost.trashinfo").exists());
}

#[cfg(unix)]
#[test]
fn list_all_users_adds_the_owner_and_stays_read_only() {
    let sb = Sandbox::new("allusers");
    fs::write(sb.work().join("mine.txt"), "x").unwrap();
    let put = sb.run(&["put", "mine.txt"]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let info = sb.trash().join("info/mine.txt.trashinfo");
    let before = fs::read(&info).unwrap();

    let me = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout).unwrap();
    let me = me.trim();
    let text = stdout_of(&sb.run(&["list", "--all-users"]));
    let row = text
        .lines()
        .find(|l| l.ends_with("/mine.txt"))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(row.contains(&format!(" {me} ")), "{row}");
    let json = stdout_of(&sb.run(&["list", "--all-users", "--json"]));
    let obj = json
        .lines()
        .find(|l| l.contains("/mine.txt\""))
        .unwrap_or_else(|| panic!("{json}"));
    assert!(obj.contains(&format!("\"owner\":\"{me}\"")), "{obj}");
    // Plain list output keeps its shape.
    assert!(!stdout_of(&sb.run(&["list", "--json"])).contains("\"owner\""));
    assert_eq!(fs::read(&info).unwrap(), before);

    for bad in [
        &["list", "--all-users", "--home-only"][..],
        &["list", "--all-users", "--trash-dir=/x"],
    ] {
        assert_eq!(sb.run(bad).status.code(), Some(2), "{bad:?}");
    }
}

/// A `.trashinfo` written by someone else can put escape sequences and
/// newlines in `Path=`. A terminal gets them as `?`; a pipe gets them raw.
#[cfg(target_os = "linux")]
#[test]
fn list_scrubs_control_characters_on_a_terminal() {
    if !Path::new("/usr/bin/script").exists() {
        eprintln!("skipping: no script(1) to give rtrash a terminal");
        return;
    }
    let sb = Sandbox::new("scrub");
    fs::create_dir_all(sb.trash().join("files")).unwrap();
    fs::create_dir_all(sb.trash().join("info")).unwrap();
    fs::write(sb.trash().join("files/evil"), b"x").unwrap();
    fs::write(
        sb.trash().join("info/evil.trashinfo"),
        "[Trash Info]\nPath=/tmp/a%1B%5B2Jb%0Afake%7F\nDeletionDate=2024-01-02T03:04:05\n",
    )
    .unwrap();

    let piped = sb.run(&["list"]);
    assert!(piped.status.success(), "{}", stderr_of(&piped));
    assert!(stdout_of(&piped).contains("/tmp/a\x1b[2Jb\nfake\x7f"));

    let cmd = format!("{} list", bin());
    let tty = Command::new("/usr/bin/script")
        .args(["-qec", &cmd, "/dev/null"])
        .current_dir(sb.work())
        .env("XDG_DATA_HOME", sb.root.join("xdg"))
        .env("XDG_CONFIG_HOME", sb.root.join(".config"))
        .env("HOME", &sb.root)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run script");
    let shown = stdout_of(&tty);
    assert!(shown.contains("/tmp/a?[2Jb?fake?"), "{shown:?}");
    assert!(
        !shown.contains('\x1b') && !shown.contains('\x7f'),
        "{shown:?}"
    );
}

/// A writable directory on another file system than `near`, if the
/// machine has one (`/dev/shm` on most Linux systems).
fn other_device_dir(near: &Path, tag: &str) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let dev = fs::metadata(near).ok()?.dev();
    for base in ["/dev/shm", "/run/user"] {
        let base = Path::new(base);
        let Ok(m) = fs::metadata(base) else { continue };
        if m.dev() == dev {
            continue;
        }
        let dir = base.join(format!("rtrash-space-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        if fs::create_dir_all(dir.join("files")).is_ok()
            && fs::create_dir_all(dir.join("info")).is_ok()
        {
            return Some(dir);
        }
    }
    None
}

/// An xattr of `p`, or `None` when it has none or the call fails.
#[cfg(target_os = "linux")]
fn xattr(p: &Path, name: &str) -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
    let name = std::ffi::CString::new(name).unwrap();
    let mut buf = vec![0u8; 4096];
    let n = unsafe {
        libc::lgetxattr(
            path.as_ptr(),
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    (n >= 0).then(|| {
        buf.truncate(n as usize);
        buf
    })
}

/// A POSIX access ACL (the `system.posix_acl_access` format) granting
/// group 4242 `perm`, with the owner, group and mask at `perm` too.
#[cfg(target_os = "linux")]
fn acl_bytes(perm: u16) -> Vec<u8> {
    let mut b = 2u32.to_le_bytes().to_vec();
    for (tag, id) in [
        (1u16, u32::MAX),
        (2, 4242),
        (4, u32::MAX),
        (0x10, u32::MAX),
        (0x20, u32::MAX),
    ] {
        b.extend_from_slice(&tag.to_le_bytes());
        b.extend_from_slice(&(if tag == 0x20 { 0 } else { perm }).to_le_bytes());
        b.extend_from_slice(&id.to_le_bytes());
    }
    b
}

/// A put to a trash on another file system copies the payload; the copy,
/// and the copy back on restore, keep the POSIX ACLs of a file and of a
/// directory (access and default ACLs).
#[cfg(target_os = "linux")]
#[test]
fn copies_across_devices_keep_posix_acls() {
    use std::os::unix::ffi::OsStrExt;
    let sb = Sandbox::new("acl");
    let Some(other) = other_device_dir(&sb.root, "acl") else {
        return;
    };
    let set = |p: &Path, name: &str, v: &[u8]| -> bool {
        let path = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
        let name = std::ffi::CString::new(name).unwrap();
        unsafe { libc::setxattr(path.as_ptr(), name.as_ptr(), v.as_ptr().cast(), v.len(), 0) == 0 }
    };
    let file = sb.work().join("f.txt");
    let dir = sb.work().join("d");
    fs::write(&file, "x").unwrap();
    fs::create_dir_all(dir.join("sub")).unwrap();
    let (rw, rwx) = (acl_bytes(6), acl_bytes(7));
    if !set(&file, "system.posix_acl_access", &rw) {
        eprintln!("no POSIX ACLs on this file system; skipping");
        let _ = fs::remove_dir_all(&other);
        return;
    }
    for p in [dir.clone(), dir.join("sub")] {
        assert!(set(&p, "system.posix_acl_access", &rwx));
        assert!(set(&p, "system.posix_acl_default", &rwx));
    }
    let check = |f: &Path, d: &Path, when: &str| {
        assert_eq!(
            xattr(f, "system.posix_acl_access"),
            Some(rw.clone()),
            "{when}: file"
        );
        for p in [d.to_path_buf(), d.join("sub")] {
            for n in ["system.posix_acl_access", "system.posix_acl_default"] {
                assert_eq!(
                    xattr(&p, n),
                    Some(rwx.clone()),
                    "{when}: {} {n}",
                    p.display()
                );
            }
        }
    };
    let pin = format!("--trash-dir={}", other.display());
    let run = |args: &[&str]| {
        common::rtrash(&sb.root)
            .args(args)
            .current_dir(sb.work())
            .output()
            .unwrap()
    };
    let out = run(&["put", "-r", "--no-space-check", &pin, "f.txt", "d"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    check(
        &other.join("files/f.txt"),
        &other.join("files/d"),
        "trashed",
    );
    for p in [&file, &dir] {
        let out = run(&[
            "restore",
            "--plain",
            "--no-space-check",
            &pin,
            p.to_str().unwrap(),
        ]);
        assert!(out.status.success(), "{}", stderr_of(&out));
    }
    check(&file, &dir, "restored");
    let _ = fs::remove_dir_all(&other);
}

/// macOS keeps access control lists outside the extended attributes. A put
/// to a trash on another volume, and the restore back, keep the list of a
/// file and of a directory. CI mounts a disk image and names it in
/// `RTRASH_TEST_OTHER_VOLUME`; without one the test skips.
#[cfg(target_os = "macos")]
#[test]
fn copies_across_volumes_keep_macos_acls() {
    let Some(volume) = std::env::var_os("RTRASH_TEST_OTHER_VOLUME") else {
        eprintln!("RTRASH_TEST_OTHER_VOLUME unset; skipping");
        return;
    };
    let sb = Sandbox::new("mac-acl");
    let other = Path::new(&volume).join(format!("rtrash-acl-{}", std::process::id()));
    fs::create_dir_all(other.join("files")).unwrap();
    fs::create_dir_all(other.join("info")).unwrap();
    let file = sb.work().join("f.txt");
    let dir = sb.work().join("d");
    fs::write(&file, "x").unwrap();
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("inner"), "y").unwrap();
    const ACE: &str = "user:nobody allow read";
    for p in [&file, &dir, &dir.join("inner")] {
        let ok = Command::new("chmod")
            .args(["+a", ACE])
            .arg(p)
            .status()
            .unwrap();
        assert!(ok.success(), "chmod +a {}", p.display());
    }
    let acl = |p: &Path| -> String {
        let out = Command::new("ls").arg("-led").arg(p).output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    // The entries without the first line of `ls -le`; macOS names the
    // read right `list` on a directory, so compare with the source.
    let entries = |p: &Path| -> Vec<String> {
        acl(p)
            .lines()
            .skip(1)
            .map(|l| l.trim().to_string())
            .collect()
    };
    let names = ["f.txt", "d", "d/inner"];
    let before: Vec<(String, Vec<String>)> = names
        .iter()
        .map(|n| {
            let p = sb.work().join(n);
            let e = entries(&p);
            assert!(
                e.iter().any(|l| l.contains("user:nobody allow")),
                "before: {}",
                acl(&p)
            );
            (acl(&p), e)
        })
        .collect();
    let check = |base: &Path, when: &str| {
        for (n, (shown_before, want)) in names.iter().zip(&before) {
            let p = base.join(n);
            assert_eq!(
                &entries(&p),
                want,
                "{when}: {}\nbefore: {shown_before}",
                acl(&p)
            );
        }
    };
    let pin = format!("--trash-dir={}", other.display());
    let run = |args: &[&str]| {
        common::rtrash(&sb.root)
            .args(args)
            .current_dir(sb.work())
            .output()
            .unwrap()
    };
    let out = run(&["put", "-r", "--no-space-check", &pin, "f.txt", "d"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!file.exists() && !dir.exists());
    check(&other.join("files"), "trashed");
    for p in [&file, &dir] {
        let out = run(&[
            "restore",
            "--plain",
            "--no-space-check",
            &pin,
            p.to_str().unwrap(),
        ]);
        assert!(out.status.success(), "{}", stderr_of(&out));
    }
    check(&sb.work(), "restored");
    let _ = fs::remove_dir_all(&other);
}

/// `setup --powershell-profile=PATH` adds the completer block once and
/// keeps what the profile had.
#[test]
fn setup_adds_the_powershell_completer_to_a_profile_once() {
    let sb = Sandbox::new("ps-profile");
    let profile = sb.root.join("ps").join("profile.ps1");
    fs::create_dir_all(profile.parent().unwrap()).unwrap();
    fs::write(&profile, "Set-Alias ll Get-ChildItem\n").unwrap();
    let prefix = format!("--prefix={}", sb.root.join("prefix").display());
    let opt = format!("--powershell-profile={}", profile.display());
    let out = sb.run(&["setup", &prefix, &opt]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("added the rtrash completer"),
        "{}",
        stdout_of(&out)
    );
    let out = sb.run(&["setup", &prefix, "-f", &opt]);
    assert!(
        stdout_of(&out).contains("loads the rtrash completer already"),
        "{}",
        stdout_of(&out)
    );
    let text = fs::read_to_string(&profile).unwrap();
    assert!(text.starts_with("Set-Alias ll Get-ChildItem\n"), "{text}");
    assert_eq!(
        text.matches("rtrash completions powershell").count(),
        1,
        "{text}"
    );
}

#[test]
fn put_across_devices_keeps_the_target_from_filling() {
    let sb = Sandbox::new("put-space");
    let Some(other) = other_device_dir(&sb.root, "put") else {
        return;
    };
    fs::write(sb.work().join("big"), vec![b'x'; 4096]).unwrap();
    let pin = format!("--trash-dir={}", other.display());
    let out = common::rtrash(&sb.root)
        .args(["put", &pin, "big"])
        .current_dir(sb.work())
        .env("RTRASH_MIN_FREE", "1000000T")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("not enough room"),
        "{}",
        stderr_of(&out)
    );
    assert!(sb.work().join("big").exists());
    assert_eq!(fs::read_dir(other.join("files")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(other.join("info")).unwrap().count(), 0);

    let out = common::rtrash(&sb.root)
        .args(["put", "--no-space-check", &pin, "big"])
        .current_dir(sb.work())
        .env("RTRASH_MIN_FREE", "1000000T")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!sb.work().join("big").exists());
    assert!(other.join("files/big").exists());
    let _ = fs::remove_dir_all(&other);
}

#[test]
fn status_gives_free_bytes_and_inodes_of_each_trash() {
    let sb = Sandbox::new("status-free");
    fs::write(sb.work().join("f"), b"x").unwrap();
    assert!(sb.run(&["put", "f"]).status.success());
    let out = sb.run(&["status", "--home-only"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let s = stdout_of(&out);
    let line = s
        .lines()
        .find(|l| l.starts_with("Free on "))
        .unwrap_or_else(|| panic!("{s}"));
    assert!(line.contains(&*sb.trash().to_string_lossy()), "{line}");
    assert!(line.contains("%)"), "{line}");
    assert!(
        line.contains("inodes (") || line.ends_with("no inode limit"),
        "{line}"
    );
    let json = stdout_of(&sb.run(&["status", "--home-only", "--json"]));
    for key in [
        "\"fs_bytes\":",
        "\"fs_free_bytes\":",
        "\"fs_free_inodes\":",
        "\"fs_low\":",
    ] {
        assert!(json.contains(key), "{key} in {json}");
    }
}

/// A child holding a file open on descriptor 3, killed on drop.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct Holder(std::process::Child);

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Holder {
    fn open(path: &Path) -> Self {
        let child = Command::new("sh")
            .arg("-c")
            .arg("exec 3<\"$1\"; exec sleep 60")
            .arg("sh")
            .arg(path)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        // Wait until the shell has opened the file and become sleep.
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(5) {
            let out = common::rtrash_real(common::Real::NoTrash)
                .args(["who", "--json"])
                .arg(path)
                .output()
                .unwrap();
            if String::from_utf8_lossy(&out.stdout).contains(&format!("\"pid\":{}", child.id())) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        Holder(child)
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for Holder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn who_names_the_process_holding_a_file_or_its_directory() {
    let sb = Sandbox::new("who");
    let held = sb.touch("held.log");
    sb.touch("idle.log");
    let child = Holder::open(&held);
    let out = sb.run(&["who", "held.log"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let line = format!("{}", child.pid());
    let text = stdout_of(&out);
    assert!(
        text.lines()
            .any(|l| l.trim_start().starts_with(&line) && l.contains("fd 3")),
        "{text}"
    );
    let out = sb.run(&["who", "--json", "."]);
    assert!(out.status.success());
    assert!(stdout_of(&out).contains(&format!("\"pid\":{},", child.pid())));
    let out = sb.run(&["who", "idle.log"]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout_of(&out));
    assert!(stdout_of(&out).is_empty());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn put_warns_about_an_open_file_and_refuses_it_on_request() {
    let sb = Sandbox::new("put-in-use");
    let held = sb.touch("held.log");
    let child = Holder::open(&held);
    let pin = format!("--trash-dir={}", sb.trash().display());
    let out = sb.run(&["put", &pin, "--in-use=refuse", "held.log"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr_of(&out);
    assert!(
        err.contains("in use by pid") && err.contains(&child.pid().to_string()),
        "{err}"
    );
    assert!(held.exists());
    let out = common::rtrash(&sb.root)
        .args(["put", &pin, "held.log"])
        .current_dir(sb.work())
        .env("RTRASH_IN_USE", "refuse")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(held.exists());
    let out = sb.run(&["put", &pin, "held.log"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains("warning: 'held.log' is in use by pid"));
    assert!(!held.exists());
    sb.touch("idle.log");
    let out = sb.run(&["put", &pin, "--in-use=refuse", "idle.log"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(stderr_of(&out).is_empty());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn reap_shared_reports_what_a_delete_frees() {
    let sb = Sandbox::new("reap-shared");
    let tmp = sb.root.join("tmp");
    fs::create_dir_all(&tmp).unwrap();
    let big = tmp.join("big.bin");
    fs::write(&big, vec![7u8; 256 * 1024]).unwrap();
    // An APFS clone shares every byte with big.bin.
    let clone = tmp.join("clone.bin");
    let cloned = cfg!(target_os = "macos")
        && Command::new("cp")
            .args(["-c", big.to_str().unwrap(), clone.to_str().unwrap()])
            .status()
            .is_ok_and(|s| s.success());
    for p in [&big, &clone] {
        if p.exists() {
            let day_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400);
            fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_times(
                    fs::FileTimes::new()
                        .set_accessed(day_ago)
                        .set_modified(day_ago),
                )
                .unwrap();
        }
    }
    let tmp_arg = tmp.to_str().unwrap();
    let out = sb.run(&[
        "reap",
        "--older-than=1h",
        "--age-by=m",
        "--dry-run",
        "--shared",
        "--json",
        tmp_arg,
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(
        text.contains(r#""shared":"#) && text.contains(r#""unique":"#),
        "{text}"
    );
    let field = |entry: &str, key: &str| -> u64 {
        let at = text
            .find(entry)
            .unwrap_or_else(|| panic!("{entry} missing: {text}"));
        let rest = &text[at..];
        let v = &rest[rest.find(&format!("\"{key}\":")).unwrap() + key.len() + 3..];
        v[..v.find(|c: char| !c.is_ascii_digit()).unwrap()]
            .parse()
            .unwrap()
    };
    let (bytes, unique) = (field("big.bin", "bytes"), field("big.bin", "unique"));
    assert!(unique <= bytes, "{text}");
    if cloned {
        assert!(
            field("clone.bin", "unique") < field("clone.bin", "bytes"),
            "{text}"
        );
    }
    let out = sb.run(&[
        "reap",
        "--older-than=1h",
        "--age-by=m",
        "--dry-run",
        "--shared",
        tmp_arg,
    ]);
    assert!(stdout_of(&out).contains(", frees "), "{}", stdout_of(&out));
    assert!(
        stderr_of(&out).contains("in extents shared with other files"),
        "{}",
        stderr_of(&out)
    );
    assert!(big.exists());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn reap_when_below_exits_while_space_is_fine_and_reaps_when_short() {
    let sb = Sandbox::new("reap-below");
    let tmp = sb.root.join("tmp");
    fs::create_dir_all(&tmp).unwrap();
    let old = stale_file(&tmp, "old.txt");
    let tmp_arg = tmp.to_str().unwrap();
    // Every file system has a byte free.
    let out = sb.run(&[
        "reap",
        "--age-by=m",
        "--older-than=1h",
        "--when-below=1",
        "-v",
        tmp_arg,
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("free space is above the target"),
        "{}",
        stderr_of(&out)
    );
    assert!(old.exists());
    let out = sb.run(&[
        "reap",
        "--age-by=m",
        "--older-than=1h",
        "--when-below=1",
        "--json",
        tmp_arg,
    ]);
    assert!(
        stdout_of(&out).contains(r#""space_ok":true"#),
        "{}",
        stdout_of(&out)
    );
    // No file system is entirely free, so this always reaps.
    let out = sb.run(&[
        "reap",
        "--age-by=m",
        "--older-than=1h",
        "--when-below=100%",
        "--until=100%",
        "--permanent",
        tmp_arg,
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!old.exists());
    for bad in [&["reap", "--when-below=lots"][..], &["reap", "--until=10%"]] {
        assert_eq!(sb.run(bad).status.code(), Some(2), "{bad:?}");
    }
}

#[cfg(all(any(target_os = "linux", target_os = "macos"), debug_assertions))]
#[test]
fn reap_watch_reaps_on_each_pass_and_needs_when_below() {
    let sb = Sandbox::new("reap-watch");
    let tmp = sb.root.join("tmp");
    fs::create_dir_all(&tmp).unwrap();
    let old = stale_file(&tmp, "old.txt");
    let tmp_arg = tmp.to_str().unwrap();
    let args = [
        "reap",
        "--watch=1s",
        "--age-by=m",
        "--older-than=1h",
        "--when-below=100%",
        "--until=100%",
        "--permanent",
        tmp_arg,
    ];
    let out = common::rtrash(&sb.root)
        .args(args)
        .env("RTRASH_TEST_WATCH_ROUNDS", "2")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!old.exists());
    // While space is fine, ticks after the first skip the full pass after
    // one statfs per file system.
    let fresh = stale_file(&tmp, "new-old.txt");
    let out = common::rtrash(&sb.root)
        .args([
            "reap",
            "--watch=1s",
            "--age-by=m",
            "--older-than=1h",
            "--when-below=1",
            "-v",
            tmp_arg,
        ])
        .env("RTRASH_TEST_WATCH_ROUNDS", "3")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    let err = stderr_of(&out);
    assert_eq!(
        err.matches("free space is above the target").count(),
        3,
        "{err}"
    );
    assert_eq!(err.matches("skipping this pass").count(), 2, "{err}");
    assert!(fresh.exists());
    for bad in [
        &["reap", "--watch", tmp_arg][..],
        &["reap", "--watch=0s", "--when-below=10%", tmp_arg],
        &["reap", "--watch", "--when-below=10%", "-n", tmp_arg],
    ] {
        assert_eq!(sb.run(bad).status.code(), Some(2), "{bad:?}");
    }
}

/// `--install-timer --watch` writes a job that stays up instead of a
/// schedule, and still needs `--when-below`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn reap_install_timer_with_watch_writes_a_service_that_stays_up() {
    let sb = Sandbox::new("reap-watch-unit");
    let out = reap_cmd(
        &sb,
        &[
            "reap",
            "--install-timer",
            "--watch=5m",
            "--when-below=10%",
            "-p",
            "ai",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    if cfg!(target_os = "macos") {
        let p = sb
            .root
            .join("Library/LaunchAgents/io.github.haozeke.rtrash-reap.plist");
        let body = fs::read_to_string(&p).unwrap();
        assert!(
            body.contains("KeepAlive") && body.contains("--watch=5m"),
            "{body}"
        );
    } else {
        let d = sb.root.join(".config/systemd/user");
        let body = fs::read_to_string(d.join("rtrash-reap.service")).unwrap();
        assert!(
            body.contains("Type=simple") && body.contains("--watch=5m"),
            "{body}"
        );
        assert!(!d.join("rtrash-reap.timer").exists());
        assert!(stdout_of(&out).contains("enable --now rtrash-reap.service"));
    }
    let text = stdout_of(&reap_cmd(&sb, &["reap", "--timer-status"]));
    assert!(text.starts_with("installed:"), "{text}");
    let out = reap_cmd(&sb, &["reap", "--remove-timer"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(stdout_of(&out).contains("removed "), "{}", stdout_of(&out));
    // The service manager calls went to the stand-in, never the session.
    let calls = fs::read_to_string(sb.root.join("service-calls.log")).unwrap();
    let stop = if cfg!(target_os = "macos") {
        "launchctl bootout gui/"
    } else {
        "systemctl --user disable --now rtrash-reap.service"
    };
    assert!(calls.contains(stop), "{calls}");
    assert_eq!(
        reap_cmd(&sb, &["reap", "--install-timer", "--watch", "-n"])
            .status
            .code(),
        Some(2)
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn reap_install_timer_prints_on_dry_run_and_writes_user_units() {
    let sb = Sandbox::new("reap-timer");
    let files: Vec<PathBuf> = if cfg!(target_os = "macos") {
        vec![sb
            .root
            .join("Library/LaunchAgents/io.github.haozeke.rtrash-reap.plist")]
    } else {
        let d = sb.root.join(".config/systemd/user");
        vec![d.join("rtrash-reap.service"), d.join("rtrash-reap.timer")]
    };
    let args = [
        "reap",
        "--install-timer=30m",
        "-n",
        "-p",
        "ai",
        "--when-below=10%",
    ];
    let out = reap_cmd(&sb, &args);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(
        text.contains("--when-below=10%") && text.contains("1800"),
        "{text}"
    );
    assert!(
        files.iter().all(|f| !f.exists()),
        "the dry run wrote a file"
    );

    let out = reap_cmd(
        &sb,
        &args[..2]
            .iter()
            .chain(&args[3..])
            .copied()
            .collect::<Vec<_>>(),
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    for f in &files {
        let body = fs::read_to_string(f).unwrap_or_else(|e| panic!("{}: {e}", f.display()));
        assert!(
            body.contains("reap") && !body.contains("--dry-run"),
            "{body}"
        );
    }
    assert!(
        stdout_of(&out).contains("turn it on with"),
        "{}",
        stdout_of(&out)
    );
    assert_eq!(
        reap_cmd(&sb, &["reap", "--install-timer=5s"]).status.code(),
        Some(2)
    );
    // --remove-timer: -n lists the files and keeps them; then they go.
    // --timer-status names the installed files.
    let out = reap_cmd(&sb, &["reap", "--timer-status"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(
        text.starts_with("installed:")
            && files.iter().all(|f| text.contains(&*f.to_string_lossy())),
        "{text}"
    );
    assert_eq!(
        reap_cmd(&sb, &["reap", "--timer-status", "-n"])
            .status
            .code(),
        Some(2)
    );
    let out = reap_cmd(&sb, &["reap", "--remove-timer", "-n"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("would remove"),
        "{}",
        stdout_of(&out)
    );
    assert!(
        files.iter().all(|f| f.exists()),
        "the dry run removed a file"
    );
    let out = reap_cmd(&sb, &["reap", "--remove-timer"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(files.iter().all(|f| !f.exists()), "{}", stdout_of(&out));
    let out = reap_cmd(&sb, &["reap", "--remove-timer"]);
    assert!(
        out.status.success() && stdout_of(&out).contains("no reap timer installed"),
        "{}",
        stdout_of(&out)
    );
    assert_eq!(
        reap_cmd(&sb, &["reap", "--remove-timer", "-p", "ai"])
            .status
            .code(),
        Some(2)
    );
    let out = reap_cmd(&sb, &["reap", "--timer-status"]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout_of(&out));
    assert!(stdout_of(&out).contains("no reap timer installed"));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn reap_purge_trash_empties_the_oldest_trash_items_when_short() {
    let sb = Sandbox::new("reap-purge");
    let tmp = sb.root.join("tmp");
    fs::create_dir_all(&tmp).unwrap();
    let gone = write_at(&sb.root, "work/gone.txt", "g");
    let out = reap_cmd(&sb, &["put", gone.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let info = sb.root.join("xdg/Trash/info/gone.txt.trashinfo");
    assert!(info.exists(), "put did not use the sandbox trash");
    let tmp_arg = tmp.to_str().unwrap();
    let args = [
        "reap",
        "--when-below=100%",
        "--until=100%",
        "--purge-trash",
        "--older-than=1h",
        "--age-by=m",
    ];
    let mut dry: Vec<&str> = args.to_vec();
    dry.extend(["-n", tmp_arg]);
    let out = reap_cmd(&sb, &dry);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("would empty 1 item"),
        "{}",
        stderr_of(&out)
    );
    assert!(info.exists(), "the dry run emptied the trash");
    let mut real: Vec<&str> = args.to_vec();
    real.push(tmp_arg);
    let out = reap_cmd(&sb, &real);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("emptied 1 item"),
        "{}",
        stderr_of(&out)
    );
    assert!(!info.exists());
    // The history says purge, so the drop is not mistaken for a plain reap.
    let hist = fs::read_to_string(sb.root.join("xdg/Trash/rtrash-history")).unwrap();
    let events: Vec<&str> = hist.lines().filter_map(|l| l.split(' ').nth(1)).collect();
    assert_eq!(events.last(), Some(&"purge"), "{hist}");
    assert!(
        !events.contains(&"reap"),
        "the dry run wrote a line: {hist}"
    );
    assert_eq!(
        reap_cmd(&sb, &["reap", "--purge-trash"]).status.code(),
        Some(2)
    );
}

#[cfg(unix)]
#[test]
fn put_warns_when_the_trash_grows_past_the_warn_size() {
    let sb = Sandbox::new("put-warn");
    let put = |name: &str, limit: &str| {
        let f = write_at(&sb.root, &format!("work/{name}"), "some bytes");
        common::rtrash(&sb.root)
            .args(["put", f.to_str().unwrap()])
            .env("RTRASH_WARN_SIZE", limit)
            .output()
            .unwrap()
    };
    let out = put("a.txt", "1");
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("past RTRASH_WARN_SIZE"),
        "{}",
        stderr_of(&out)
    );
    let out = put("b.txt", "100%");
    assert!(
        !stderr_of(&out).contains("past RTRASH_WARN_SIZE"),
        "{}",
        stderr_of(&out)
    );
    let out = put("c.txt", "lots");
    assert!(out.status.success());
    assert!(
        stderr_of(&out).contains("ignoring RTRASH_WARN_SIZE='lots'"),
        "{}",
        stderr_of(&out)
    );
}

#[cfg(unix)]
#[test]
fn trash_stats_summarizes_each_trash_as_text_and_json() {
    let sb = Sandbox::new("trash-stats");
    let run = |args: &[&str]| common::rtrash(&sb.root).args(args).output().unwrap();
    let small = write_at(&sb.root, "work/small.txt", "s");
    let big = write_at(&sb.root, "work/big.bin", &"b".repeat(200_000));
    let tree = write_at(&sb.root, "work/tree/a/b.txt", "t");
    for p in [&small, &big] {
        assert!(run(&["put", p.to_str().unwrap()]).status.success());
    }
    let tree_dir = tree.parent().unwrap().parent().unwrap();
    assert!(run(&["put", "-r", tree_dir.to_str().unwrap()])
        .status
        .success());
    fs::write(sb.root.join("xdg/Trash/files/stray"), "o").unwrap();
    let out = run(&["trash-stats", "--home-only", "--top=1", "--json"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(text.contains(r#""items":4,"#), "{text}");
    assert!(text.contains(r#""orphans":1,"#), "{text}");
    // files: small, big, tree, tree/a, tree/a/b.txt, stray; info: 3.
    assert!(text.contains(r#""inodes":9,"#), "{text}");
    let top = &text[text.find(r#""top":["#).unwrap()..];
    assert!(
        top.contains("big.bin") && !top.contains("small.txt"),
        "{text}"
    );
    assert!(text.contains(r#""oldest":{"name":"#), "{text}");
    let out = run(&["trash-stats", "--home-only"]);
    let plain = stdout_of(&out);
    assert!(
        plain.contains("4 items") && plain.contains("#1: big.bin"),
        "{plain}"
    );
    assert_eq!(run(&["trash-stats", "--top=x"]).status.code(), Some(2));
}

/// Set a file's access and modification times `secs` into the past.
#[cfg(unix)]
fn age_by(p: &Path, secs: u64) {
    let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
    fs::File::options()
        .write(true)
        .open(p)
        .unwrap()
        .set_times(fs::FileTimes::new().set_accessed(t).set_modified(t))
        .unwrap();
}

/// Files and directories at or below `dir`, as RTRASH_TEST_FS counts them.
#[cfg(unix)]
fn count_entries(dir: &Path) -> u64 {
    let mut n = 0;
    for e in fs::read_dir(dir).unwrap().flatten() {
        n += 1;
        if e.file_type().unwrap().is_dir() {
            n += count_entries(&e.path());
        }
    }
    n
}

#[cfg(unix)]
#[test]
fn reap_when_below_stops_once_the_measured_space_is_back() {
    let sb = Sandbox::new("reap-seam");
    let tmp = sb.root.join("tmp");
    fs::create_dir_all(&tmp).unwrap();
    let files: Vec<PathBuf> = (0..3)
        .map(|i| {
            let p = tmp.join(format!("f{i}.bin"));
            fs::write(&p, vec![1u8; 100_000]).unwrap();
            // f0 is the oldest.
            age_by(&p, 86_400 * (10 - i as u64));
            p
        })
        .collect();
    let used: u64 = files
        .iter()
        .map(|p| {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(p).unwrap().blocks() * 512
        })
        .sum();
    let total = 1_000_000u64;
    // Short by about one and a half files: the two oldest must go.
    let below = total - used + used / 2;
    let spec = format!("{total}:{}", tmp.display());
    let run = |extra: &[&str]| {
        let mut args = vec![
            "reap".to_string(),
            "--older-than=1h".into(),
            "--age-by=m".into(),
            "--permanent".into(),
            format!("--when-below={below}"),
        ];
        args.extend(extra.iter().map(|s| s.to_string()));
        args.push(tmp.to_str().unwrap().into());
        common::rtrash(&sb.root)
            .args(&args)
            .env("RTRASH_TEST_FS", &spec)
            .output()
            .unwrap()
    };
    let out = run(&["-n", "-v"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(text.contains("f0.bin") && text.contains("f1.bin"), "{text}");
    assert!(
        text.contains("kept") && text.contains("back above the target"),
        "{text}"
    );
    let out = run(&[]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        !files[0].exists() && !files[1].exists(),
        "{}",
        stdout_of(&out)
    );
    assert!(
        files[2].exists(),
        "reaped past the target: {}",
        stdout_of(&out)
    );
    // Now above the target: a second run exits at once.
    let out = run(&["-v"]);
    assert!(
        stderr_of(&out).contains("free space is above the target"),
        "{}",
        stderr_of(&out)
    );
    assert!(files[2].exists());
}

#[cfg(unix)]
#[test]
fn reap_purge_trash_counts_the_inodes_each_item_frees() {
    let sb = Sandbox::new("reap-inodes");
    let tmp = sb.root.join("tmp");
    fs::create_dir_all(&tmp).unwrap();
    let tree = sb.root.join("work/tree");
    for i in 0..20 {
        write_at(&tree, &format!("f{i}"), "x");
    }
    let single = write_at(&sb.root, "work/single.txt", "s");
    let mut put = common::rtrash(&sb.root);
    put.args(["put", "-r", tree.to_str().unwrap()]);
    assert!(put.output().unwrap().status.success());
    common::next_trash_second();
    let mut put = common::rtrash(&sb.root);
    put.args(["put", single.to_str().unwrap()]);
    assert!(put.output().unwrap().status.success());
    let info = sb.root.join("xdg/Trash/info");
    assert_eq!(fs::read_dir(&info).unwrap().count(), 2);
    // 1000 inodes; short by a few, which the tree (22 inodes) covers.
    let n0 = count_entries(&sb.root);
    let pct = (1000 - n0 + 5).div_ceil(10);
    let spec = format!("{}:1000:{}", u64::MAX / 2, sb.root.display());
    let mut reap = common::rtrash(&sb.root);
    reap.args([
        "reap",
        &format!("--when-below=inodes={pct}%"),
        "--purge-trash",
        "--older-than=1h",
        tmp.to_str().unwrap(),
    ])
    .env("RTRASH_TEST_FS", &spec);
    let out = reap.output().unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("emptied 1 item"),
        "{}",
        stderr_of(&out)
    );
    let left: Vec<String> = fs::read_dir(&info)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, ["single.txt.trashinfo"], "the newer item went too");
}

#[cfg(unix)]
#[test]
fn put_keeps_a_running_trash_total_that_matches_a_full_count() {
    let sb = Sandbox::new("trash-total");
    let run = |args: &[&str]| {
        common::rtrash(&sb.root)
            .args(args)
            .env_remove("RTRASH_WARN_SIZE")
            .env_remove("RTRASH_MAX_SIZE")
            .output()
            .unwrap()
    };
    let total_file = sb.root.join("xdg/Trash/rtrash-total");
    let stats_bytes = || {
        let text = stdout_of(&run(&["trash-stats", "--home-only", "--json", "--top=0"]));
        let at = text.find(r#""total":{"bytes":"#).unwrap() + r#""total":{"bytes":"#.len();
        text[at..text[at..].find(',').unwrap() + at]
            .parse::<u64>()
            .unwrap()
    };
    let stored = || {
        let body = fs::read_to_string(&total_file).unwrap();
        let f: Vec<&str> = body.split_whitespace().collect();
        (f[1].parse::<u64>().unwrap(), f[2].parse::<u64>().unwrap())
    };
    // The default warning (20% of the disk) measures the trash after the
    // first put and stores the total.
    let a = write_at(&sb.root, "work/a.bin", &"a".repeat(50_000));
    let out = run(&["put", a.to_str().unwrap()]);
    assert!(
        out.status.success() && !stderr_of(&out).contains("warning"),
        "{}",
        stderr_of(&out)
    );
    assert!(total_file.exists(), "no running total after a put");
    assert_eq!(stored(), (stats_bytes(), 1));
    // Later puts add to it without a pass.
    let tree = write_at(&sb.root, "work/tree/x/y.txt", "y");
    let out = run(&[
        "put",
        "-r",
        tree.parent().unwrap().parent().unwrap().to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(stored(), (stats_bytes(), 2));
    // Another tool's change makes it stale; the next put measures again.
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(
        sb.root.join("xdg/Trash/info/ghost.trashinfo"),
        "[Trash Info]\nPath=/ghost\nDeletionDate=2020-01-01T00:00:00\n",
    )
    .unwrap();
    let c = write_at(&sb.root, "work/c.txt", "c");
    assert!(run(&["put", c.to_str().unwrap()]).status.success());
    assert_eq!(stored(), (stats_bytes(), 4));
}

/// `rm` keeps a current total current, and `trash-stats --total` reads
/// it without a walk; `--measure` stores one where none is current.
#[cfg(unix)]
#[test]
fn rm_carries_the_total_and_trash_stats_total_reads_it() {
    let sb = Sandbox::new("total-rm");
    let run = |args: &[&str]| {
        common::rtrash(&sb.root)
            .args(args)
            .env_remove("RTRASH_WARN_SIZE")
            .env_remove("RTRASH_MAX_SIZE")
            .output()
            .unwrap()
    };
    let total = || stdout_of(&run(&["trash-stats", "--home-only", "--total", "--json"]));
    let a = write_at(&sb.root, "work/a.bin", &"a".repeat(50_000));
    let tree = write_at(&sb.root, "work/tree/x/y.txt", &"y".repeat(9_000));
    for p in [a, tree.parent().unwrap().parent().unwrap().to_path_buf()] {
        let out = run(&["put", "-r", p.to_str().unwrap()]);
        assert!(out.status.success(), "{}", stderr_of(&out));
    }
    let out = run(&["rm", "--home-only", "a.bin"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let carried = total();
    assert!(carried.contains(r#""items":1,"stale":false"#), "{carried}");
    // The tree's record, tree, x and y: counted at put, read without a walk.
    assert!(carried.contains(r#""inodes":4}"#), "{carried}");
    // An orphan is not in the total, so removing one keeps it as it is.
    fs::write(
        sb.root.join("xdg/Trash/files/orphan.bin"),
        "o".repeat(7_000),
    )
    .unwrap();
    let out = run(&["rm", "--home-only", "orphan.bin"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!sb.root.join("xdg/Trash/files/orphan.bin").exists());
    assert_eq!(total(), carried);
    // A fresh measure agrees with the carried total.
    fs::remove_file(sb.root.join("xdg/Trash/rtrash-total")).unwrap();
    assert!(total().contains(r#""bytes":null"#), "{}", total());
    let out = run(&[
        "trash-stats",
        "--home-only",
        "--total",
        "--measure",
        "--json",
    ]);
    assert_eq!(stdout_of(&out), carried);
}

/// A total that appears while a restore runs (stored by another process)
/// still loses the restored item, sized where it now lives.
#[cfg(all(unix, debug_assertions))]
#[test]
fn restore_carries_a_total_stored_while_it_ran() {
    let sb = Sandbox::new("total-restore-midway");
    let run = |args: &[&str], midway: Option<&str>| {
        let mut c = common::rtrash(&sb.root);
        c.args(args)
            .env_remove("RTRASH_WARN_SIZE")
            .env_remove("RTRASH_MAX_SIZE");
        if let Some(m) = midway {
            c.env("RTRASH_TEST_TOTAL_MIDWAY", m);
        }
        c.output().unwrap()
    };
    let total = |measure: bool| {
        let mut a = vec!["trash-stats", "--home-only", "--total", "--json"];
        if measure {
            a.push("--measure");
        }
        stdout_of(&run(&a, None))
    };
    let field = |j: &str, k: &str| -> u64 {
        let at = j.find(&format!("\"{k}\":")).unwrap() + k.len() + 3;
        j[at..].split([',', '}']).next().unwrap().parse().unwrap()
    };
    let a = write_at(&sb.root, "work/a.bin", &"a".repeat(30_000));
    let tree = write_at(&sb.root, "work/t/x/y.txt", &"y".repeat(12_000));
    let t = tree.parent().unwrap().parent().unwrap().to_path_buf();
    for p in [&a, &t] {
        assert!(run(&["put", "-r", p.to_str().unwrap()], None)
            .status
            .success());
    }
    let full = total(true);
    let midway = format!("{}:{}", field(&full, "bytes"), field(&full, "items"));
    fs::remove_file(sb.root.join("xdg/Trash/rtrash-total")).unwrap();
    let out = run(
        &["restore", "--plain", "--home-only", t.to_str().unwrap()],
        Some(&midway),
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(t.join("x/y.txt").exists());
    let carried = total(false);
    assert!(carried.contains(r#""items":1,"stale":false"#), "{carried}");
    fs::remove_file(sb.root.join("xdg/Trash/rtrash-total")).unwrap();
    assert_eq!(total(true), carried);
}

/// `trash-stats --total --exit-low` exits 3 while the trash's file
/// system is under 10% free, 0 otherwise, and still prints the totals.
#[cfg(unix)]
#[test]
fn trash_stats_exit_low_flags_a_full_file_system() {
    let sb = Sandbox::new("total-exit-low");
    let run = |args: &[&str], total: u64| {
        common::rtrash(&sb.root)
            .args(args)
            .env("RTRASH_TEST_FS", format!("{total}:{}", sb.root.display()))
            .env_remove("RTRASH_WARN_SIZE")
            .env_remove("RTRASH_MAX_SIZE")
            .output()
            .unwrap()
    };
    let p = write_at(&sb.root, "work/a.bin", &"z".repeat(50_000));
    assert!(run(&["put", p.to_str().unwrap()], 1 << 40).status.success());
    let args = [
        "trash-stats",
        "--home-only",
        "--total",
        "--exit-low",
        "--json",
    ];
    let roomy = run(&args, 1 << 40);
    assert_eq!(roomy.status.code(), Some(0), "{}", stderr_of(&roomy));
    assert!(stdout_of(&roomy).contains(r#""low":false"#));
    // The sandbox holds about 50 kB, so a 55 kB file system is over 90% full.
    let full = run(&args, 55_000);
    assert_eq!(full.status.code(), Some(3), "{}", stderr_of(&full));
    assert!(
        stdout_of(&full).contains(r#""low":true"#),
        "{}",
        stdout_of(&full)
    );
    assert_eq!(
        run(&["trash-stats", "--exit-low"], 55_000).status.code(),
        Some(2)
    );
}

/// `empty --until-free` removes the oldest trash items until the file
/// system has the asked free room, and no more.
#[cfg(unix)]
#[test]
fn empty_until_free_removes_the_oldest_until_there_is_room() {
    let sb = Sandbox::new("until-free");
    let run = |args: &[&str], total: u64| {
        common::rtrash(&sb.root)
            .args(args)
            .env("RTRASH_TEST_FS", format!("{total}:{}", sb.root.display()))
            .env_remove("RTRASH_WARN_SIZE")
            .env_remove("RTRASH_MAX_SIZE")
            .output()
            .unwrap()
    };
    let big = 1u64 << 40;
    for n in ["old.bin", "new.bin"] {
        let p = write_at(&sb.root, &format!("work/{n}"), &"z".repeat(40_000));
        assert!(run(&["put", p.to_str().unwrap()], big).status.success());
    }
    let info = sb.root.join("xdg/Trash/info");
    let old = info.join("old.bin.trashinfo");
    let body = fs::read_to_string(&old).unwrap();
    let date = body
        .lines()
        .find(|l| l.starts_with("DeletionDate="))
        .unwrap();
    fs::write(&old, body.replace(date, "DeletionDate=2020-01-01T00:00:00")).unwrap();
    let spaced = stdout_of(&run(
        &["trash-stats", "--home-only", "--total", "--space", "--json"],
        big,
    ));
    let avail: u64 = spaced
        .split(r#""avail":"#)
        .nth(1)
        .unwrap()
        .split(',')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let used = big - avail;
    // 10 kB free, 30 kB wanted: one 40 kB item is enough.
    let total = used + 10_000;
    let roomy = run(
        &["empty", "--home-only", "--until-free=30000", "--json"],
        big,
    );
    assert_eq!(roomy.status.code(), Some(0), "{}", stderr_of(&roomy));
    assert!(
        stdout_of(&roomy).contains(r#""short":0"#),
        "{}",
        stdout_of(&roomy)
    );
    let dry = run(&["empty", "--home-only", "--until-free=30000", "-n"], total);
    assert!(
        stderr_of(&dry).contains("would empty 1 item"),
        "{}",
        stderr_of(&dry)
    );
    assert!(old.exists());
    let o = run(
        &["empty", "--home-only", "--until-free=30000", "--json"],
        total,
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr_of(&o));
    assert!(
        stdout_of(&o).contains(r#""short":1,"still_short":0,"file_systems":[{"path":"#)
            && stdout_of(&o).contains(r#""left_bytes":0,"left_inodes":0}]"#),
        "{}",
        stdout_of(&o)
    );
    assert!(!old.exists() && info.join("new.bin.trashinfo").exists());
    // Nothing left can make 200 kB free: exit 1.
    let o = run(
        &["empty", "--home-only", "--until-free=200000"],
        used + 10_000,
    );
    assert_eq!(o.status.code(), Some(1), "{}", stderr_of(&o));
    assert!(stderr_of(&o).contains("still under"), "{}", stderr_of(&o));
    assert_eq!(
        run(&["empty", "--home-only", "--until-free=10%", "3"], big)
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        run(&["empty", "--home-only", "--until-free=lots"], big)
            .status
            .code(),
        Some(2)
    );
}

/// `empty --dangling` keeps a current total current.
#[cfg(unix)]
#[test]
fn empty_dangling_carries_the_total() {
    let sb = Sandbox::new("total-dangling");
    let run = |args: &[&str]| {
        common::rtrash(&sb.root)
            .args(args)
            .env_remove("RTRASH_WARN_SIZE")
            .env_remove("RTRASH_MAX_SIZE")
            .output()
            .unwrap()
    };
    let total = |measure: bool| {
        let mut a = vec!["trash-stats", "--home-only", "--total", "--json"];
        if measure {
            a.push("--measure");
        }
        stdout_of(&run(&a))
    };
    for n in ["a.bin", "b.bin"] {
        let p = write_at(&sb.root, &format!("work/{n}"), &"z".repeat(20_000));
        assert!(run(&["put", p.to_str().unwrap()]).status.success());
    }
    let trash = sb.root.join("xdg/Trash");
    fs::remove_file(trash.join("files/b.bin")).unwrap();
    fs::remove_file(trash.join("rtrash-total")).unwrap();
    assert!(total(true).contains(r#""items":2,"stale":false"#));
    let out = run(&["empty", "--dangling", "--home-only"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let carried = total(false);
    assert!(carried.contains(r#""items":1,"stale":false"#), "{carried}");
    fs::remove_file(trash.join("rtrash-total")).unwrap();
    assert_eq!(total(true), carried);
    // --space adds the file system, from a statfs and no walk.
    let spaced = stdout_of(&run(&[
        "trash-stats",
        "--home-only",
        "--total",
        "--space",
        "--json",
    ]));
    assert!(
        spaced.contains(r#""fs":{"total":"#) && spaced.contains(r#""low":"#),
        "{spaced}"
    );
    // One file item left: its record and its payload.
    assert!(
        spaced.contains(r#""stale":false,"inodes":2,"fs""#),
        "{spaced}"
    );
    let text = stdout_of(&run(&["trash-stats", "--home-only", "--total", "--space"]));
    assert!(text.contains("1 item, 2 inodes"), "{text}");
    assert!(text.contains("\tfree "), "{text}");
    assert_eq!(run(&["trash-stats", "--space"]).status.code(), Some(2));
}
#[test]
#[cfg(unix)]
fn empty_restore_and_prune_keep_the_running_trash_total_current() {
    let sb = Sandbox::new("trash-total-keep");
    let run = |args: &[&str]| {
        common::rtrash(&sb.root)
            .args(args)
            .env_remove("RTRASH_WARN_SIZE")
            .env_remove("RTRASH_MAX_SIZE")
            .output()
            .unwrap()
    };
    let trash = sb.root.join("xdg/Trash");
    let stats_bytes = || {
        let text = stdout_of(&run(&["trash-stats", "--home-only", "--json", "--top=0"]));
        let at = text.find(r#""total":{"bytes":"#).unwrap() + r#""total":{"bytes":"#.len();
        text[at..text[at..].find(',').unwrap() + at]
            .parse::<u64>()
            .unwrap()
    };
    // The stored (bytes, items), which must still be current.
    let stored = || {
        let body = fs::read_to_string(trash.join("rtrash-total")).unwrap();
        let f: Vec<&str> = body.split_whitespace().collect();
        let stamp = fs::metadata(trash.join("info"))
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        assert_eq!(f[3].parse::<u128>().unwrap(), stamp, "stale total: {body}");
        (f[1].parse::<u64>().unwrap(), f[2].parse::<u64>().unwrap())
    };
    let ok = |out: std::process::Output| assert!(out.status.success(), "{}", stderr_of(&out));
    let mut paths = Vec::new();
    for (name, body) in [
        ("a.bin", "a".repeat(40_000)),
        ("b.bin", "b".repeat(9_000)),
        ("c.txt", "c".into()),
    ] {
        let p = write_at(&sb.root, &format!("work/{name}"), &body);
        ok(run(&["put", p.to_str().unwrap()]));
        paths.push(p);
    }
    let tree = write_at(&sb.root, "work/tree/x/y.txt", "y");
    ok(run(&[
        "put",
        "-r",
        tree.parent().unwrap().parent().unwrap().to_str().unwrap(),
    ]));
    assert_eq!(stored(), (stats_bytes(), 4));
    // A restore takes its item off.
    ok(run(&["restore", "--home-only", paths[2].to_str().unwrap()]));
    assert!(paths[2].exists());
    assert_eq!(stored(), (stats_bytes(), 3));
    // A size budget takes the oldest items off and stores what is left.
    std::thread::sleep(std::time::Duration::from_millis(20));
    ok(run(&["empty", "--home-only", "--max-size=20K"]));
    let (b, n) = stored();
    assert!(n < 3 && b <= 20 * 1024, "{b} {n}");
    assert_eq!((b, n), (stats_bytes(), n));
    // An age pass takes the old items off a current total.
    let infos: Vec<PathBuf> = fs::read_dir(trash.join("info"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    let old = &infos[0];
    let body = fs::read_to_string(old).unwrap();
    let at = body.find("DeletionDate=").unwrap();
    fs::write(
        old,
        format!("{}DeletionDate=2001-01-01T00:00:00\n", &body[..at]),
    )
    .unwrap();
    // The edit made the total stale; a prune that takes nothing renews it.
    ok(run(&["empty", "--home-only", "--max-size=1T"]));
    let n = stored().1;
    ok(run(&["empty", "--home-only", "30"]));
    assert_eq!(stored(), (stats_bytes(), n - 1));
    // A full empty leaves nothing.
    ok(run(&["empty", "--home-only"]));
    assert_eq!(stored(), (0, 0));
}

/// Debug builds only: status reads the trash's file system through
/// space::query, so RTRASH_TEST_FS can make it short and full again.
#[cfg(all(unix, debug_assertions))]
#[test]
fn status_warns_from_the_test_file_system_and_stops_once_it_is_freed() {
    let sb = Sandbox::new("status-seam");
    let big = sb.work().join("big.bin");
    fs::write(&big, vec![7u8; 400_000]).unwrap();
    assert!(sb.run(&["put", big.to_str().unwrap()]).status.success());
    let xdg = sb.root.join("xdg");
    let status = |json: bool| {
        let mut args = vec!["status", "--home-only"];
        if json {
            args.push("--json");
        }
        common::rtrash(&sb.root)
            .args(&args)
            .env("XDG_DATA_HOME", &xdg)
            .env("RTRASH_TEST_FS", format!("1000000:{}", xdg.display()))
            .output()
            .unwrap()
    };
    // 400 KB of a 1 MB disk leaves 60% free: fine.
    let out = status(false);
    assert!(
        out.status.success() && !stderr_of(&out).contains("is low"),
        "{}",
        stderr_of(&out)
    );
    let json = stdout_of(&status(true));
    assert!(
        json.contains("\"fs_bytes\":1000000") && json.contains("\"fs_low\":false"),
        "{json}"
    );
    // Another 550 KB leaves under 10% free.
    fs::write(xdg.join("filler"), vec![1u8; 550_000]).unwrap();
    let out = status(false);
    assert!(
        stderr_of(&out).contains("is low") && stderr_of(&out).contains("empty --until-free=10%"),
        "{}",
        stderr_of(&out)
    );
    assert!(stdout_of(&status(true)).contains("\"fs_low\":true"));
    // Emptying the trash frees enough.
    fs::remove_file(xdg.join("filler")).unwrap();
    assert!(sb.run(&["empty", "--home-only"]).status.success());
    assert!(!stderr_of(&status(false)).contains("is low"));
}

#[test]
#[cfg(unix)]
fn trash_stats_history_records_each_change_rtrash_makes() {
    let sb = Sandbox::new("stats-log");
    let a = sb.work().join("a.bin");
    fs::write(&a, vec![1u8; 30_000]).unwrap();
    assert!(sb.run(&["put", a.to_str().unwrap()]).status.success());
    let b = sb.work().join("b.txt");
    fs::write(&b, "b").unwrap();
    assert!(sb.run(&["put", b.to_str().unwrap()]).status.success());
    let c = sb.work().join("c.txt");
    fs::write(&c, "c").unwrap();
    assert!(sb.run(&["put", c.to_str().unwrap()]).status.success());
    // A restore and an rm each change the trash, so each adds a line.
    let out = sb.run(&["restore", "--home-only", b.to_str().unwrap()]);
    assert!(out.status.success() && b.exists(), "{}", stderr_of(&out));
    let out = sb.run(&["rm", "--home-only", c.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    // The rm line counts the one item left, even in the same clock tick
    // as the restore before it.
    let hist = fs::read_to_string(sb.trash().join("rtrash-history")).unwrap();
    let last = hist.lines().last().unwrap();
    assert!(last.contains(" rm ") && last.ends_with(" 1"), "{hist}");
    // A dry run, a put that fails and an rm that matches nothing change
    // nothing, so they add no line.
    assert!(sb.run(&["empty", "--home-only", "-n"]).status.success());
    assert!(!sb.run(&["put", "missing-file"]).status.success());
    sb.run(&["rm", "--home-only", "nothing-matches"]);
    assert!(sb.run(&["empty", "--home-only"]).status.success());
    let out = sb.run(&["trash-stats", "--home-only", "--history", "--json"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let json = stdout_of(&out);
    let events: Vec<&str> = json
        .match_indices(r#""event":""#)
        .map(|(i, m)| {
            let rest = &json[i + m.len()..];
            &rest[..rest.find('"').unwrap()]
        })
        .collect();
    assert_eq!(
        events,
        ["put", "put", "put", "restore", "rm", "empty"],
        "{json}"
    );
    assert!(
        json.contains(r#""bytes":0,"items":0,"delta_bytes":-"#),
        "{json}"
    );
    // The first line has no line before it; the empty drops one item.
    assert!(
        json.contains(r#""delta_bytes":null,"delta_items":null}"#),
        "{json}"
    );
    assert!(
        json.contains(r#""event":"empty","bytes":0,"items":0,"delta_bytes":-"#),
        "{json}"
    );
    assert!(json.contains(r#","delta_items":-1}]"#), "{json}");
    assert!(json.contains(r#""items":3,"delta_bytes":"#), "{json}");
    assert!(json.contains(r#""event":"rm","bytes":"#), "{json}");
    // Text lists the same lines; without --history there are none.
    let text = stdout_of(&sb.run(&["trash-stats", "--home-only", "--history"]));
    assert_eq!(text.matches("  put  ").count(), 3, "{text}");
    // Each line after the first shows how the size moved.
    assert_eq!(text.matches(" (+").count(), 2, "{text}");
    assert!(
        text.lines()
            .any(|l| l.contains("  empty") && l.contains(" (-")),
        "{text}"
    );
    // The trend bar has a bar per line; the empty at the end is the lowest.
    let trend = text
        .lines()
        .find_map(|l| l.strip_prefix("  trend: "))
        .unwrap();
    assert_eq!(trend.chars().count(), 6, "{text}");
    assert!(trend.ends_with('▁') && trend.contains('█'), "{text}");
    let plain = stdout_of(&sb.run(&["trash-stats", "--home-only", "--json"]));
    assert!(!plain.contains("history"), "{plain}");
    // --since keeps only recent lines: an old one written by hand drops.
    let hist = sb.trash().join("rtrash-history");
    let body = fs::read_to_string(&hist).unwrap();
    fs::write(&hist, format!("1000000000 put 1 1\n{body}")).unwrap();
    let all = stdout_of(&sb.run(&["trash-stats", "--home-only", "--history", "--json"]));
    assert!(all.contains(r#""epoch":1000000000,"#), "{all}");
    let recent = stdout_of(&sb.run(&[
        "trash-stats",
        "--home-only",
        "--history",
        "--since=1d",
        "--json",
    ]));
    assert!(!recent.contains(r#""epoch":1000000000,"#), "{recent}");
    assert_eq!(recent.matches(r#""event":"#).count(), 6, "{recent}");
    for bad in [&["--since=1d"][..], &["--history", "--since=soon"]] {
        let mut args = vec!["trash-stats", "--home-only"];
        args.extend_from_slice(bad);
        assert_eq!(sb.run(&args).status.code(), Some(2), "{bad:?}");
    }
}

/// A file in `dir` whose access and modification times are a day old.
fn stale_file(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, b"old").unwrap();
    let day_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400);
    let f = fs::File::options().write(true).open(&p).unwrap();
    f.set_times(
        fs::FileTimes::new()
            .set_accessed(day_ago)
            .set_modified(day_ago),
    )
    .unwrap();
    p
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn reap_trashes_stale_entries_and_keeps_held_fresh_and_live_lock_ones() {
    let sb = Sandbox::new("reap");
    let tmp = sb.root.join("tmp");
    fs::create_dir_all(&tmp).unwrap();
    let stale = stale_file(&tmp, "stale.txt");
    let held = stale_file(&tmp, "held.txt");
    let fresh = tmp.join("fresh.txt");
    fs::write(&fresh, b"new").unwrap();
    let lock = tmp.join("app.lock");
    fs::write(&lock, format!("{}\n", std::process::id())).unwrap();
    let lock = {
        let day_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400);
        let f = fs::File::options().write(true).open(&lock).unwrap();
        f.set_times(
            fs::FileTimes::new()
                .set_accessed(day_ago)
                .set_modified(day_ago),
        )
        .unwrap();
        lock
    };
    let fifo = tmp.join("pipe");
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: a valid C string and mode.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let child = Holder::open(&held);
    let pin = format!("--trash-dir={}", sb.trash().display());
    let tmp_arg = tmp.to_str().unwrap();

    // Ctime is always new on a fresh file, so age by mtime only.
    let out = sb.run(&[
        "reap",
        "--older-than=1h",
        "--age-by=m",
        "--dry-run",
        "--json",
        tmp_arg,
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(text.contains("\"dry_run\":true"), "{text}");
    let reaped = text.split("\"skipped\"").next().unwrap();
    assert!(reaped.contains("stale.txt"), "{text}");
    for kept in ["held.txt", "fresh.txt", "app.lock", "pipe"] {
        assert!(!reaped.contains(kept), "{kept} in {text}");
    }
    assert!(
        text.contains(&format!("in use by pid {}", child.pid())),
        "{text}"
    );
    assert!(
        text.contains(&format!("lock file of live pid {}", std::process::id())),
        "{text}"
    );
    assert!(text.contains("a named pipe"), "{text}");
    assert!(stale.exists(), "the dry run moved a file");

    let out = sb.run(&["reap", "--older-than=1h", "--age-by=m", &pin, tmp_arg]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!stale.exists());
    assert!(held.exists() && fresh.exists() && lock.exists() && fifo.exists());
    assert_eq!(trash_names(&sb), vec!["stale.txt".to_string()]);

    drop(child);
    let out = sb.run(&[
        "reap",
        "--older-than=1h",
        "--age-by=m",
        "--permanent",
        tmp_arg,
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!held.exists(), "a file no longer held stayed");
    assert!(fresh.exists());
}

#[cfg(unix)]
#[test]
fn reap_refuses_home_and_rejects_bad_ages() {
    let sb = Sandbox::new("reap-args");
    let out = sb.run(&["reap", "--dry-run", sb.root.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr_of(&out).contains("refusing to reap"));
    let out = sb.run(&["reap", "--older-than=3y", "."]);
    assert_eq!(out.status.code(), Some(2));
    let out = sb.run(&["reap", "--age-by=x", "."]);
    assert_eq!(out.status.code(), Some(2));
}

/// Set access and modification times a day back on `p` and everything
/// in it, children first so a directory's own times stay old.
fn age_tree(p: &Path) {
    let day_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400);
    let meta = fs::symlink_metadata(p).unwrap();
    if meta.is_dir() {
        for e in fs::read_dir(p).unwrap() {
            age_tree(&e.unwrap().path());
        }
    }
    let f = if meta.is_dir() {
        fs::File::open(p).unwrap()
    } else {
        fs::File::options().write(true).open(p).unwrap()
    };
    f.set_times(
        fs::FileTimes::new()
            .set_accessed(day_ago)
            .set_modified(day_ago),
    )
    .unwrap();
}

/// `rtrash` in the sandbox, with its HOME and config, without the
/// variables that would point profiles at the host's real caches.
/// What `reap --timer-status` prints from the service manager, with
/// stand-in `systemctl` and `launchctl` scripts first on PATH: the fields
/// it asks for, as text and as JSON, and only those.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn reap_timer_status_reports_what_the_service_manager_says() {
    use std::os::unix::fs::PermissionsExt;
    let sb = Sandbox::new("timer-status-manager");
    let bin_dir = sb.root.join("fake-bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let asked = sb.root.join("asked");
    let script = |name: &str, body: &str| {
        let p = bin_dir.join(name);
        fs::write(
            &p,
            format!("#!/bin/sh\necho \"$@\" >> '{}'\n{body}", asked.display()),
        )
        .unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    };
    script(
        "systemctl",
        "printf 'ActiveState=active\\nLastTriggerUSec=Tue 2026-10-13 21:00:00 CDT\\nNextElapseUSecRealtime=Tue 2026-10-13 22:00:00 CDT\\nUnrelated=x\\n'\n",
    );
    script(
        "launchctl",
        "printf '\\tstate = running\\n\\truns = 3\\n\\tlast exit code = 0\\n\\tunrelated = x\\n'\n",
    );
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = |args: &[&str]| {
        common::rtrash(&sb.root)
            .args(args)
            .current_dir(sb.work())
            .env("PATH", &path)
            .output()
            .unwrap()
    };
    let out = run(&["reap", "--install-timer=1h"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let out = run(&["reap", "--timer-status"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    let json = stdout_of(&run(&["reap", "--timer-status", "--json"]));
    assert!(json.starts_with(r#"{"installed":true,"#), "{json}");
    assert!(
        !text.contains("nrelated") && !json.contains("nrelated"),
        "{text}{json}"
    );
    let asked = fs::read_to_string(&asked).unwrap();
    if cfg!(target_os = "macos") {
        for want in ["state = running", "runs = 3", "last exit code = 0"] {
            assert!(text.contains(want), "{text}");
        }
        assert!(json.contains(r#""manager":"launchd""#), "{json}");
        assert!(
            json.contains(r#""state":{"state":"running","runs":"3","last exit code":"0"}"#),
            "{json}"
        );
        assert!(
            asked.contains("print gui/") && asked.contains("io.github.haozeke.rtrash-reap"),
            "{asked}"
        );
    } else {
        for want in [
            "ActiveState=active",
            "LastTriggerUSec=Tue 2026-10-13 21:00:00 CDT",
            "NextElapseUSecRealtime=Tue 2026-10-13 22:00:00 CDT",
        ] {
            assert!(text.contains(want), "{text}");
        }
        assert!(json.contains(r#""manager":"systemd""#), "{json}");
        assert!(
            json.contains(
                r#""ActiveState":"active","LastTriggerUSec":"Tue 2026-10-13 21:00:00 CDT""#
            ),
            "{json}"
        );
        assert!(asked.contains("--user show rtrash-reap.timer"), "{asked}");
    }
    // A manager that fails reports nothing, and the text says so.
    script("systemctl", "exit 1\n");
    script("launchctl", "exit 1\n");
    let text = stdout_of(&run(&["reap", "--timer-status"]));
    assert!(
        text.contains("the service manager reports nothing"),
        "{text}"
    );
    let json = stdout_of(&run(&["reap", "--timer-status", "--json"]));
    assert!(json.ends_with("\"state\":{}}\n"), "{json}");
    assert_eq!(
        run(&["reap", "--timer-status", "-n"]).status.code(),
        Some(2)
    );
}

/// A stand-in for systemctl and launchctl that logs each call to
/// `service-calls.log` in the sandbox, so no test reaches the real user
/// session.
#[cfg(unix)]
fn fake_service_tool(sb: &Sandbox) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = sb.root.join("fake-service-tool");
    if !p.exists() {
        let log = sb.root.join("service-calls.log");
        fs::write(
            &p,
            format!("#!/bin/sh\necho \"$@\" >> '{}'\n", log.display()),
        )
        .unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }
    p
}

fn reap_cmd(sb: &Sandbox, args: &[&str]) -> Output {
    let mut c = common::rtrash(&sb.root);
    #[cfg(unix)]
    c.env("RTRASH_TEST_SERVICE_TOOL", fake_service_tool(sb));
    c.args(args)
        .current_dir(sb.work())
        .env("XDG_CACHE_HOME", sb.root.join(".cache"))
        .env("TMPDIR", sb.root.join("tmp"))
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_HOME")
        .env_remove("CODEX_HOME")
        .env_remove("HF_HOME")
        .env_remove("MAMBA_ROOT_PREFIX")
        .env_remove("PRE_COMMIT_HOME")
        .env_remove("PREK_HOME")
        .env_remove("OLLAMA_MODELS")
        .env_remove("PUPPETEER_CACHE_DIR")
        .env("PLAYWRIGHT_BROWSERS_PATH", sb.root.join("pw"))
        .output()
        .unwrap()
}

/// The huggingface profile takes a stale partial download and leaves the
/// finished blob beside it; mamba takes a stale tarball and leaves the
/// extracted package.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn reap_profiles_take_partial_downloads_and_tarballs_only() {
    let sb = Sandbox::new("reap-hf-mamba");
    let blobs = sb.root.join(".cache/huggingface/hub/models--org--m/blobs");
    let pkgs = sb.root.join("micromamba/pkgs");
    fs::create_dir_all(&blobs).unwrap();
    fs::create_dir_all(pkgs.join("numpy-2.0-py312_0")).unwrap();
    let partial = stale_file(&blobs, "abc123.incomplete");
    let done = stale_file(&blobs, "def456");
    let tarball = stale_file(&pkgs, "numpy-2.0-py312_0.conda");
    let out = reap_cmd(
        &sb,
        &[
            "reap",
            "-n",
            "--json",
            "--older-than=1h",
            "-p",
            "huggingface",
            "-p",
            "mamba",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    let j = stdout_of(&out);
    assert!(j.contains(&json_path(&partial)), "{j}");
    assert!(j.contains(&json_path(&tarball)), "{j}");
    assert!(!j.contains(&json_path(&done)), "{j}");
    assert!(!j.contains("numpy-2.0-py312_0\""), "{j}");
}

/// The ollama profile takes a stale partial blob and leaves a finished
/// one, playwright takes every browser build but the newest of each, and
/// pip-build takes pip's scratch dirs in the temp dir.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn reap_profiles_take_ollama_partials_old_browsers_and_pip_scratch() {
    let sb = Sandbox::new("reap-ollama-pw-pip");
    let blobs = sb.root.join(".ollama/models/blobs");
    fs::create_dir_all(&blobs).unwrap();
    let partial = stale_file(&blobs, "sha256-ab12-partial-3");
    let done = stale_file(&blobs, "sha256-cd34");
    let pw = sb.root.join("pw");
    for d in [
        "chromium-1140",
        "chromium-1148",
        "firefox-1466",
        "ffmpeg-1010",
    ] {
        fs::create_dir_all(pw.join(d)).unwrap();
        stale_file(&pw.join(d), "INSTALLATION_COMPLETE");
    }
    let tmp = sb.root.join("tmp");
    fs::create_dir_all(tmp.join("pip-req-build-x1")).unwrap();
    stale_file(&tmp.join("pip-req-build-x1"), "setup.py");
    let out = reap_cmd(
        &sb,
        &[
            "reap",
            "-n",
            "--json",
            "--older-than=1h",
            "-p",
            "ollama",
            "-p",
            "playwright",
            "-p",
            "pip-build",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    let j = stdout_of(&out);
    assert!(j.contains(&json_path(&partial)), "{j}");
    assert!(!j.contains(&json_path(&done)), "{j}");
    assert!(j.contains(&json_path(&pw.join("chromium-1140"))), "{j}");
    for kept in ["chromium-1148", "firefox-1466", "ffmpeg-1010"] {
        assert!(!j.contains(&json_path(&pw.join(kept))), "{j}");
    }
    assert!(j.contains(&json_path(&tmp.join("pip-req-build-x1"))), "{j}");
}

/// The pip-build profile keeps pip's scratch dirs while a process named
/// pip works in the temp dir.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn reap_pip_build_waits_for_a_running_pip() {
    let sb = Sandbox::new("reap-pip-busy");
    let tmp = sb.root.join("tmp");
    fs::create_dir_all(tmp.join("pip-install-ab12")).unwrap();
    stale_file(&tmp.join("pip-install-ab12"), "setup.py");
    age_tree(&tmp.join("pip-install-ab12"));
    let mut child = spawn_as(&sb.root.join("bin/pip"), &tmp);
    std::thread::sleep(std::time::Duration::from_millis(200));
    let out = reap_cmd(
        &sb,
        &[
            "reap",
            "--json",
            "--age-by=m",
            "--older-than=1h",
            "-p",
            "pip-build",
        ],
    );
    let _ = child.kill();
    let _ = child.wait();
    assert!(out.status.success(), "{}", stderr_of(&out));
    let j = stdout_of(&out);
    assert!(
        j.contains("is busy with pid") && j.contains("(pip, cwd)"),
        "{j}"
    );
    assert!(tmp.join("pip-install-ab12").exists());
}

/// The puppeteer profile compares dotted versions and keeps the newest
/// build of each browser.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn reap_puppeteer_keeps_the_newest_dotted_version() {
    let sb = Sandbox::new("reap-puppeteer");
    let chrome = sb.root.join(".cache/puppeteer/chrome");
    for d in ["linux-131.0.6778.85", "linux-131.0.6778.204"] {
        fs::create_dir_all(chrome.join(d)).unwrap();
        stale_file(&chrome.join(d), "chrome");
    }
    let out = reap_cmd(
        &sb,
        &["reap", "-n", "--json", "--older-than=1h", "-p", "puppeteer"],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    let j = stdout_of(&out);
    assert!(
        j.contains(&json_path(&chrome.join("linux-131.0.6778.85"))),
        "{j}"
    );
    assert!(
        !j.contains(&json_path(&chrome.join("linux-131.0.6778.204"))),
        "{j}"
    );
}

/// A user tmpfiles.d file gives a directory its age and exclusions, with
/// %C read as the user's cache directory.
#[cfg(target_os = "linux")]
#[test]
fn reap_reads_user_tmpfiles_with_specifiers() {
    let sb = Sandbox::new("reap-user-tmpfiles");
    let scratch = sb.root.join(".cache/scratch");
    fs::create_dir_all(&scratch).unwrap();
    let old = stale_file(&scratch, "old.bin");
    let kept = stale_file(&scratch, "keep-me.bin");
    let dir = scratch.to_string_lossy().into_owned();
    let run = |sb: &Sandbox| {
        let out = reap_cmd(sb, &["reap", "-n", "--json", "--age-by=m", &dir]);
        assert!(out.status.success(), "{}", stderr_of(&out));
        let j = stdout_of(&out);
        j.split(r#""skipped""#).next().unwrap().to_string()
    };
    // Without a tmpfiles.d line the default 10 days spare a day-old file.
    assert!(!run(&sb).contains(&json_path(&old)));
    let conf = sb.root.join(".config/user-tmpfiles.d");
    fs::create_dir_all(&conf).unwrap();
    fs::write(
        conf.join("scratch.conf"),
        "e %C/scratch - - - 12h\nx %C/scratch/keep-*\n",
    )
    .unwrap();
    let reaped = run(&sb);
    assert!(reaped.contains(&json_path(&old)), "{reaped}");
    assert!(!reaped.contains(&json_path(&kept)), "{reaped}");
}

/// A ~ age reaps inside each subdirectory and spares the entries directly
/// inside; a `m:` prefix ages by modification time alone, so a file whose
/// change time is new still goes.
#[cfg(target_os = "linux")]
#[test]
fn reap_honours_tmpfiles_age_modifiers() {
    let sb = Sandbox::new("reap-tmpfiles-mods");
    let deep = sb.root.join(".cache/deep");
    let mt = sb.root.join(".cache/mt");
    fs::create_dir_all(deep.join("sub")).unwrap();
    fs::create_dir_all(&mt).unwrap();
    let top = stale_file(&deep, "top.bin");
    let inner = stale_file(&deep.join("sub"), "old.bin");
    let by_m = stale_file(&mt, "f.bin");
    let conf = sb.root.join(".config/user-tmpfiles.d");
    fs::create_dir_all(&conf).unwrap();
    fs::write(
        conf.join("mods.conf"),
        "e %C/deep - - - ~m:12h\ne %C/mt - - - m:12h\n",
    )
    .unwrap();
    let (d, m) = (deep.to_string_lossy(), mt.to_string_lossy());
    let out = reap_cmd(&sb, &["reap", "-n", "--json", &d, &m]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let j = stdout_of(&out);
    let reaped = j.split(r#""skipped""#).next().unwrap();
    assert!(reaped.contains(&json_path(&inner)), "{j}");
    assert!(reaped.contains(&json_path(&by_m)), "{j}");
    assert!(!reaped.contains(&json_path(&top)), "{j}");
    // --age-by overrides the line's letters: ctime is new, so f.bin stays.
    let out = reap_cmd(&sb, &["reap", "-n", "--json", "--age-by=c", &m]);
    let j = stdout_of(&out);
    let reaped = j.split(r#""skipped""#).next().unwrap();
    assert!(!reaped.contains(&json_path(&by_m)), "{j}");
}

/// Without a DIR, a glob age line under a temp dir makes each match a
/// root with that age, and the temp dir's own pass leaves the match alone.
#[cfg(target_os = "linux")]
#[test]
fn reap_default_dirs_take_glob_age_lines_as_roots() {
    let sb = Sandbox::new("reap-tmpfiles-glob");
    let tmp = fs::canonicalize(sb.root.join("tmp")).unwrap_or_else(|_| {
        fs::create_dir_all(sb.root.join("tmp")).unwrap();
        fs::canonicalize(sb.root.join("tmp")).unwrap()
    });
    let job = tmp.join("job-1");
    fs::create_dir_all(&job).unwrap();
    let old = stale_file(&job, "old.bin");
    age_tree(&job);
    let conf = sb.root.join(".config/user-tmpfiles.d");
    fs::create_dir_all(&conf).unwrap();
    fs::write(conf.join("jobs.conf"), "e %T/job-* - - - 1h\n").unwrap();
    let out = reap_cmd(&sb, &["reap", "-n", "--json", "--age-by=m"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let j = stdout_of(&out);
    let reaped = j.split(r#""skipped""#).next().unwrap();
    assert!(reaped.contains(&json_path(&old)), "{j}");
    // job-1 itself is a root now, never an entry of the temp dir.
    assert!(!j.contains(&json_path(&job)), "{j}");
}

/// node-gyp takes old header downloads and leaves a fresh one.
#[cfg(target_os = "linux")]
#[test]
fn reap_profiles_take_old_node_gyp_headers() {
    let sb = Sandbox::new("reap-node-gyp");
    let gyp = sb.root.join(".cache/node-gyp");
    let old = gyp.join("16.20.2");
    let fresh = gyp.join("22.1.0");
    fs::create_dir_all(&old).unwrap();
    fs::create_dir_all(&fresh).unwrap();
    age_tree(&old);
    let out = reap_cmd(
        &sb,
        &[
            "reap",
            "-n",
            "--json",
            "--older-than=1h",
            "--age-by=m",
            "-p",
            "node-gyp",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    let j = stdout_of(&out);
    let reaped = j.split(r#""skipped""#).next().unwrap();
    assert!(reaped.contains(&json_path(&old)), "{j}");
    assert!(!reaped.contains(&json_path(&fresh)), "{j}");
}

/// torch takes a stale partial checkpoint and an old inductor cache and
/// leaves the finished checkpoint; vllm takes an old compile cache.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn reap_profiles_take_torch_partials_and_compile_caches() {
    let sb = Sandbox::new("reap-torch");
    let ck = sb.root.join(".cache/torch/hub/checkpoints");
    let vllm = sb.root.join(".cache/vllm/torch_compile_cache");
    let tmp = sb.root.join("tmp");
    for d in [&ck, &vllm, &tmp] {
        fs::create_dir_all(d).unwrap();
    }
    let partial = stale_file(&ck, "resnet50.pth.1a2b.partial");
    let done = stale_file(&ck, "resnet50.pth");
    fs::create_dir_all(tmp.join("torchinductor_me")).unwrap();
    let inductor = tmp.join("torchinductor_me");
    let compiled = vllm.join("abc123");
    fs::create_dir_all(&compiled).unwrap();
    age_tree(&inductor);
    age_tree(&compiled);
    let out = common::rtrash(&sb.root)
        .args([
            "reap",
            "-n",
            "--json",
            "--older-than=1h",
            "--age-by=m",
            "-p",
            "torch",
            "-p",
            "vllm",
        ])
        .env("TMPDIR", &tmp)
        .env_remove("TORCH_HOME")
        .env_remove("VLLM_CACHE_ROOT")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    let j = stdout_of(&out);
    for p in [&partial, &inductor, &compiled] {
        assert!(j.contains(&json_path(p)), "{} missing: {j}", p.display());
    }
    assert!(!j.contains(&json_path(&done)), "{j}");
}

/// triton and jax take old kernel and compile cache entries and leave fresh ones.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn reap_profiles_take_triton_and_jax_caches() {
    let sb = Sandbox::new("reap-triton-jax");
    let triton = sb.root.join(".triton/cache");
    let jax = sb.root.join(".cache/jax");
    for d in [&triton, &jax] {
        fs::create_dir_all(d).unwrap();
    }
    let kernel = triton.join("Zk9q");
    fs::create_dir_all(&kernel).unwrap();
    age_tree(&kernel);
    let fresh = triton.join("fresh");
    fs::create_dir_all(&fresh).unwrap();
    let compiled = stale_file(&jax, "jit_f-abc123-cache");
    let out = common::rtrash(&sb.root)
        .args([
            "reap",
            "-n",
            "--json",
            "--older-than=1h",
            "--age-by=m",
            "-p",
            "triton",
            "-p",
            "jax",
        ])
        .env_remove("TRITON_CACHE_DIR")
        .env_remove("JAX_COMPILATION_CACHE_DIR")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    let j = stdout_of(&out);
    for p in [&kernel, &compiled] {
        assert!(j.contains(&json_path(p)), "{} missing: {j}", p.display());
    }
    let reaped = j.split(r#""skipped""#).next().unwrap();
    assert!(!reaped.contains(&json_path(&fresh)), "{j}");
    // Fresh is skipped as recently touched; the age itself depends on the
    // runner's speed, so only the reason's kind is checked.
    let skipped = j.split(r#""skipped""#).nth(1).expect("skipped list");
    let at = skipped
        .find(&json_path(&fresh))
        .unwrap_or_else(|| panic!("fresh not skipped: {j}"));
    let entry = &skipped[at..skipped[at..].find('}').map_or(skipped.len(), |e| at + e)];
    assert!(entry.contains(r#""reason":"touched "#), "{entry}");
    assert!(entry.contains(" ago\""), "{entry}");
}

fn json_path(p: &Path) -> String {
    format!("\"{}\"", p.display())
}

fn write_at(root: &Path, rel: &str, body: &str) -> PathBuf {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, body).unwrap();
    p
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn reap_profile_claude_takes_stale_scratch_and_never_settings_or_secrets() {
    let sb = Sandbox::new("reap-claude");
    let home = sb.root.clone();
    fs::create_dir_all(home.join("tmp")).unwrap();
    let old_snap = write_at(&home, ".claude/shell-snapshots/snapshot-zsh-1.sh", "x");
    let old_todo = write_at(&home, ".claude/todos/a.json", "[]");
    let secret = write_at(&home, ".claude/todos/b/history.jsonl", "h");
    let settings = write_at(&home, ".claude/settings.json", "{}");
    let creds = write_at(&home, ".claude/.credentials.json", "{}");
    age_tree(&home.join(".claude"));
    let fresh = write_at(&home, ".claude/shell-snapshots/snapshot-zsh-2.sh", "y");

    let out = reap_cmd(
        &sb,
        &[
            "reap",
            "--profile",
            "claude",
            "--age-by=m",
            "--older-than=1h",
            "--dry-run",
            "--json",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    let reaped = text
        .split("\"reaped\"")
        .nth(1)
        .unwrap()
        .split("\"skipped\"")
        .next()
        .unwrap();
    assert!(
        reaped.contains("snapshot-zsh-1.sh") && reaped.contains("todos/a.json"),
        "{text}"
    );
    for kept in [
        "snapshot-zsh-2.sh",
        "todos/b",
        "settings.json",
        "credentials",
    ] {
        assert!(!reaped.contains(kept), "{kept} in {text}");
    }
    assert!(
        text.contains("looks like settings, history or a credential"),
        "{text}"
    );
    assert!(
        text.contains(r#""name":"claude","action":"trash""#),
        "{text}"
    );

    let pin = format!("--trash-dir={}", sb.trash().display());
    let out = reap_cmd(
        &sb,
        &[
            "reap",
            "-p",
            "claude",
            "--age-by=m",
            "--older-than=1h",
            &pin,
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!old_snap.exists() && !old_todo.exists());
    assert!(fresh.exists() && secret.exists() && settings.exists() && creds.exists());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn reap_profile_cargo_target_takes_idle_targets_and_keeps_busy_ones() {
    let sb = Sandbox::new("reap-cargo");
    let home = sb.root.clone();
    fs::create_dir_all(home.join("tmp")).unwrap();
    write_at(&home, "src/idle/target/.rustc_info.json", "{}");
    write_at(&home, "src/idle/target/debug/big", "0123456789");
    write_at(&home, "src/busy/target/.rustc_info.json", "{}");
    write_at(&home, "src/busy/target/debug/x", "1");
    write_at(&home, "src/notrust/target/out.bin", "1");
    age_tree(&home.join("src"));
    // A process named cargo working in the busy project.
    let mut child = spawn_as(&home.join("bin/cargo"), &home.join("src/busy"));
    std::thread::sleep(std::time::Duration::from_millis(200));

    let out = reap_cmd(
        &sb,
        &[
            "reap",
            "--profile=cargo-target",
            "--age-by=m",
            "--older-than=1h",
            "--dry-run",
        ],
    );
    let text = stdout_of(&out);
    let _ = (&text, out.status);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        text.contains("would reap") && text.contains("src/idle/target"),
        "{text}"
    );
    assert!(!text.contains("src/notrust"), "{text}");
    assert!(text.contains("cargo-target: would reap 1 entry"), "{text}");

    let out = reap_cmd(
        &sb,
        &[
            "reap",
            "--profile=cargo-target",
            "--age-by=m",
            "--older-than=1h",
            "--json",
        ],
    );
    let _ = child.kill();
    let _ = child.wait();
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(
        text.contains("is busy with pid") && text.contains("(cargo, cwd)"),
        "{text}"
    );
    assert!(!home.join("src/idle/target").exists(), "{text}");
    assert!(home.join("src/busy/target").exists());
    assert!(home.join("src/notrust/target").exists());
}

#[cfg(unix)]
#[test]
fn reap_profiles_come_from_config_and_replace_builtins() {
    let sb = Sandbox::new("reap-config");
    let home = sb.root.clone();
    fs::create_dir_all(home.join("tmp")).unwrap();
    write_at(
        &home,
        ".config/rtrash/config.toml",
        "[reap.profiles.mine]\nabout = \"my scratch\"\npaths = [\"~/scratch/*\"]\nage = \"1h\"\naction = \"permanent\"\n\n[reap.profiles.go]\nabout = \"go replaced\"\npaths = []\n",
    );
    let old = write_at(&home, "scratch/old.txt", "o");
    age_tree(&old);
    let out = reap_cmd(&sb, &["reap", "--list-profiles"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    for name in [
        "ai",
        "claude",
        "compilers",
        "cargo-target",
        "pkg",
        "pip",
        "thumbnails",
    ] {
        assert!(
            text.lines().any(|l| l.starts_with(name)),
            "{name} missing:\n{text}"
        );
    }
    assert!(
        text.contains("my scratch (config)") && text.contains("go replaced (config)"),
        "{text}"
    );
    let out = reap_cmd(&sb, &["reap", "--list-profiles", "--json"]);
    assert!(stdout_of(&out)
        .contains(r#""name":"mine","about":"my scratch","age":3600,"action":"permanent""#));

    let out = reap_cmd(&sb, &["reap", "-p", "mine", "--age-by=m"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("mine: reaped 1 entry"),
        "{}",
        stdout_of(&out)
    );
    assert!(!old.exists());
    let out = reap_cmd(&sb, &["reap", "-p", "nope"]);
    assert_eq!(out.status.code(), Some(2));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn reap_roots_find_cargo_targets_outside_home_to_the_set_depth() {
    let sb = Sandbox::new("reap-roots");
    let home = sb.root.join("home");
    let ws = sb.root.join("ws");
    fs::create_dir_all(home.join("tmp")).unwrap();
    write_at(&ws, "p1/target/.rustc_info.json", "{}");
    write_at(&ws, "a/b/c/d/e/p2/target/.rustc_info.json", "{}");
    age_tree(&ws);
    let run = |extra: &[&str], env_roots: Option<&Path>| {
        let mut c = common::rtrash(&home);
        c.args([
            "reap",
            "-p",
            "cargo-target",
            "--age-by=m",
            "--older-than=1h",
            "--dry-run",
            "--json",
        ])
        .args(extra)
        .env("TMPDIR", home.join("tmp"))
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("RTRASH_REAP_ROOTS");
        if let Some(r) = env_roots {
            c.env("RTRASH_REAP_ROOTS", r);
        }
        let out = c.output().unwrap();
        assert!(out.status.success(), "{}", stderr_of(&out));
        stdout_of(&out)
    };
    let roots = format!("--roots={}", ws.display());
    let text = run(&[], None);
    assert!(!text.contains("p1/target"), "{text}");
    let text = run(&[&roots], None);
    assert!(
        text.contains("p1/target") && !text.contains("p2/target"),
        "{text}"
    );
    let text = run(&[&roots, "--depth=8"], None);
    assert!(
        text.contains("p1/target") && text.contains("p2/target"),
        "{text}"
    );
    let text = run(&[], Some(&ws));
    assert!(text.contains("p1/target"), "{text}");
}

/// Run a copy of sleep under the file name of `as_exe`, in `cwd`.
/// A fork elsewhere in the test binary can briefly hold the fresh copy
/// open for writing, so spawning retries while it is busy.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn spawn_as(as_exe: &Path, cwd: &Path) -> std::process::Child {
    fs::create_dir_all(as_exe.parent().unwrap()).unwrap();
    let sleep = ["/bin/sleep", "/usr/bin/sleep"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .unwrap();
    fs::copy(sleep, as_exe).unwrap();
    for _ in 0..100 {
        match Command::new(as_exe).arg("60").current_dir(cwd).spawn() {
            Ok(c) => return c,
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            Err(e) => panic!("cannot run {}: {e}", as_exe.display()),
        }
    }
    panic!("{} stayed busy", as_exe.display());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn reap_busy_names_match_beyond_the_short_command_name() {
    let sb = Sandbox::new("reap-busy-name");
    let home = sb.root.clone();
    fs::create_dir_all(home.join("tmp")).unwrap();
    write_at(
        &home,
        ".config/rtrash/config.toml",
        "[reap.profiles.longname]\npaths = [\"~/proj/out\"]\nmarker = \"stamp\"\nbusy = [\"a-very-long-builder-name\"]\nage = \"1h\"\n",
    );
    write_at(&home, "proj/out/stamp", "s");
    age_tree(&home.join("proj"));
    // Linux cuts the command name to 15 bytes; the exe name is whole.
    let mut child = spawn_as(
        &home.join("bin/a-very-long-builder-name"),
        &home.join("proj"),
    );
    std::thread::sleep(std::time::Duration::from_millis(200));
    let out = reap_cmd(
        &sb,
        &[
            "reap",
            "-p",
            "longname",
            "--age-by=m",
            "--dry-run",
            "--json",
        ],
    );
    let _ = child.kill();
    let _ = child.wait();
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(text.contains("is busy with pid"), "{text}");
    let out = reap_cmd(
        &sb,
        &[
            "reap",
            "-p",
            "longname",
            "--age-by=m",
            "--dry-run",
            "--json",
        ],
    );
    assert!(!stdout_of(&out).contains("is busy"), "{}", stdout_of(&out));
}

/// Under a clock as coarse as the test seam makes it, every change falls
/// in one tick, so a stored total stays "current" by its stamp however
/// much the trash changes. Each command must still leave the history
/// with the right item count. Debug builds only.
#[cfg(debug_assertions)]
#[test]
fn history_counts_stay_right_when_every_change_shares_a_clock_tick() {
    let sb = Sandbox::new("coarse-tick");
    // About eleven days a tick: the whole test runs in one.
    let run = |args: &[&str]| {
        common::rtrash(&sb.root)
            .args(args)
            .current_dir(sb.work())
            .env("RTRASH_TEST_STAMP_TICK", "1000000000000000")
            .output()
            .unwrap()
    };
    let last_items = || -> u64 {
        let h = fs::read_to_string(sb.trash().join("rtrash-history")).unwrap();
        h.lines()
            .last()
            .unwrap()
            .rsplit(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    let file = |n: &str| {
        let p = sb.work().join(n);
        fs::write(&p, n).unwrap();
        p
    };
    let (a, b, c, d) = (file("a"), file("b"), file("c"), file("d"));
    for p in [&a, &b, &c, &d] {
        assert!(run(&["put", p.to_str().unwrap()]).status.success());
    }
    assert_eq!(last_items(), 4, "put");
    let out = run(&["restore", "--home-only", d.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(last_items(), 3, "restore");
    let out = run(&["rm", "--home-only", c.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(last_items(), 2, "rm");
    // An empty by age takes b once its record is dated back.
    let info = sb.trash().join("info/b.trashinfo");
    let body = fs::read_to_string(&info).unwrap();
    let dated: Vec<String> = body
        .lines()
        .map(|l| match l.starts_with("DeletionDate=") {
            true => "DeletionDate=2000-01-01T00:00:00".to_string(),
            false => l.to_string(),
        })
        .collect();
    fs::write(&info, dated.join("\n") + "\n").unwrap();
    let out = run(&["empty", "--home-only", "30"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!info.exists());
    assert_eq!(last_items(), 1, "empty by age");
    let json = stdout_of(&run(&["trash-stats", "--home-only", "--json"]));
    assert!(json.contains(r#""items":1,"#), "{json}");
    // A full empty that fails partway: the seam keeps e, the rest goes,
    // and the history counts the one item left.
    let e = file("e");
    assert!(run(&["put", e.to_str().unwrap()]).status.success());
    assert_eq!(last_items(), 2, "put e");
    let out = common::rtrash(&sb.root)
        .args(["empty", "--home-only", "--all"])
        .current_dir(sb.work())
        .env("RTRASH_TEST_STAMP_TICK", "1000000000000000")
        .env("RTRASH_TEST_FAIL_DELETE", "e")
        .output()
        .unwrap();
    assert!(!out.status.success(), "the seam did not refuse e");
    assert!(sb.trash().join("info/e.trashinfo").exists());
    assert!(!sb.trash().join("info/a.trashinfo").exists());
    assert_eq!(last_items(), 1, "failed full empty");
}

/// `empty --dangling` removes only the records whose payload is gone:
/// real items, orphan payloads and names that are not UTF-8 all stay.
#[test]
fn list_and_status_flag_records_without_a_payload() {
    let sb = Sandbox::new("missing");
    let keep = sb.work().join("keep.txt");
    fs::write(&keep, "k").unwrap();
    assert!(sb.run(&["put", keep.to_str().unwrap()]).status.success());
    fs::write(
        sb.trash().join("info/ghost.trashinfo"),
        "[Trash Info]\nPath=/tmp/ghost\nDeletionDate=2020-01-01T00:00:00\n",
    )
    .unwrap();
    let out = sb.run(&["list", "--home-only", "--json"]);
    let list = stdout_of(&out);
    let line = |name: &str| list.lines().find(|l| l.contains(name)).unwrap().to_string();
    assert!(line("ghost").contains(r#""missing":true"#), "{list}");
    assert!(line("keep.txt").contains(r#""missing":false"#), "{list}");
    let out = sb.run(&["status", "--home-only"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("2 items (1 without a payload)"),
        "{}",
        stdout_of(&out)
    );
    assert!(
        stderr_of(&out).contains("1 record cannot be restored")
            && stderr_of(&out).contains("rtrash empty --dangling"),
        "{}",
        stderr_of(&out)
    );
    let out = sb.run(&["status", "--home-only", "--json"]);
    assert!(
        stdout_of(&out).contains(r#""dangling":1"#),
        "{}",
        stdout_of(&out)
    );
    // The numbered picker marks the row it cannot restore; an empty
    // answer restores nothing.
    let out = sb.run_with_stdin(&["restore", "--home-only", "--plain"], "\n");
    let rows = stdout_of(&out);
    let row = |name: &str| rows.lines().find(|l| l.contains(name)).unwrap().to_string();
    assert!(row("ghost").ends_with("[payload gone]"), "{rows}");
    assert!(!row("keep.txt").contains("payload gone"), "{rows}");
    assert!(sb
        .run(&["empty", "--home-only", "--dangling"])
        .status
        .success());
    let out = sb.run(&["status", "--home-only"]);
    assert!(
        !stderr_of(&out).contains("--dangling"),
        "{}",
        stderr_of(&out)
    );
    assert!(!stdout_of(&out).contains("payload"), "{}", stdout_of(&out));
}

#[test]
fn empty_dangling_removes_only_records_without_a_payload() {
    use std::os::unix::ffi::OsStrExt;
    let sb = Sandbox::new("dangling");
    let keep = sb.work().join("keep.txt");
    fs::write(&keep, "k").unwrap();
    assert!(sb.run(&["put", keep.to_str().unwrap()]).status.success());
    let info = sb.trash().join("info");
    let files = sb.trash().join("files");
    let record = |name: &[u8]| {
        let mut p = name.to_vec();
        p.extend_from_slice(b".trashinfo");
        let path = info.join(std::ffi::OsStr::from_bytes(&p));
        fs::write(
            &path,
            "[Trash Info]\nPath=/tmp/x\nDeletionDate=2020-01-01T00:00:00\n",
        )
        .unwrap();
        path
    };
    let ghost = record(b"ghost");
    // A name that is not UTF-8, with its payload: a real item. macOS
    // file systems take only UTF-8 names.
    let odd = cfg!(target_os = "linux").then(|| {
        let r = record(b"odd\xff");
        fs::write(files.join(std::ffi::OsStr::from_bytes(b"odd\xff")), "o").unwrap();
        r
    });
    // An orphan payload with no record stays too.
    fs::write(files.join("orphan"), "o").unwrap();
    let out = sb.run(&["empty", "--home-only", "--dangling", "-n"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(stdout_of(&out).lines().count(), 1, "{}", stdout_of(&out));
    assert!(stdout_of(&out).contains("ghost.trashinfo"));
    assert!(ghost.exists(), "the dry run removed it");
    let out = sb.run(&["empty", "--home-only", "--dangling", "--json"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains(r#""removed":1"#),
        "{}",
        stdout_of(&out)
    );
    assert!(!ghost.exists() && odd.is_none_or(|p| p.exists()));
    assert!(info.join("keep.txt.trashinfo").exists() && files.join("orphan").exists());
    assert_eq!(
        sb.run(&["empty", "--home-only", "--dangling", "3"])
            .status
            .code(),
        Some(2)
    );
}

/// `empty --dangling` keeps the limits `status --check --fix` keeps for
/// the same records: `RTRASH_NO_PERMANENT`, `RTRASH_MAX_PERMANENT` (and
/// `--max-count`) and `RTRASH_ALLOW_ROOTS` each refuse the run whole with
/// exit 2, the same lines as the fix and nothing removed; a dry run
/// still lists; within the limits the records go and the item stays.
#[test]
fn empty_dangling_keeps_the_limits_status_fix_keeps() {
    let sb = Sandbox::new("dangling-limits");
    sb.touch("dl-keep");
    assert!(sb.run(&["put", "dl-keep"]).status.success());
    let (info, files) = (sb.trash().join("info"), sb.trash().join("files"));
    let work = sb.work().display().to_string();
    let record = |name: &str, origin: &str| {
        fs::write(
            info.join(format!("{name}.trashinfo")),
            format!("[Trash Info]\nPath={origin}\nDeletionDate=2026-01-02T03:04:05\n"),
        )
        .unwrap();
    };
    record("dl-in", &format!("{work}/dl-in"));
    record("dl-out", "/rtrash-elsewhere/dl-out");
    let watch = [info.as_path(), files.as_path()];
    let empty = ["empty", "--home-only", "--dangling"];
    let fix = ["status", "--home-only", "--check", "--fix"];
    // Each line without its `PROG: ` head, so the two commands compare.
    let tail = |out: &Output| -> Vec<String> {
        stderr_of(out)
            .lines()
            .map(|l| l.split_once(": ").map_or(l, |(_, t)| t).to_string())
            .collect()
    };
    let refusals: [(&[(&str, &str)], &str); 3] = [
        (
            &[("RTRASH_NO_PERMANENT", "1")],
            "put, list and restore still work",
        ),
        (
            &[("RTRASH_MAX_PERMANENT", "1")],
            "nothing was deleted, and -n lists them",
        ),
        (
            &[("RTRASH_ALLOW_ROOTS", work.as_str())],
            "nothing was changed",
        ),
    ];
    for (envs, last) in refusals {
        let label = (&empty, envs);
        let out = common::assert_refused_unchanged(&label, &watch, 2, &[(last, 1)], || {
            sb.run_env(&empty, envs)
        });
        let same = common::assert_refused_unchanged(&(&fix, envs), &watch, 2, &[(last, 1)], || {
            sb.run_env(&fix, envs)
        });
        assert_eq!(tail(&out), tail(&same), "{envs:?}");
    }
    let out = common::assert_refused_unchanged(&"scope", &watch, 2, &[], || {
        sb.run_env(&empty, &[("RTRASH_ALLOW_ROOTS", work.as_str())])
    });
    assert_eq!(common::outside_lines(&out), 1, "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("rtrash-elsewhere"),
        "{}",
        stderr_of(&out)
    );
    let capped = ["empty", "--home-only", "--dangling", "--max-count=1"];
    common::assert_refused_unchanged(&capped, &watch, 2, &[("-n lists them", 1)], || {
        sb.run(&capped)
    });
    let dry = ["empty", "--home-only", "--dangling", "-n"];
    let out = common::assert_refused_unchanged(&dry, &watch, 0, &[], || {
        sb.run_env(&dry, &[("RTRASH_NO_PERMANENT", "1")])
    });
    assert_eq!(stdout_of(&out).lines().count(), 2, "{}", stdout_of(&out));

    // Within the limits: the record outside the root is gone by hand, and
    // the one left fits both the root and a cap of 1.
    fs::remove_file(info.join("dl-out.trashinfo")).unwrap();
    let envs = [
        ("RTRASH_ALLOW_ROOTS", work.as_str()),
        ("RTRASH_MAX_PERMANENT", "1"),
    ];
    let out = sb.run_env(&empty, &envs);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    assert!(!info.join("dl-in.trashinfo").exists());
    assert!(info.join("dl-keep.trashinfo").exists() && files.join("dl-keep").exists());
}

/// `put -v` names the trash directory each item went to.
#[cfg(unix)]
#[test]
fn put_verbose_names_the_trash_directory() {
    let sb = Sandbox::new("put-v-trash");
    fs::create_dir_all(sb.work().join("d")).unwrap();
    fs::write(sb.work().join("f.txt"), "x").unwrap();
    let out = sb.run(&["put", "-v", "-r", "--home-only", "f.txt", "d"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    let to = sb.trash().display().to_string();
    assert!(
        text.contains(&format!("removed 'f.txt' (trash {to})")),
        "{text}"
    );
    assert!(
        text.contains(&format!("removed directory 'd' (trash {to})")),
        "{text}"
    );
}

/// A put onto a file system short of space says that it freed nothing.
#[cfg(all(unix, debug_assertions))]
#[test]
fn put_warns_when_the_trash_file_system_is_low() {
    let sb = Sandbox::new("put-low");
    let f = write_at(&sb.root, "work/big.bin", &"b".repeat(90_000));
    let out = common::rtrash(&sb.root)
        .args(["put", "--home-only", f.to_str().unwrap()])
        .env_remove("RTRASH_WARN_SIZE")
        .env_remove("RTRASH_MAX_SIZE")
        .env("RTRASH_TEST_FS", format!("100000:{}", sb.root.display()))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(
        err.contains("is low (")
            && err.contains("trashing frees nothing")
            && err.contains("empty --until-free=10%"),
        "{err}"
    );
    let quiet = write_at(&sb.root, "work/ok.bin", "x");
    let out = sb.run(&["put", "--home-only", quiet.to_str().unwrap()]);
    assert!(!stderr_of(&out).contains("is low"), "{}", stderr_of(&out));
}

#[test]
fn an_unscoped_empty_needs_all_for_items_other_puts_left() {
    let sb = Sandbox::new("emptyguard");
    for n in ["a", "b"] {
        fs::write(sb.work().join(n), b"x").unwrap();
        assert!(sb.run(&["put", n]).status.success());
    }
    // b's run is the latest; a came from another one.
    let out = sb.run(&["empty", "--home-only"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains("--all"), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("rm --batch=last"),
        "{}",
        stderr_of(&out)
    );
    assert!(sb.trash().join("files/a").exists() && sb.trash().join("files/b").exists());
    // Scoped or dry runs go ahead.
    assert!(sb.run(&["empty", "--home-only", "-n"]).status.success());
    assert!(sb.run(&["empty", "--home-only", "--all"]).status.success());
    assert!(!sb.trash().join("files/a").exists());
    // Only the latest put's items: no --all needed.
    fs::write(sb.work().join("c"), b"x").unwrap();
    assert!(sb.run(&["put", "c"]).status.success());
    assert!(sb.run(&["empty", "--home-only"]).status.success());
    assert!(!sb.trash().join("files/c").exists());
}

#[test]
fn rm_batch_removes_one_put_run() {
    let sb = Sandbox::new("rmbatch");
    for n in ["a", "b", "c"] {
        fs::write(sb.work().join(n), b"x").unwrap();
    }
    assert!(sb.run(&["put", "a"]).status.success());
    assert!(sb.run(&["put", "b", "c"]).status.success());
    let out = sb.run(&["rm", "--home-only", "--batch=last", "-n"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(sb.trash().join("files/b").exists());
    let out = sb.run(&["rm", "--home-only", "--batch=last"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!sb.trash().join("files/b").exists() && !sb.trash().join("files/c").exists());
    assert!(sb.trash().join("files/a").exists());
    // A PATTERN narrows the run; an unknown run removes nothing.
    let out = sb.run(&["rm", "--home-only", "--batch=nope"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(sb.trash().join("files/a").exists());
}

#[test]
fn list_filters_by_glob_regex_name_and_cwd() {
    let sb = Sandbox::new("listmatch");
    fs::create_dir_all(sb.work().join("sub")).unwrap();
    for n in ["a.log", "b.txt", "sub/c.log"] {
        fs::write(sb.work().join(n), b"x").unwrap();
        assert!(sb.run(&["put", n]).status.success());
    }
    let lines = |args: &[&str]| -> Vec<String> {
        let mut a = vec!["list", "--home-only"];
        a.extend_from_slice(args);
        let out = sb.run(&a);
        assert!(out.status.success(), "{}", stderr_of(&out));
        stdout_of(&out).lines().map(str::to_string).collect()
    };
    let got = lines(&["--glob=*.log"]);
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got.iter().all(|l| l.ends_with(".log")));
    let got = lines(&["--regex=/sub/"]);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].ends_with("sub/c.log"));
    assert_eq!(lines(&["--name=b.txt"]).len(), 1);
    assert_eq!(lines(&["--cwd-only"]).len(), 3);
    // Two matchers at once are refused.
    let out = sb.run(&["list", "--home-only", "--glob=*", "--name=x"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn bare_patterns_filter_list_and_glob_restore() {
    let sb = Sandbox::new("barepat");
    for n in ["report.log", "notes.txt", "old.log"] {
        fs::write(sb.work().join(n), b"x").unwrap();
        assert!(sb.run(&["put", n]).status.success());
    }
    let lines = |pat: &str| -> Vec<String> {
        let out = sb.run(&["list", "--home-only", pat]);
        assert!(out.status.success(), "{}", stderr_of(&out));
        stdout_of(&out).lines().map(str::to_string).collect()
    };
    // No glob character: a piece of the name.
    assert_eq!(lines("port").len(), 1);
    assert_eq!(lines("*.log").len(), 2);
    assert!(lines("nothing").is_empty());
    let out = sb.run(&["list", "--home-only", "port", "--name=x"]);
    assert_eq!(out.status.code(), Some(2));
    // A quoted glob restores like --glob.
    let out = sb.run(&["restore", "--home-only", "*.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(sb.work().join("notes.txt").exists());
}

#[test]
fn list_format_prints_fields_through_a_template() {
    let sb = Sandbox::new("listfmt");
    fs::write(sb.work().join("a b.txt"), b"xyz").unwrap();
    assert!(sb.run(&["put", "a b.txt"]).status.success());
    let out = sb.run(&[
        "list",
        "--home-only",
        "--format={name}\\t{size}\\t{path}\\0",
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let o = stdout_of(&out);
    let row = o.strip_suffix('\0').expect("NUL ends the row");
    let cols: Vec<&str> = row.split('\t').collect();
    assert_eq!(cols[0], "a b.txt");
    assert!(cols[1].parse::<u64>().unwrap() >= 3);
    assert!(cols[2].ends_with("/a b.txt"));
    let out = sb.run(&["list", "--home-only", "--format={{{epoch}}}"]);
    assert!(stdout_of(&out).starts_with('{') && stdout_of(&out).ends_with('}'));
    for bad in ["--format={nope}", "--format=}"] {
        assert_eq!(sb.run(&["list", bad]).status.code(), Some(2), "{bad}");
    }
    let out = sb.run(&["list", "--json", "--format={path}"]);
    assert_eq!(out.status.code(), Some(2));
}

// macOS empties the Finder Trash here; tests/finder_default.rs covers it.
#[cfg(not(target_os = "macos"))]
#[test]
fn verbose_empty_names_original_paths() {
    let sb = Sandbox::new("emptyv");
    for n in ["a.txt", "b.txt"] {
        fs::write(sb.work().join(n), b"x").unwrap();
        assert!(sb.run(&["put", n]).status.success());
    }
    let want = sb.work().join("a.txt");
    let want = fs::canonicalize(want.parent().unwrap())
        .unwrap()
        .join("a.txt");
    for args in [
        &["empty", "--home-only", "--all", "-n", "-v"][..],
        &["empty", "--home-only", "--all", "-v"][..],
        &["empty", "--home-only", "-v"][..],
    ] {
        if args.len() == 3 {
            fs::write(sb.work().join("a.txt"), b"x").unwrap();
            assert!(sb.run(&["put", "a.txt"]).status.success());
        }
        let out = sb.run(args);
        assert!(out.status.success(), "{args:?}: {}", stderr_of(&out));
        let o = stdout_of(&out);
        assert!(
            o.lines().any(|l| l.contains(&*want.to_string_lossy())),
            "{args:?}: {o}"
        );
        assert!(!o.contains("/files/"), "{args:?}: {o}");
    }
}

#[cfg(not(target_os = "macos"))]
#[test]
fn restore_batch_brings_back_one_put_run() {
    let sb = Sandbox::new("restbatch");
    fs::write(sb.work().join("old.txt"), b"o").unwrap();
    assert!(sb.run(&["put", "old.txt"]).status.success());
    for n in ["a.txt", "b.txt"] {
        fs::write(sb.work().join(n), b"x").unwrap();
    }
    assert!(sb.run(&["put", "a.txt", "b.txt"]).status.success());
    let out = sb.run(&["restore", "--home-only", "--batch=last"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(sb.work().join("a.txt").exists() && sb.work().join("b.txt").exists());
    assert!(!sb.work().join("old.txt").exists());
    // Options undo does not share are refused.
    let out = sb.run(&["restore", "--batch=last", "--to=/tmp"]);
    assert_eq!(out.status.code(), Some(2));
    let out = sb.run(&["restore", "--batch="]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn put_dry_run_checks_and_moves_nothing() {
    let sb = Sandbox::new("putdry");
    fs::write(sb.work().join("a"), b"x").unwrap();
    let out = sb.run(&["put", "--dry-run", "a", "missing"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("would trash"),
        "{}",
        stdout_of(&out)
    );
    assert!(stderr_of(&out).contains("missing"));
    assert!(sb.work().join("a").exists());
    assert!(!sb.trash().join("files/a").exists());
}

#[test]
fn next_trash_second_waits_for_a_later_second() {
    let before = common::epoch_secs();
    assert!(common::next_trash_second() > before);
    assert!(common::epoch_secs() > before);
    // A file changed now carries a later second too, whatever the lag of
    // the clock the file system stamps it with.
    assert!(common::fs_clock_secs() > before);

    // It waits for the next tick only, with no fixed wait on top: on a
    // clock that reaches second 8 at its third reading, it naps twice
    // and returns 8. Counting naps, not wall time, holds on a loaded
    // runner: a 1.5 s bound on the elapsed time failed on a busy macOS
    // runner in round 215.
    let readings = [7, 7, 8, 9];
    let mut next = readings.iter().copied();
    let mut naps = 0;
    let got = common::wait_for_later_second(7, || next.next().unwrap(), || naps += 1);
    assert_eq!((got, naps), (8, 2));
    // A clock that never moves ends in a panic, not an endless loop.
    let mut stuck = 0;
    let gave_up = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        common::wait_for_later_second(7, || 7, || stuck += 1)
    }));
    assert!(gave_up.is_err());
    assert_eq!(stuck, common::SECOND_POLLS);
}

#[test]
fn list_show_trashpath_names_the_payload_under_files() {
    let sb = Sandbox::new("list-trashpath");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("shown.txt");
    assert!(sb.run(&["put", "shown.txt"]).status.success());
    let out = sb.run(&["list", &pin, "--show-trashpath"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    let lines: Vec<&str> = text.lines().collect();
    // One row, then the line naming its payload.
    assert_eq!(lines.len(), 2, "{text}");
    let at = lines[1].strip_prefix("  trash: ").expect("trash line");
    assert!(at.ends_with("files/shown.txt"), "{text}");
    assert!(Path::new(at).exists(), "{text}");
    let json = stdout_of(&sb.run(&["list", &pin, "--json", "--show-trashpath"]));
    assert!(json.contains("\"trash_path\":\""), "{json}");
    assert!(json.contains("files/shown.txt\""), "{json}");
    let plain = stdout_of(&sb.run(&["list", &pin]));
    assert!(!plain.contains("trash: "), "{plain}");
    let fmt = sb.run(&["list", &pin, "--show-trashpath", "--format={name}"]);
    assert_eq!(fmt.status.code(), Some(2));
}

#[test]
fn no_permanent_guard_refuses_rm_and_empty_but_not_dry_runs() {
    let sb = Sandbox::new("no-permanent");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("keep.txt");
    assert!(sb.run(&["put", "keep.txt"]).status.success());
    let guarded = |args: &[&str]| {
        common::rtrash(&sb.root)
            .args(args)
            .current_dir(sb.work())
            .env("RTRASH_NO_PERMANENT", "1")
            .output()
            .expect("run rtrash")
    };
    for args in [
        &["rm", &pin, "keep.txt"][..],
        &["rm", &pin, "-f", "*"],
        &["empty", &pin, "--all"],
    ] {
        let out = guarded(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(stderr_of(&out).contains("RTRASH_NO_PERMANENT"), "{args:?}");
    }
    // Nothing went: the item is still listed, and still restorable.
    let listed = stdout_of(&sb.run(&["list", &pin]));
    assert!(listed.contains("keep.txt"), "{listed}");
    // A dry run only looks, so the guard lets it through.
    let dry = guarded(&["rm", &pin, "-n", "keep.txt"]);
    assert!(dry.status.success(), "{}", stderr_of(&dry));
    // "0" leaves the guard off.
    let off = common::rtrash(&sb.root)
        .args(["rm", &pin, "keep.txt"])
        .current_dir(sb.work())
        .env("RTRASH_NO_PERMANENT", "0")
        .output()
        .unwrap();
    assert!(off.status.success(), "{}", stderr_of(&off));
    let listed = stdout_of(&sb.run(&["list", &pin]));
    assert!(!listed.contains("keep.txt"), "{listed}");
}

/// Under `RTRASH_NO_PERMANENT` the size cap after a put keeps every older
/// item: the put itself still moves the file.
#[test]
fn no_permanent_guard_keeps_items_the_put_cap_would_drop() {
    let sb = Sandbox::new("noperm-putcap");
    trash_aged(&sb, "old1", 40_000, 20);
    trash_aged(&sb, "old2", 40_000, 10);
    fs::write(sb.work().join("fresh"), vec![b'y'; 150_000]).unwrap();
    let out = common::rtrash(&sb.root)
        .args(["put", "fresh"])
        .current_dir(sb.work())
        .env("RTRASH_MAX_SIZE", "100K")
        .env("RTRASH_NO_PERMANENT", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(names_in_trash(&sb), ["fresh", "old1", "old2"]);
    assert!(
        stderr_of(&out).contains("RTRASH_NO_PERMANENT"),
        "{}",
        stderr_of(&out)
    );
}

/// A restore over an existing file would delete that file for good, so
/// the guard refuses it and both copies stay.
#[test]
fn no_permanent_guard_refuses_a_restore_that_overwrites() {
    let sb = Sandbox::new("noperm-restore");
    fs::write(sb.work().join("pg.txt"), b"trashed").unwrap();
    assert!(sb.run(&["put", "pg.txt"]).status.success());
    fs::write(sb.work().join("pg.txt"), b"newer").unwrap();
    let out = sb.run_guarded(&["restore", "--home-only", "-f", "pg.txt"]);
    assert!(!out.status.success(), "{}", stdout_of(&out));
    assert!(
        stderr_of(&out).contains("RTRASH_NO_PERMANENT"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(fs::read(sb.work().join("pg.txt")).unwrap(), b"newer");
    assert_eq!(names_in_trash(&sb), ["pg.txt"]);
    // A restore with nothing in the way still works.
    fs::remove_file(sb.work().join("pg.txt")).unwrap();
    let out = sb.run_guarded(&["restore", "--home-only", "pg.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(sb.work().join("pg.txt")).unwrap(), b"trashed");
}

/// reap deletes for good with `--permanent` and `--purge-trash`; the
/// guard stops both, and a plain reap (to the trash) still runs.
#[test]
fn no_permanent_guard_refuses_reap_permanent_and_purge_trash() {
    let sb = Sandbox::new("noperm-reap");
    let dir = sb.work().join("scratch");
    fs::create_dir_all(&dir).unwrap();
    let old = dir.join("old.txt");
    fs::write(&old, b"o").unwrap();
    let day_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400);
    let times = fs::FileTimes::new()
        .set_accessed(day_ago)
        .set_modified(day_ago);
    fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_times(times)
        .unwrap();
    let d = dir.to_str().unwrap();
    let pin = format!("--trash-dir={}", sb.trash().display());
    let age = ["--older-than=1h", "--age-by=m"];
    let permanent = [&["reap"][..], &age[..], &["--permanent", d][..]].concat();
    let purge = [
        &["reap"][..],
        &age[..],
        &["--when-below=100%", "--purge-trash", d][..],
    ]
    .concat();
    for args in [&permanent, &purge] {
        let out = sb.run_guarded(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr_of(&out));
        assert!(stderr_of(&out).contains("RTRASH_NO_PERMANENT"), "{args:?}");
        assert!(old.exists(), "{args:?} removed the file");
    }
    let to_trash = [&["reap"][..], &age[..], &[pin.as_str(), d][..]].concat();
    let out = sb.run_guarded(&to_trash);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!old.exists());
    assert_eq!(names_in_trash(&sb), ["old.txt"]);
}

/// `--max-count` and `RTRASH_MAX_PERMANENT` refuse an rm or empty that
/// would delete more than N items, and then nothing goes.
#[test]
fn max_count_refuses_rm_and_empty_past_the_cap() {
    let sb = Sandbox::new("maxcount");
    let pin = format!("--trash-dir={}", sb.trash().display());
    for n in ["mc-a", "mc-b", "mc-c"] {
        sb.touch(n);
        assert!(sb.run(&["put", n]).status.success());
    }
    let all = ["mc-a", "mc-b", "mc-c"];
    let refused = |args: &[&str], envs: &[(&str, &str)], says: &str| {
        let out = sb.run_env(args, envs);
        assert_eq!(out.status.code(), Some(2), "{args:?} {envs:?}");
        assert!(
            stderr_of(&out).contains(says),
            "{args:?}: {}",
            stderr_of(&out)
        );
        assert_eq!(names_in_trash(&sb), all, "{args:?} deleted items");
    };
    let cap2 = [("RTRASH_MAX_PERMANENT", "2")];
    refused(
        &["rm", &pin, "--max-count=2", "mc-*"],
        &[],
        "3 items would be deleted",
    );
    refused(&["rm", &pin, "mc-*"], &cap2, "more than the cap of 2");
    refused(&["empty", &pin, "--all"], &cap2, "more than the cap of 2");
    refused(
        &["rm", &pin, "mc-a"],
        &[("RTRASH_MAX_PERMANENT", "lots")],
        "RTRASH_MAX_PERMANENT",
    );
    refused(&["rm", &pin, "--max-count=x", "mc-a"], &[], "--max-count");
    // A dry run only looks, so the cap lets it through.
    let out = sb.run_env(&["rm", &pin, "-n", "mc-*"], &cap2);
    assert!(out.status.success(), "{}", stderr_of(&out));
    // Within the cap the run goes ahead, and --max-count wins over the variable.
    let one = [("RTRASH_MAX_PERMANENT", "1")];
    let out = sb.run_env(&["rm", &pin, "mc-a"], &one);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(names_in_trash(&sb), ["mc-b", "mc-c"]);
    let out = sb.run_env(&["empty", &pin, "--all", "--max-count=2"], &one);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(names_in_trash(&sb).is_empty());
}

/// `RTRASH_ALLOW_ROOTS` keeps put, restore and rm to the listed folders,
/// sees through a symlinked folder that leads out, refuses empty, and on
/// a refusal changes nothing.
#[cfg(target_os = "linux")]
#[test]
fn allow_roots_keeps_put_restore_and_rm_inside() {
    let sb = Sandbox::new("allowroots");
    let pin = format!("--trash-dir={}", sb.trash().display());
    let (inside, outside) = (sb.work().join("ar-in"), sb.work().join("ar-out"));
    fs::create_dir_all(&inside).unwrap();
    fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, inside.join("ar-link")).unwrap();
    for f in ["ar-in/ar-a", "ar-in/ar-b", "ar-out/ar-c", "ar-out/ar-d"] {
        sb.touch(f);
    }
    // ar-c goes to the trash unscoped, so rm and restore meet an outside item.
    assert!(sb.run(&["put", "ar-out/ar-c"]).status.success());
    let scope_s = inside.display().to_string();
    let scope = [("RTRASH_ALLOW_ROOTS", scope_s.as_str())];
    // A refusal leaves info/ and files/ as they were just before it.
    let (info, files) = (sb.trash().join("info"), sb.trash().join("files"));
    let watch = [info.as_path(), files.as_path()];
    let refused = |args: &[&str], outside_paths: usize| {
        let lines = [(common::OUTSIDE, outside_paths)];
        common::assert_refused_unchanged(&args, &watch, 2, &lines, || sb.run_env(args, &scope))
    };
    // One outside operand refuses the whole put, so ar-a stays too.
    refused(&["put", "ar-in/ar-a", "ar-out/ar-d"], 1);
    // The symlinked folder resolves outside, so this path is not inside.
    refused(&["put", "ar-in/ar-link/ar-d"], 1);
    for f in ["ar-in/ar-a", "ar-out/ar-d"] {
        assert!(sb.work().join(f).exists(), "{f} moved");
    }
    refused(&["rm", &pin, "ar-c"], 1);
    refused(&["restore", "--home-only", "--name=ar-c"], 1);
    let empty = ["empty", pin.as_str(), "--all"];
    common::assert_refused_unchanged(&empty, &watch, 2, &[], || sb.run_env(&empty, &scope));
    // A dry run only looks, so it goes through.
    assert!(sb
        .run_env(&["empty", &pin, "-n", "--all"], &scope)
        .status
        .success());

    // Inside the root everything works, the link itself included.
    let out = sb.run_env(
        &["put", "ar-in/ar-a", "ar-in/ar-b", "ar-in/ar-link"],
        &scope,
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(names_in_trash(&sb), ["ar-a", "ar-b", "ar-c", "ar-link"]);
    let to_out = format!("--to={}", outside.display());
    refused(&["restore", "--home-only", &to_out, "--name=ar-a"], 1);
    let after_put = names_in_trash(&sb);
    let out = sb.run_env(&["restore", "--home-only", "--name=ar-a"], &scope);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(inside.join("ar-a").exists());
    let out = sb.run_env(&["rm", &pin, "ar-b"], &scope);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(names_in_trash(&sb).len(), after_put.len() - 2);
    // A root that does not exist allows nothing.
    let gone = [("RTRASH_ALLOW_ROOTS", "/nonexistent-ar-root")];
    let put_d = ["put", "ar-out/ar-d"];
    common::assert_refused_unchanged(&put_d, &watch, 2, &[(common::OUTSIDE, 1)], || {
        sb.run_env(&put_d, &gone)
    });
    // Unset or empty, the scope is off.
    let out = sb.run_env(&["put", "ar-out/ar-d"], &[("RTRASH_ALLOW_ROOTS", "")]);
    assert!(out.status.success(), "{}", stderr_of(&out));
}

/// The `info/` names of the sandbox trash, sorted.
fn info_names(sb: &Sandbox) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(sb.trash().join("info"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// The stdout lines of `out` that start with `prefix`.
fn lines_starting(out: &Output, prefix: &str) -> usize {
    stdout_of(out)
        .lines()
        .filter(|l| l.starts_with(prefix))
        .count()
}

/// `status --check` finds each kind of problem once, `--fix -n` and every
/// guard leave the trash as it was just before, and `--fix` removes the
/// dangling and bad records only, keeping the bad record's payload.
#[test]
fn status_check_reports_and_fix_removes_only_broken_records() {
    let sb = Sandbox::new("trashcheck");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("ck-a");
    assert!(sb.run(&["put", "ck-a"]).status.success());
    let (info, files) = (sb.trash().join("info"), sb.trash().join("files"));
    let record = |name: &str, body: &str| {
        fs::write(info.join(format!("{name}.trashinfo")), body).unwrap();
    };
    let work = sb.work().display().to_string();
    let good = |name: &str| {
        format!("[Trash Info]\nPath={work}/{name}\nDeletionDate=2026-01-02T03:04:05\n")
    };
    record("ck-gone", &good("ck-gone"));
    record("ck-bad", "[Trash Info]\nDeletionDate=2026-01-02T03:04:05\n");
    fs::write(files.join("ck-bad"), b"bad").unwrap();
    record(
        "ck-date",
        &format!("[Trash Info]\nPath={work}/ck-date\nDeletionDate=soon\n"),
    );
    fs::write(files.join("ck-date"), b"date").unwrap();
    fs::write(files.join("ck-orph"), b"orph").unwrap();
    fs::write(info.join("ck-stray.txt"), b"stray").unwrap();

    let check = sb.run(&["status", &pin, "--check"]);
    assert_eq!(check.status.code(), Some(1), "{}", stderr_of(&check));
    for prefix in [
        "dangling: ",
        "bad record: ",
        "bad date: ",
        "orphan: ",
        "stray: ",
    ] {
        assert_eq!(
            lines_starting(&check, prefix),
            1,
            "{prefix}: {}",
            stdout_of(&check)
        );
    }
    assert!(!stdout_of(&check).contains("ck-a"), "{}", stdout_of(&check));
    assert!(
        stderr_of(&check).contains("5 problems found, 2 records --fix would remove"),
        "{}",
        stderr_of(&check)
    );

    // Each of these leaves info/ and files/ as they were just before it.
    let watch = [info.as_path(), files.as_path()];
    let unchanged = |args: &[&str], envs: &[(&str, &str)], code: i32| {
        common::assert_refused_unchanged(&(args, envs), &watch, code, &[], || {
            sb.run_env(args, envs)
        })
    };
    let fix = ["status", pin.as_str(), "--check", "--fix"];
    let dry = ["status", pin.as_str(), "--check", "--fix", "-n"];
    let out = unchanged(&dry, &[], 1);
    assert_eq!(
        lines_starting(&out, "would remove "),
        2,
        "{}",
        stdout_of(&out)
    );
    let out = unchanged(&fix, &[("RTRASH_NO_PERMANENT", "1")], 2);
    assert!(
        stderr_of(&out).contains("RTRASH_NO_PERMANENT"),
        "{}",
        stderr_of(&out)
    );
    let out = unchanged(&fix, &[("RTRASH_MAX_PERMANENT", "1")], 2);
    assert!(
        stderr_of(&out).contains("more than the cap of 1"),
        "{}",
        stderr_of(&out)
    );
    // The bad record names no origin, so a scope refuses the whole fix.
    let out = unchanged(&fix, &[("RTRASH_ALLOW_ROOTS", work.as_str())], 2);
    assert_eq!(common::outside_lines(&out), 1, "{}", stderr_of(&out));
    unchanged(&["status", pin.as_str(), "--fix"], &[], 2);

    let out = sb.run(&fix);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    assert_eq!(lines_starting(&out, "removed "), 2, "{}", stdout_of(&out));
    assert_eq!(
        info_names(&sb),
        ["ck-a.trashinfo", "ck-date.trashinfo", "ck-stray.txt"]
    );
    assert_eq!(
        names_in_trash(&sb),
        ["ck-a", "ck-bad", "ck-date", "ck-orph"]
    );

    // What is left is for the user: the date, two orphans, the stray name.
    let again = sb.run(&["status", &pin, "--check"]);
    assert_eq!(again.status.code(), Some(1), "{}", stderr_of(&again));
    assert_eq!(
        lines_starting(&again, "orphan: "),
        2,
        "{}",
        stdout_of(&again)
    );
    for prefix in ["dangling: ", "bad record: "] {
        assert_eq!(lines_starting(&again, prefix), 0, "{}", stdout_of(&again));
    }
    assert!(
        stderr_of(&again).contains("4 problems found, 0 records --fix would remove"),
        "{}",
        stderr_of(&again)
    );
}

/// `status --check --fix` appends one `fix` line to the trash history
/// once it removed a record. A check, a dry run and a fix with nothing
/// left to remove append none, and undo still brings back the put run
/// the fix left alone.
#[test]
fn status_fix_appends_one_fix_line_to_the_trash_history() {
    let sb = Sandbox::new("trashcheck-history");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("fx-a");
    assert!(sb.run(&["put", "fx-a"]).status.success());
    let work = sb.work().display().to_string();
    fs::write(
        sb.trash().join("info").join("fx-gone.trashinfo"),
        format!("[Trash Info]\nPath={work}/fx-gone\nDeletionDate=2026-01-02T03:04:05\n"),
    )
    .unwrap();
    let hist = sb.trash().join("rtrash-history");
    let lines = || -> Vec<String> {
        let body = fs::read_to_string(&hist).unwrap_or_default();
        body.lines().map(String::from).collect()
    };
    let events = || -> Vec<String> {
        let all = lines();
        all.iter()
            .map(|l| l.split(' ').nth(1).unwrap_or("").to_string())
            .collect()
    };
    assert_eq!(events(), ["put"]);
    let watch = [hist.as_path()];
    let check = ["status", pin.as_str(), "--check"];
    let fix = ["status", pin.as_str(), "--check", "--fix"];
    let dry = ["status", pin.as_str(), "--check", "--fix", "-n"];
    common::assert_refused_unchanged(&check, &watch, 1, &[], || sb.run(&check));
    common::assert_refused_unchanged(&dry, &watch, 1, &[], || sb.run(&dry));
    common::assert_refused_unchanged(&fix, &watch, 2, &[], || {
        sb.run_env(&fix, &[("RTRASH_NO_PERMANENT", "1")])
    });

    let out = sb.run(&fix);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    assert_eq!(lines_starting(&out, "removed "), 1, "{}", stdout_of(&out));
    assert_eq!(events(), ["put", "fix"], "{:?}", lines());
    // The fix line counts the one item left, fx-a.
    let all = lines();
    assert!(all[1].ends_with(" 1"), "{all:?}");
    common::assert_refused_unchanged(&fix, &watch, 0, &[], || sb.run(&fix));

    let out = sb.run(&["undo", &pin]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    assert!(sb.work().join("fx-a").exists(), "{}", stderr_of(&out));
    assert_eq!(lines()[..2], all[..], "{:?}", lines());
}

/// A folder and a link named like records are stray, never records:
/// `status --check` reports both and `--fix`, `--fix -n` and
/// `empty --dangling` leave the folder, its file, the link and the
/// link's target as they were (21a: --fix removed such a folder whole).
#[test]
fn status_fix_and_empty_dangling_leave_non_file_records_alone() {
    let sb = Sandbox::new("trashcheck-nonfile");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("nf-keep");
    assert!(sb.run(&["put", "nf-keep"]).status.success());
    let info = sb.trash().join("info");
    let files = sb.trash().join("files");
    let folder = info.join("d.trashinfo");
    fs::create_dir(&folder).unwrap();
    fs::write(folder.join("inner"), b"keep me").unwrap();
    // A link to a record that parses and whose payload is missing: read
    // through the link, it would pass for a dangling record.
    let target = sb.root.join("nf-target.trashinfo");
    let work = sb.work().display().to_string();
    fs::write(
        &target,
        format!("[Trash Info]\nPath={work}/s\nDeletionDate=2026-01-02T03:04:05\n"),
    )
    .unwrap();
    std::os::unix::fs::symlink(&target, info.join("s.trashinfo")).unwrap();

    let stray = "(kept; not a regular file, so not a record)";
    let check = sb.run(&["status", &pin, "--check"]);
    assert_eq!(check.status.code(), Some(1), "{}", stderr_of(&check));
    let strays = stdout_of(&check)
        .lines()
        .filter(|l| l.starts_with("stray: ") && l.ends_with(stray))
        .count();
    assert_eq!(strays, 2, "{}", stdout_of(&check));
    for prefix in ["dangling: ", "bad record: "] {
        assert_eq!(lines_starting(&check, prefix), 0, "{}", stdout_of(&check));
    }
    assert!(
        stderr_of(&check).contains("2 problems found, 0 records --fix would remove"),
        "{}",
        stderr_of(&check)
    );

    // Nothing here is a record to remove, so each run changes nothing.
    let watch = [info.as_path(), files.as_path(), target.as_path()];
    let unchanged = |args: &[&str], code: i32, lines: &[(&str, usize)]| {
        common::assert_refused_unchanged(&args, &watch, code, lines, || sb.run(args))
    };
    let out = unchanged(&["status", &pin, "--check", "--fix", "-n"], 1, &[]);
    assert_eq!(
        lines_starting(&out, "would remove "),
        0,
        "{}",
        stdout_of(&out)
    );
    let left = [("removed 0 records; 2 problems left to review", 1)];
    let out = unchanged(&["status", &pin, "--check", "--fix"], 0, &left[..]);
    assert_eq!(lines_starting(&out, "removed "), 0, "{}", stdout_of(&out));
    let none = [("Removed 0 records without a payload", 1)];
    unchanged(&["empty", "--home-only", "--dangling"], 0, &none[..]);
    assert_eq!(
        info_names(&sb),
        ["d.trashinfo", "nf-keep.trashinfo", "s.trashinfo"]
    );
    assert_eq!(fs::read(folder.join("inner")).unwrap(), b"keep me");
    let link = fs::symlink_metadata(info.join("s.trashinfo")).unwrap();
    assert!(link.file_type().is_symlink());
}
