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

/// Held by each test that reaches past its sandbox: one that mounts, one
/// that makes or removes a volume trash on a real file system, and one
/// that runs a bare `empty`, which reaches every volume trash. Without it
/// a bare `empty` in one test could clear the volume trash of a mount
/// another test just made. It is an exclusive `flock` on one file under
/// `CARGO_TARGET_TMPDIR`. Each call opens its own descriptor, so the lock
/// serializes these tests across the threads of `cargo test` as well as
/// across the processes of `cargo nextest`. Dropping the guard, a panic
/// that unwinds past it, or the death of the process lets go of it.
struct GlobalState(fs::File);

impl Drop for GlobalState {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        // SAFETY: an open descriptor this guard owns.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Wait for, then take, the lock on state outside the sandboxes; see
/// [`GlobalState`]. Take it before the sandbox, so it goes last.
fn global_state() -> GlobalState {
    use std::os::unix::io::AsRawFd;
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    fs::create_dir_all(dir).unwrap();
    let path = dir.join("rtrash-global-state.lock");
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    // SAFETY: an open descriptor; flock blocks until no other holds it.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    assert_eq!(rc, 0, "flock {}", path.display());
    GlobalState(file)
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
        let out = sb.run(&["rm", "--home-only", &evil]);
        assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
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
    // small is older; big is newer and larger, so date order and size differ.
    fs::write(sb.work().join("small.txt"), b"s").unwrap();
    assert!(sb.run(&["put", "small.txt"]).status.success());
    common::next_trash_second();
    fs::write(sb.work().join("big.bin"), vec![7u8; 1 << 20]).unwrap();
    assert!(sb.run(&["put", "big.bin"]).status.success());

    let out = sb.run_with_stdin(&["restore", "--plain", "--dry-run"], "0\n");
    assert!(out.status.success(), "{}", stderr_of(&out));
    let first = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    assert!(first.ends_with("small.txt"), "{first}");

    let out = sb.run_with_stdin(&["restore", "--plain", "--dry-run", "--sort=size"], "0\n");
    assert!(out.status.success(), "{}", stderr_of(&out));
    let first = String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|l| l.starts_with("would restore"))
        .unwrap_or("")
        .to_string();
    assert!(first.contains("big.bin"), "{first}");
    assert!(!sb.work().join("big.bin").exists());
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

/// trash-cli #354: put waits for a trash lock another process has. After
/// a second it names the trash; past `RTRASH_LOCK_WAIT` it gives up with
/// an error and leaves the file where it was. A lock let go in time lets
/// the put through.
#[test]
fn put_waits_for_the_trash_lock_with_a_note_and_a_bound() {
    use std::os::unix::io::AsRawFd;
    let sb = Sandbox::new("lock-wait");
    fs::create_dir_all(sb.trash().join("files")).unwrap();
    fs::create_dir_all(sb.trash().join("info")).unwrap();
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(sb.trash().join(".rtrash.lock"))
        .unwrap();
    let rc = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    assert_eq!(rc, 0);
    let kept = sb.touch("kept.txt");
    let out = sb.run_env(&["put", "kept.txt"], &[("RTRASH_LOCK_WAIT", "1.5")]);
    let err = stderr_of(&out);
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("waiting for the trash lock on"), "{err}");
    assert!(err.contains("RTRASH_LOCK_WAIT sets the wait"), "{err}");
    assert!(kept.exists(), "{err}");
    let moved = sb.touch("moved.txt");
    let mut child = common::rtrash(&sb.root)
        .args(["put", "moved.txt"])
        .current_dir(sb.work())
        .env("RTRASH_LOCK_WAIT", "60")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Let go of the lock once the put says it waits for it.
    let stderr = std::io::BufReader::new(child.stderr.take().unwrap());
    let mut lines = std::io::BufRead::lines(stderr).map(Result::unwrap);
    let note = lines.find(|l| l.contains("waiting for the trash lock on"));
    assert!(note.is_some(), "no note before the put ended");
    drop(lock);
    let rest: Vec<String> = lines.collect();
    let status = child.wait().unwrap();
    let err = rest.join("\n");
    assert!(status.success(), "{err}");
    assert!(!moved.exists(), "{err}");
}

/// script(1): /usr/bin/script, else the first one on PATH, for a host
/// that keeps it elsewhere (Nix has no /usr/bin). `None` without one.
#[cfg(target_os = "linux")]
fn script_bin() -> Option<PathBuf> {
    let usr = Path::new("/usr/bin/script");
    if usr.exists() {
        return Some(usr.to_path_buf());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join("script"))
        .find(|p| p.is_file())
}

/// What `rtrash ARGS` shows on a terminal, through script(1), in `sb`;
/// `None` without script(1). `args` is shell text, so a name with control
/// characters can come from printf.
#[cfg(target_os = "linux")]
fn on_a_terminal(sb: &Sandbox, args: &str) -> Option<String> {
    let script = script_bin()?;
    let cmd = format!("{} {args}", bin());
    let tty = Command::new(script)
        .args(["-qec", &cmd, "/dev/null"])
        .current_dir(sb.work())
        .env("SHELL", "/bin/sh")
        .env("NO_COLOR", "1")
        .env("XDG_DATA_HOME", sb.root.join("xdg"))
        .env("XDG_CONFIG_HOME", sb.root.join(".config"))
        .env("HOME", &sb.root)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run script");
    Some(stdout_of(&tty))
}

/// dua #365: a name with an escape sequence and a newline must not reach
/// the terminal raw through a message. put, restore, rm, empty and du each
/// name it with `?` for each control character on a terminal.
#[cfg(target_os = "linux")]
#[test]
fn messages_scrub_control_characters_on_a_terminal() {
    let sb = Sandbox::new("msg-scrub");
    let evil = r#""$(printf 'a\033[2Jb\nc')""#;
    let shown = "a?[2Jb?c";
    // A pipe gets the name as it is, as before.
    let piped = sb.run(&["put", "a\x1b[2Jb\nc"]);
    assert!(stderr_of(&piped).contains("a\x1b[2Jb\nc"), "{piped:?}");
    // A trashed item whose original path carries both characters.
    fs::create_dir_all(sb.trash().join("files")).unwrap();
    fs::create_dir_all(sb.trash().join("info")).unwrap();
    fs::write(sb.trash().join("files/evil"), b"x").unwrap();
    let record = format!(
        "[Trash Info]\nPath={}/a%1B%5B2Jb%0Ac\nDeletionDate=2024-01-02T03:04:05\n",
        sb.work().display()
    );
    fs::write(sb.trash().join("info/evil.trashinfo"), record).unwrap();
    let pin = format!("--trash-dir={}", sb.trash().display());
    let cases = [
        format!("put -- {evil}"),
        format!("restore --to={evil}"),
        "rm --home-only --dry-run --force --match=glob evil".to_string(),
        format!("empty {pin} --plain --dry-run --verbose"),
        format!("du --file={evil}"),
    ];
    for args in &cases {
        let Some(out) = on_a_terminal(&sb, args) else {
            common::skip("skipping: no script(1) to give rtrash a terminal");
            return;
        };
        assert!(out.contains(shown), "{args}: {out:?}");
        assert!(!out.contains('\x1b'), "{args}: {out:?}");
    }
}

/// dua #365: the paths `du --print0` lists go raw down a pipe, for
/// `xargs -0`, and with `?` for each control character on a terminal.
/// `p` in `du -i` prints its marked paths the same way.
#[cfg(target_os = "linux")]
#[test]
fn du_print0_scrubs_control_characters_on_a_terminal() {
    let sb = Sandbox::new("print0-scrub");
    let dir = sb.work().join("d");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a\x1b[2Jb\nc"), vec![1u8; 50_000]).unwrap();
    let piped = sb.run(&["du", "--files", "--print0", "d"]);
    assert!(piped.status.success(), "{piped:?}");
    assert!(piped.stdout.ends_with(b"a\x1b[2Jb\nc\0"), "{piped:?}");
    let Some(out) = on_a_terminal(&sb, "du --files --print0 d") else {
        common::skip("skipping: no script(1) to give rtrash a terminal");
        return;
    };
    assert!(out.contains("a?[2Jb?c"), "{out:?}");
    assert!(!out.contains('\x1b'), "{out:?}");
}

/// restore errors quote the record's Path= verbatim; on a terminal its
/// escape sequences must show as `?`, as list shows them.
#[cfg(target_os = "linux")]
#[test]
fn restore_errors_scrub_control_characters_on_a_terminal() {
    let Some(script) = script_bin() else {
        common::skip("skipping: no script(1) to give rtrash a terminal");
        return;
    };
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
    let tty = Command::new(&script)
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
    let out = sb.run(&[
        "rm",
        "--home-only",
        "--match=glob",
        "--older-than=1",
        &pattern,
    ]);
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
    let stderr = stderr_of(&out);
    assert!(stderr.contains("removed 'a.txt'"), "{stderr}");
    assert!(stderr.contains("removed directory 'd'"), "{stderr}");
    assert!(stderr.contains("missing.txt"), "{stderr}");
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
    for f in ["a.txt", "b.o", "c.o", "d.c"] {
        fs::write(lwork.join(f), b"x").unwrap();
    }
    let out = run_in(&lwork, &ldata, &["put", "a.txt", "b.o", "c.o", "d.c"]);
    assert!(out.status.success(), "{}", stderr_of(&out));

    let typed = lwork.join("a.txt");
    let out = run_in(&lwork, &ldata, &["rm", typed.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let glob = format!("{}/*.o", lwork.display());
    let out = run_in(&lwork, &ldata, &["rm", "--match=glob", "-n", &glob]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(stdout_of(&out).lines().count(), 2, "{}", stdout_of(&out));
    let left: Vec<_> = fs::read_dir(sb.trash().join("info"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(!left.iter().any(|n| n.starts_with("a.txt")), "{left:?}");
    assert_eq!(left.len(), 3, "{left:?}");
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

/// `.` and `..` are refused however the operand spells them, as GNU rm
/// refuses them (trash-cli #434 lost a directory's files to `./`). `d/.`
/// used to read as `d` and trash it.
#[test]
fn put_refuses_dot_operands_however_they_are_spelled() {
    let sb = Sandbox::new("put-dot-spellings");
    fs::create_dir(sb.work().join("d")).unwrap();
    sb.touch("d/f");
    for op in [
        ".", "./", ".//", "..", "../", "d/.", "d/./", "d/..", "d/../",
    ] {
        let out = sb.run(&["put", "-rf", op]);
        assert_eq!(out.status.code(), Some(1), "{op}: {}", stderr_of(&out));
        let want = format!("refusing to remove '.' or '..' directory: skipping '{op}'");
        assert!(stderr_of(&out).contains(&want), "{op}: {}", stderr_of(&out));
    }
    assert!(sb.work().join("d/f").exists());
    assert_eq!(names_in(&sb.trash().join("files")), Vec::<String>::new());
}

/// `d/` and `d//` trash d, with a record that names d, as `d` does, and
/// `f/` on a file is an error that leaves the file, `-f` included
/// (trash-cli #434).
#[test]
fn put_with_a_trailing_slash_trashes_the_directory_it_names() {
    let sb = Sandbox::new("put-trailing-slash");
    for d in ["d1", "d2"] {
        fs::create_dir(sb.work().join(d)).unwrap();
        sb.touch(&format!("{d}/f"));
    }
    let out = sb.run(&["put", "-r", "d1/", "d2//"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(names_in_trash(&sb), ["d1", "d2"]);
    let work = sb.work().display().to_string();
    for d in ["d1", "d2"] {
        let record = sb.trash().join(format!("info/{d}.trashinfo"));
        let info = fs::read_to_string(record).unwrap();
        assert!(info.contains(&format!("Path={work}/{d}\n")), "{info}");
        assert!(sb.trash().join(format!("files/{d}/f")).exists());
    }
    sb.touch("f");
    for args in [&["put", "f/"][..], &["put", "-f", "f/"][..]] {
        let out = sb.run(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        let err = stderr_of(&out);
        assert!(err.contains("cannot remove 'f/'"), "{args:?}: {err}");
    }
    assert!(sb.work().join("f").exists());
}

/// `link/`, where link points to a directory, names that directory while
/// the move would take the link, so put refuses it and leaves the link and
/// the directory's files alone. `link` then trashes the link only
/// (trash-cli #434).
#[test]
fn put_refuses_a_symlink_named_with_a_trailing_slash() {
    let sb = Sandbox::new("put-link-slash");
    fs::create_dir(sb.work().join("target")).unwrap();
    sb.touch("target/f");
    let link = sb.work().join("link");
    std::os::unix::fs::symlink("target", &link).unwrap();
    for args in [
        &["put", "link/"][..],
        &["put", "-rf", "link/"][..],
        &["put", "-d", "link//"][..],
    ] {
        let out = sb.run(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", stderr_of(&out));
        let err = stderr_of(&out);
        assert!(
            err.contains("it is a symlink to a directory"),
            "{args:?}: {err}"
        );
    }
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    assert!(sb.work().join("target/f").exists());
    assert_eq!(names_in(&sb.trash().join("files")), Vec::<String>::new());

    let out = sb.run(&["put", "link"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(link.symlink_metadata().is_err());
    assert!(sb.work().join("target/f").exists());
    assert_eq!(names_in_trash(&sb), ["link"]);
}

/// A trailing slash does not change put's answer. In a parent that does
/// not allow deleting, `p/d1/` and `p/d2` get the same exit code and the
/// same fate, and so do the read-only directories `ro1/` and `ro2`. Each
/// file ends up where it was or in the trash (trash-cli #434 lost them).
#[test]
fn put_gives_the_same_answer_with_or_without_a_trailing_slash() {
    use std::os::unix::fs::PermissionsExt;
    let sb = Sandbox::new("put-slash-perms");
    let (w, files) = (sb.work(), sb.trash().join("files"));
    for d in ["p/d1", "p/d2", "ro1", "ro2"] {
        fs::create_dir_all(w.join(d)).unwrap();
        sb.touch(&format!("{d}/f"));
    }
    let chmod = |p: &Path, m: u32| {
        if p.exists() {
            fs::set_permissions(p, fs::Permissions::from_mode(m)).unwrap();
        }
    };
    for d in ["p", "ro1", "ro2"] {
        chmod(&w.join(d), 0o555);
    }
    for (slash, bare, a, b) in [("p/d1/", "p/d2", "d1", "d2"), ("ro1/", "ro2", "ro1", "ro2")] {
        let one = sb.run(&["put", "-r", slash]);
        let two = sb.run(&["put", "-r", bare]);
        let why = format!("{}{}", stderr_of(&one), stderr_of(&two));
        assert_eq!(
            one.status.code(),
            two.status.code(),
            "{slash} {bare}: {why}"
        );
        let gone = |d: &str| !w.join(d).exists();
        let dir = slash.trim_end_matches('/');
        assert_eq!(gone(dir), gone(bare), "{slash} {bare}: {why}");
        for (orig, name) in [(dir, a), (bare, b)] {
            let kept = w.join(orig).join("f").exists();
            let trashed = files.join(name).join("f").exists();
            assert!(
                kept != trashed,
                "{orig}: kept {kept}, trashed {trashed}: {why}"
            );
        }
    }
    for d in [w.join("p"), w.join("ro1"), w.join("ro2")] {
        chmod(&d, 0o755);
    }
    for d in ["ro1", "ro2"] {
        chmod(&files.join(d), 0o755);
    }
}

/// trash-cli #427 and #434 lost the files of a directory they could not
/// move. On one file system put renames, and a rename needs a parent
/// that allows deleting: put refuses a directory and a file in a
/// read-only parent.
/// Linux also needs write access to a directory to give it a new parent,
/// so a read-only directory is refused there too. Every file stays, and
/// the trash gets no payload and no record. Root may move all of these,
/// so the test needs an ordinary user.
#[test]
fn put_refuses_what_it_cannot_move_and_keeps_every_file() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        common::skip("root may move every case here; skipping");
        return;
    }
    let sb = Sandbox::new("put-unmovable");
    let w = sb.work();
    let chmod = |p: &Path, m: u32| fs::set_permissions(p, fs::Permissions::from_mode(m)).unwrap();
    fs::create_dir_all(w.join("p/d")).unwrap();
    sb.touch("p/d/f");
    sb.touch("p/x");
    fs::create_dir_all(w.join("ro/sub")).unwrap();
    sb.touch("ro/f");
    sb.touch("ro/sub/g");
    chmod(&w.join("p"), 0o555);
    chmod(&w.join("ro"), 0o555);
    let mut cases = vec![("p/d", "p/d/f"), ("p/x", "p/x")];
    if cfg!(target_os = "linux") {
        cases.push(("ro", "ro/sub/g"));
    }
    for (op, keep) in cases {
        let out = sb.run(&["put", "-r", op]);
        let err = stderr_of(&out);
        assert_eq!(out.status.code(), Some(1), "{op}: {err}");
        let want = format!("cannot trash '{op}': Permission denied");
        assert!(err.contains(&want), "{op}: {err}");
        assert!(w.join(keep).exists(), "{op}: {keep} is gone");
        assert_eq!(
            names_in(&sb.trash().join("files")),
            Vec::<String>::new(),
            "{op}"
        );
        assert_eq!(
            names_in(&sb.trash().join("info")),
            Vec::<String>::new(),
            "{op}"
        );
    }
    chmod(&w.join("p"), 0o755);
    chmod(&w.join("ro"), 0o755);
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

    // A built-in safe-rm entry that exists here: put -f of a missing path
    // says nothing and exits 0, and the Nix build sandbox has no /usr.
    let system = ["/usr", "/etc", "/bin", "/lib", "/var"]
        .into_iter()
        .find(|p| Path::new(p).is_dir());
    if let Some(system) = system {
        let out = sb.run(&["put", "-rf", system]);
        assert_eq!(out.status.code(), Some(1));
        let err = stderr_of(&out);
        assert!(
            err.contains(&format!("safe-rm list entry '{system}'")),
            "{err}"
        );
    } else {
        common::skip("none of /usr, /etc, /bin, /lib or /var exists; skipping the system path");
    }

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
        common::skip("git unavailable; skipping");
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

    // The tree cannot be listed, so the git check calls it unknown and
    // leaves it. -f is the override.
    let out = sb.run(&["rm", "--home-only", "-f", "--match=glob", "linux-6.9"]);
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
fn empty_dry_run_is_nonzero_when_nothing_matches() {
    let sb = Sandbox::new("empty-dry-none");
    fs::create_dir_all(sb.trash().join("files")).unwrap();
    fs::create_dir_all(sb.trash().join("info")).unwrap();
    let pin = format!("--trash-dir={}", sb.trash().display());
    let out = sb.run(&["empty", &pin, "--plain", "--dry-run"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).contains("nothing would be permanently removed"),
        "{}",
        stderr_of(&out)
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
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.contains(".tmp.") || name.ends_with(".tmp")
        })
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
    let out = sb.run(&["rm", "--match=glob", "--dry-run", "*.o"]);
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
    let _global = global_state();
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
        common::skip(&format!(
            "skipping: a bare empty would remove the host's '{foreign}'"
        ));
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
    let out = sb.run(&["rm", "--match=glob", "*"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr_of(&out));
    assert_eq!(trash_names(&sb), vec!["a.txt"]);
}

/// A mass pattern must be refused before it deletes. The pin keeps a
/// guard that fails from reaching a volume trash outside the sandbox.
fn mass_pattern_keeps_the_trash(tag: &str, args: &[&str]) {
    let sb = Sandbox::new(tag);
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("keep.txt");
    let put = sb.run(&["put", &pin, "keep.txt"]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let mut cmd = vec!["rm", pin.as_str()];
    cmd.extend_from_slice(args);
    let out = sb.run(&cmd);
    let err = stderr_of(&out);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("refusing mass pattern"), "{err}");
    assert_eq!(trash_names(&sb), vec!["keep.txt".to_string()]);
}

#[test]
fn question_star_is_refused_without_force() {
    mass_pattern_keeps_the_trash("rm-mass-qstar", &["--glob", "?*"]);
}

#[test]
fn a_regex_dot_is_refused_without_force() {
    mass_pattern_keeps_the_trash("rm-mass-dot", &["--regex", "."]);
}

#[test]
fn an_empty_prefix_is_refused_without_force() {
    let sb = Sandbox::new("rm-mass-empty");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("keep.txt");
    assert!(sb.run(&["put", &pin, "keep.txt"]).status.success());
    let out = sb.run(&["rm", &pin, "--prefix", ""]);
    let err = stderr_of(&out);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("'' is not a path"), "{err}");
    assert_eq!(trash_names(&sb), vec!["keep.txt".to_string()]);
}

#[test]
fn slash_slash_is_refused_without_force() {
    mass_pattern_keeps_the_trash("rm-mass-slashslash", &["--prefix", "//"]);
}

#[test]
fn slash_dot_is_refused_without_force() {
    mass_pattern_keeps_the_trash("rm-mass-slashdot", &["--prefix", "/."]);
}

#[test]
fn a_prefix_that_climbs_to_the_root_is_refused_without_force() {
    mass_pattern_keeps_the_trash("rm-mass-dotdot", &["--prefix", "/tmp/.."]);
}

#[test]
fn tilde_is_refused_without_force() {
    mass_pattern_keeps_the_trash("rm-mass-tilde", &["--prefix", "~"]);
}

#[test]
fn a_trash_root_alias_is_refused_without_force() {
    let sb = Sandbox::new("rm-mass-trashroot");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("keep.txt");
    let put = sb.run(&["put", &pin, "keep.txt"]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let alias = format!("{}/.", sb.trash().display());
    let tilde = "~/xdg/Trash/.".to_string();
    for pattern in [alias.as_str(), tilde.as_str()] {
        let out = sb.run(&["rm", &pin, "--prefix", pattern]);
        let err = stderr_of(&out);
        assert_eq!(out.status.code(), Some(2), "{pattern}: {err}");
        assert!(err.contains("refusing mass pattern"), "{pattern}: {err}");
        assert_eq!(trash_names(&sb), vec!["keep.txt".to_string()], "{pattern}");
    }
}

#[test]
fn a_regex_that_matches_every_path_is_refused_without_force() {
    for (tag, pattern) in [
        ("rm-mass-re-slash", "/"),
        ("rm-mass-re-caret", "^/"),
        ("rm-mass-re-dotdot", ".."),
        ("rm-mass-re-count", ".{2}"),
        ("rm-mass-re-bs", r"\\"),
    ] {
        mass_pattern_keeps_the_trash(tag, &["--regex", pattern]);
    }
}

#[test]
fn a_glob_that_matches_every_path_is_refused_without_force() {
    for (tag, pattern) in [
        ("rm-mass-glob-q", "/?*"),
        ("rm-mass-glob-sq", "/*?"),
        ("rm-mass-glob-dq", "**/?*"),
        ("rm-mass-glob-starq", "*/?*"),
        ("rm-mass-glob-drive", "?:\\*"),
    ] {
        mass_pattern_keeps_the_trash(tag, &["--glob", pattern]);
    }
}

#[test]
fn a_narrow_prefix_still_removes_only_what_it_names() {
    let sb = Sandbox::new("rm-narrow-prefix");
    let pin = format!("--trash-dir={}", sb.trash().display());
    fs::create_dir_all(sb.work().join("proj")).unwrap();
    fs::write(sb.work().join("proj/a.txt"), b"a").unwrap();
    sb.touch("other.txt");
    assert!(sb
        .run(&["put", &pin, "proj/a.txt", "other.txt"])
        .status
        .success());
    let proj = sb.work().join("proj");
    let out = sb.run(&["rm", &pin, "--prefix", proj.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let mut left = trash_names(&sb);
    left.sort();
    assert_eq!(left, vec!["other.txt".to_string()]);
}

#[test]
fn a_prefix_is_refused_when_home_is_unset() {
    let sb = Sandbox::new("rm-mass-nohome");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("keep.txt");
    let put = sb.run(&["put", &pin, "keep.txt"]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let out = common::rtrash(&sb.root)
        .env_remove("HOME")
        .env_remove("USERPROFILE")
        .env_remove("XDG_DATA_HOME")
        .args(["rm", &pin, "--prefix", "/tmp/not-a-root"])
        .current_dir(sb.work())
        .output()
        .unwrap();
    let err = stderr_of(&out);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("refusing mass pattern"), "{err}");
    assert_eq!(trash_names(&sb), vec!["keep.txt".to_string()]);
}

#[test]
fn a_glob_is_refused_when_home_is_unset() {
    let sb = Sandbox::new("rm-glob-nohome");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("inside.txt");
    fs::write(sb.root.join("outside.txt"), b"outside").unwrap();
    let put = sb.run(&[
        "put",
        &pin,
        sb.work().join("inside.txt").to_str().unwrap(),
        sb.root.join("outside.txt").to_str().unwrap(),
    ]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let pattern = format!("{}/*", sb.work().display());
    let out = common::rtrash(&sb.root)
        .env_remove("HOME")
        .env_remove("USERPROFILE")
        .env_remove("XDG_DATA_HOME")
        .args(["rm", &pin, "--glob", &pattern])
        .current_dir(sb.work())
        .output()
        .unwrap();
    let err = stderr_of(&out);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("When the scope is unknown"), "{err}");
    let mut left = trash_names(&sb);
    left.sort();
    assert_eq!(
        left,
        vec!["inside.txt".to_string(), "outside.txt".to_string()]
    );
}

#[test]
fn a_basename_glob_stays_narrow_when_home_is_unset() {
    let sb = Sandbox::new("rm-basename-glob-nohome");
    let pin = format!("--trash-dir={}", sb.trash().display());
    for name in ["keep.txt", "other.txt"] {
        sb.touch(name);
    }
    let put = sb.run(&["put", &pin, "keep.txt", "other.txt"]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let out = common::rtrash(&sb.root)
        .env_remove("HOME")
        .env_remove("USERPROFILE")
        .env_remove("XDG_DATA_HOME")
        .args(["rm", &pin, "--glob", "keep*"])
        .current_dir(sb.work())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(trash_names(&sb), vec!["other.txt".to_string()]);
}

#[test]
fn a_cache_prefix_glob_stays_narrow_when_home_is_unset() {
    let sb = Sandbox::new("rm-cache-glob-nohome");
    let pin = format!("--trash-dir={}", sb.trash().display());
    for name in ["cache1.dat", "other.txt"] {
        sb.touch(name);
    }
    let put = sb.run(&["put", &pin, "cache1.dat", "other.txt"]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let out = common::rtrash(&sb.root)
        .env_remove("HOME")
        .env_remove("USERPROFILE")
        .env_remove("XDG_DATA_HOME")
        .args(["rm", &pin, "--glob", "cache*"])
        .current_dir(sb.work())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(trash_names(&sb), vec!["other.txt".to_string()]);
}

#[test]
fn a_basename_glob_uses_full_paths_only_with_known_scope_roots() {
    for roots_known in [true, false] {
        let tag = if roots_known {
            "rm-basename-glob-known-roots"
        } else {
            "rm-basename-glob-unknown-roots"
        };
        let sb = Sandbox::new(tag);
        let pin = format!("--trash-dir={}", sb.trash().display());
        let work = sb.work().join("work");
        let other = sb.work().join("other");
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&other).unwrap();
        for path in [
            work.join("a.txt"),
            work.join("b.txt"),
            other.join("work.txt"),
            sb.root.join("outside.txt"),
        ] {
            fs::write(path, b"x").unwrap();
        }
        let put = sb.run(&[
            "put",
            &pin,
            work.join("a.txt").to_str().unwrap(),
            work.join("b.txt").to_str().unwrap(),
            other.join("work.txt").to_str().unwrap(),
            sb.root.join("outside.txt").to_str().unwrap(),
        ]);
        assert!(put.status.success(), "{tag}: {}", stderr_of(&put));
        let mut cmd = common::rtrash(&sb.root);
        if roots_known {
            let home = sb.root.join("hm");
            fs::create_dir_all(&home).unwrap();
            cmd.env("HOME", &home).env("USERPROFILE", &home);
        } else {
            cmd.env_remove("HOME")
                .env_remove("USERPROFILE")
                .env_remove("XDG_DATA_HOME");
        }
        let out = cmd
            .args(["rm", &pin, "--glob", "*work*"])
            .current_dir(sb.work())
            .output()
            .unwrap();
        assert!(out.status.success(), "{tag}: {}", stderr_of(&out));
        let mut left = trash_names(&sb);
        left.sort();
        let expected = if roots_known {
            vec!["outside.txt".to_string()]
        } else {
            vec![
                "a.txt".to_string(),
                "b.txt".to_string(),
                "outside.txt".to_string(),
            ]
        };
        assert_eq!(left, expected, "{tag}");
    }
}

#[test]
fn disjoint_scope_roots_do_not_make_an_inherited_component_a_glob_match() {
    let sb = Sandbox::new("rm-disjoint-person-roots");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("chosen.txt");
    sb.touch("outside.dat");
    assert!(sb
        .run(&["put", &pin, "chosen.txt", "outside.dat"])
        .status
        .success());
    let home = sb.root.join("home");
    let xdg = Path::new("/rtrash-disjoint-xdg");
    fs::create_dir_all(&home).unwrap();
    let out = common::rtrash(&sb.root)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_DATA_HOME", xdg)
        .args(["rm", &pin, "--older-than=0", "--glob", "*person*"])
        .current_dir(sb.work())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains("no trash items match"));
    assert_eq!(trash_names(&sb).len(), 2);
}

#[test]
fn a_symlinked_temp_parent_does_not_widen_a_basename_glob() {
    let top = common::scratch_dir("test", "rm-glob-symlinked-temp");
    let real_tmp = top.join("folder-o");
    fs::create_dir(&real_tmp).unwrap();
    let linked_tmp = top.join("linked-tmp");
    std::os::unix::fs::symlink(&real_tmp, &linked_tmp).unwrap();
    let spelled_root = linked_tmp.join("sandbox");
    fs::create_dir_all(spelled_root.join("work")).unwrap();
    let sb = Sandbox {
        root: fs::canonicalize(&spelled_root).unwrap(),
    };
    fs::create_dir_all(sb.root.join("home")).unwrap();
    fs::write(sb.work().join("foo.txt"), b"x").unwrap();
    fs::write(sb.root.join("spared.txt"), b"x").unwrap();
    let pin = format!("--trash-dir={}", sb.trash().display());
    let put = common::rtrash(&sb.root)
        .env("TMPDIR", &linked_tmp)
        .args([
            "put",
            &pin,
            sb.work().join("foo.txt").to_str().unwrap(),
            sb.root.join("spared.txt").to_str().unwrap(),
        ])
        .current_dir(sb.work())
        .output()
        .unwrap();
    assert!(put.status.success(), "{}", stderr_of(&put));

    let out = common::rtrash(&sb.root)
        .env("HOME", sb.root.join("home"))
        .env("USERPROFILE", sb.root.join("home"))
        .env("TMPDIR", &linked_tmp)
        .args(["rm", &pin, "--glob", "*o*"])
        .current_dir(sb.work())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(trash_names(&sb), vec!["spared.txt".to_string()]);
}

#[test]
fn stripping_an_inherited_scope_prefix_never_adds_a_glob_match() {
    let sb = Sandbox::new("rm-glob-scope-relative");
    fs::create_dir_all(sb.root.join("home")).unwrap();
    let width = sb.root.to_str().unwrap().len();
    assert!(width > 2);
    let nested = sb.root.join("a").join("b".repeat(width - 2));
    fs::create_dir_all(nested.parent().unwrap()).unwrap();
    fs::write(&nested, b"x").unwrap();
    fs::write(sb.root.join("spare-long-name.txt"), b"x").unwrap();
    let pin = format!("--trash-dir={}", sb.trash().display());
    let put = sb.run(&[
        "put",
        &pin,
        nested.to_str().unwrap(),
        sb.root.join("spare-long-name.txt").to_str().unwrap(),
    ]);
    assert!(put.status.success(), "{}", stderr_of(&put));

    let pattern = "?".repeat(width);
    let out = common::rtrash(&sb.root)
        .env("HOME", sb.root.join("home"))
        .env("USERPROFILE", sb.root.join("home"))
        .args(["rm", &pin, "--glob", &pattern])
        .current_dir(sb.work())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert_eq!(trash_names(&sb).len(), 2);
}

#[test]
fn a_regex_directory_is_refused_when_home_is_unset() {
    let sb = Sandbox::new("rm-regex-nohome");
    let pin = format!("--trash-dir={}", sb.trash().display());
    for name in ["inside.txt", "second.txt"] {
        sb.touch(name);
    }
    fs::write(sb.root.join("outside.txt"), b"outside").unwrap();
    let put = sb.run(&[
        "put",
        &pin,
        sb.work().join("inside.txt").to_str().unwrap(),
        sb.work().join("second.txt").to_str().unwrap(),
        sb.root.join("outside.txt").to_str().unwrap(),
    ]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let pattern = format!("^{}/.*", regex_escape(sb.work().to_str().unwrap()));
    let out = common::rtrash(&sb.root)
        .env_remove("HOME")
        .env_remove("USERPROFILE")
        .env_remove("XDG_DATA_HOME")
        .args(["rm", &pin, "--regex", &pattern])
        .current_dir(sb.work())
        .output()
        .unwrap();
    let err = stderr_of(&out);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("When the scope is unknown"), "{err}");
    let mut left = trash_names(&sb);
    left.sort();
    assert_eq!(
        left,
        vec![
            "inside.txt".to_string(),
            "outside.txt".to_string(),
            "second.txt".to_string()
        ]
    );
}

fn refuse_unknown_regex_scope(tag: &str, dir_name: &str, mode: &str) {
    let sb = Sandbox::new(tag);
    let pin = format!("--trash-dir={}", sb.trash().display());
    let dir = sb.work().join(dir_name);
    fs::create_dir_all(&dir).unwrap();
    for name in ["a.txt", "b.txt"] {
        fs::write(dir.join(name), name).unwrap();
    }
    fs::write(sb.root.join("outside.txt"), b"outside").unwrap();
    let put = sb.run(&[
        "put",
        &pin,
        dir.join("a.txt").to_str().unwrap(),
        dir.join("b.txt").to_str().unwrap(),
        sb.root.join("outside.txt").to_str().unwrap(),
    ]);
    assert!(put.status.success(), "{tag}: {}", stderr_of(&put));
    let pattern = match mode {
        "raw" => format!("^{}/.*", dir.display()),
        "escaped" => format!("^{}/.*", regex_escape(dir.to_str().unwrap())),
        "parent-wildcard" => format!("^.*/{dir_name}/.*"),
        _ => unreachable!("{mode}"),
    };
    let out = common::rtrash(&sb.root)
        .env_remove("HOME")
        .env_remove("USERPROFILE")
        .env_remove("XDG_DATA_HOME")
        .args(["rm", &pin, "--regex", &pattern])
        .current_dir(sb.work())
        .output()
        .unwrap();
    let err = stderr_of(&out);
    assert_eq!(out.status.code(), Some(2), "{tag} {pattern}: {err}");
    assert!(err.contains("When the scope is unknown"), "{tag}: {err}");
    let mut left = trash_names(&sb);
    left.sort();
    assert_eq!(
        left,
        vec![
            "a.txt".to_string(),
            "b.txt".to_string(),
            "outside.txt".to_string()
        ],
        "{tag}: {err}"
    );
}

#[test]
fn a_regex_directory_with_a_dot_is_refused_without_scope_roots() {
    refuse_unknown_regex_scope("rm-regex-dot-dir", "work.with.dot", "raw");
}

#[test]
fn an_escaped_regex_directory_with_a_dot_is_refused_without_scope_roots() {
    refuse_unknown_regex_scope("rm-regex-escaped-dot", "work.with.dot", "escaped");
}

#[test]
fn a_regex_hidden_directory_is_refused_without_scope_roots() {
    refuse_unknown_regex_scope("rm-regex-hidden", ".config", "raw");
}

#[test]
fn an_escaped_regex_hidden_directory_is_refused_without_scope_roots() {
    refuse_unknown_regex_scope("rm-regex-escaped-hidden", ".config", "escaped");
}

#[test]
fn a_regex_directory_below_a_wildcard_parent_is_refused_without_scope_roots() {
    refuse_unknown_regex_scope("rm-regex-wild-parent", "work", "parent-wildcard");
}

#[test]
fn every_regex_needs_force_without_scope_roots() {
    let sb = Sandbox::new("rm-regex-narrow-nohome");
    let pin = format!("--trash-dir={}", sb.trash().display());
    for name in ["a.o", "foo", "b.c", "keep.txt"] {
        sb.touch(name);
    }
    fs::write(sb.root.join("outside.txt"), b"outside").unwrap();
    let put = sb.run(&[
        "put",
        &pin,
        "a.o",
        "foo",
        "b.c",
        "keep.txt",
        sb.root.join("outside.txt").to_str().unwrap(),
    ]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let run = |pattern: &str| {
        common::rtrash(&sb.root)
            .env_remove("HOME")
            .env_remove("USERPROFILE")
            .env_remove("XDG_DATA_HOME")
            .args(["rm", &pin, "--regex", pattern])
            .current_dir(sb.work())
            .output()
            .unwrap()
    };
    for pattern in [
        r"\.o$".to_string(),
        "o".to_string(),
        "foo".to_string(),
        format!(r"^{}/.*\.c$", regex_escape(sb.work().to_str().unwrap())),
    ] {
        let out = run(&pattern);
        let err = stderr_of(&out);
        assert_eq!(out.status.code(), Some(2), "{pattern}: {err}");
        assert!(
            err.contains("When the scope is unknown"),
            "{pattern}: {err}"
        );
    }
    assert_eq!(trash_names(&sb).len(), 5);
    let out = common::rtrash(&sb.root)
        .env_remove("HOME")
        .env_remove("USERPROFILE")
        .env_remove("XDG_DATA_HOME")
        .args(["rm", &pin, "--force", "--regex", r"\.o$"])
        .current_dir(sb.work())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    let mut left = trash_names(&sb);
    left.sort();
    assert_eq!(
        left,
        vec![
            "b.c".to_string(),
            "foo".to_string(),
            "keep.txt".to_string(),
            "outside.txt".to_string()
        ]
    );
}

fn remove_only_objects_without_scope_roots(tag: &str, flag: &str, pattern: &str, nested: bool) {
    let sb = Sandbox::new(tag);
    let pin = format!("--trash-dir={}", sb.trash().display());
    let (a, b) = if nested {
        let a = sb.work().join("group/proj1/a.o");
        let b = sb.work().join("group/proj2/b.o");
        fs::create_dir_all(a.parent().unwrap()).unwrap();
        fs::create_dir_all(b.parent().unwrap()).unwrap();
        (a, b)
    } else {
        (sb.work().join("a.o"), sb.work().join("b.o"))
    };
    fs::write(&a, b"a").unwrap();
    fs::write(&b, b"b").unwrap();
    fs::write(sb.root.join("outside.txt"), b"outside").unwrap();
    let put = sb.run(&[
        "put",
        &pin,
        a.to_str().unwrap(),
        b.to_str().unwrap(),
        sb.root.join("outside.txt").to_str().unwrap(),
    ]);
    assert!(put.status.success(), "{tag}: {}", stderr_of(&put));
    let mut cmd = common::rtrash(&sb.root);
    cmd.env_remove("HOME")
        .env_remove("USERPROFILE")
        .env_remove("XDG_DATA_HOME")
        .args(["rm", &pin]);
    if flag == "--regex" {
        cmd.arg("--force");
    }
    let out = cmd
        .args([flag, pattern])
        .current_dir(sb.work())
        .output()
        .unwrap();
    assert!(out.status.success(), "{tag}: {}", stderr_of(&out));
    assert_eq!(trash_names(&sb), vec!["outside.txt".to_string()]);
}

#[test]
fn a_basename_glob_can_remove_the_only_objects_below_a_directory() {
    remove_only_objects_without_scope_roots("rm-objects-glob", "--glob", "*.o", false);
}

#[test]
fn a_forced_basename_regex_can_remove_the_only_objects_below_a_directory() {
    remove_only_objects_without_scope_roots("rm-objects-regex", "--regex", r"\.o$", false);
}

#[test]
fn a_basename_glob_can_remove_objects_from_separate_subdirectories() {
    remove_only_objects_without_scope_roots("rm-nested-objects-glob", "--glob", "*.o", true);
}

#[test]
fn a_forced_basename_regex_can_remove_objects_from_separate_subdirectories() {
    remove_only_objects_without_scope_roots("rm-nested-objects-regex", "--regex", r"\.o$", true);
}

#[test]
fn an_unanchored_basename_regex_needs_force_without_scope_roots() {
    let sb = Sandbox::new("rm-unanchored-basename");
    let pin = format!("--trash-dir={}", sb.trash().display());
    let dir = sb.work().join("work");
    fs::create_dir_all(&dir).unwrap();
    for name in ["foo.txt", "keep.txt"] {
        fs::write(dir.join(name), name).unwrap();
    }
    fs::write(sb.root.join("outside.txt"), b"outside").unwrap();
    let put = sb.run(&[
        "put",
        &pin,
        dir.join("foo.txt").to_str().unwrap(),
        dir.join("keep.txt").to_str().unwrap(),
        sb.root.join("outside.txt").to_str().unwrap(),
    ]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let out = common::rtrash(&sb.root)
        .env_remove("HOME")
        .env_remove("USERPROFILE")
        .env_remove("XDG_DATA_HOME")
        .args(["rm", &pin, "--regex", "foo"])
        .current_dir(sb.work())
        .output()
        .unwrap();
    let err = stderr_of(&out);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("When the scope is unknown"), "{err}");
    let mut left = trash_names(&sb);
    left.sort();
    assert_eq!(
        left,
        vec![
            "foo.txt".to_string(),
            "keep.txt".to_string(),
            "outside.txt".to_string()
        ]
    );
}

#[test]
fn a_directory_regex_refuses_its_only_entry_without_scope_roots() {
    let sb = Sandbox::new("rm-regex-one-nohome");
    let pin = format!("--trash-dir={}", sb.trash().display());
    let dir = sb.work().join("work");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("only.txt"), b"only").unwrap();
    fs::write(sb.root.join("outside.txt"), b"outside").unwrap();
    let put = sb.run(&[
        "put",
        &pin,
        dir.join("only.txt").to_str().unwrap(),
        sb.root.join("outside.txt").to_str().unwrap(),
    ]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let pattern = format!("^{}/.*", regex_escape(dir.to_str().unwrap()));
    let out = common::rtrash(&sb.root)
        .env_remove("HOME")
        .env_remove("USERPROFILE")
        .env_remove("XDG_DATA_HOME")
        .args(["rm", &pin, "--regex", &pattern])
        .current_dir(sb.work())
        .output()
        .unwrap();
    let err = stderr_of(&out);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("When the scope is unknown"), "{err}");
    let mut left = trash_names(&sb);
    left.sort();
    assert_eq!(
        left,
        vec!["only.txt".to_string(), "outside.txt".to_string()]
    );
}

#[test]
fn directory_glob_regex_and_substring_share_the_same_refusal() {
    for (tag, flag) in [
        ("rm-dir-kind-glob", "--glob"),
        ("rm-dir-kind-regex", "--regex"),
        ("rm-dir-kind-substring", "--substring"),
    ] {
        let sb = Sandbox::new(tag);
        let pin = format!("--trash-dir={}", sb.trash().display());
        let dir = sb.work().join("named");
        fs::create_dir_all(&dir).unwrap();
        for name in ["a.txt", "b.txt"] {
            fs::write(dir.join(name), name).unwrap();
        }
        fs::write(sb.root.join("outside.txt"), b"outside").unwrap();
        let put = sb.run(&[
            "put",
            &pin,
            dir.join("a.txt").to_str().unwrap(),
            dir.join("b.txt").to_str().unwrap(),
            sb.root.join("outside.txt").to_str().unwrap(),
        ]);
        assert!(put.status.success(), "{tag}: {}", stderr_of(&put));
        let pattern = match flag {
            "--glob" => format!("{}/*", dir.display()),
            "--regex" => format!("^{}/.*", regex_escape(dir.to_str().unwrap())),
            "--substring" => format!("{}/", dir.display()),
            _ => unreachable!("{flag}"),
        };
        let out = common::rtrash(&sb.root)
            .env_remove("HOME")
            .env_remove("USERPROFILE")
            .env_remove("XDG_DATA_HOME")
            .args(["rm", &pin, flag, &pattern])
            .current_dir(sb.work())
            .output()
            .unwrap();
        let err = stderr_of(&out);
        assert_eq!(out.status.code(), Some(2), "{tag} {pattern}: {err}");
        assert!(err.contains("When the scope is unknown"), "{tag}: {err}");
        assert_eq!(trash_names(&sb).len(), 3, "{tag}: {err}");
    }
}

#[test]
fn directory_pattern_spellings_share_the_same_refusal() {
    let mut failures = Vec::new();
    for (n, mode) in [
        "unanchored-path",
        "wild-parent",
        "wrapped-anchor",
        "alternative-anchor",
        "inline-flags",
        "probe-bypass",
        "glob-no-separator",
    ]
    .into_iter()
    .enumerate()
    {
        let tag = format!("rm-dir-spelling-{n}");
        let sb = Sandbox::new(&tag);
        let pin = format!("--trash-dir={}", sb.trash().display());
        let dir = sb.work().join("work");
        fs::create_dir_all(&dir).unwrap();
        for name in ["a.txt", "b.txt"] {
            fs::write(dir.join(name), name).unwrap();
        }
        fs::write(sb.root.join("outside.txt"), b"outside").unwrap();
        let put = sb.run(&[
            "put",
            &pin,
            dir.join("a.txt").to_str().unwrap(),
            dir.join("b.txt").to_str().unwrap(),
            sb.root.join("outside.txt").to_str().unwrap(),
        ]);
        assert!(put.status.success(), "{mode}: {}", stderr_of(&put));
        let escaped = regex_escape(dir.to_str().unwrap());
        let (flag, pattern) = match mode {
            "unanchored-path" => ("--regex", format!("{}/.*", dir.display())),
            "wild-parent" => ("--regex", r".*/work/.*".to_string()),
            "wrapped-anchor" => ("--regex", format!("(?:^{escaped}/.*)")),
            "alternative-anchor" => ("--regex", format!("QQ|^{escaped}/.*")),
            "inline-flags" => ("--regex", format!("(?i)^{escaped}/.*")),
            "probe-bypass" => ("--regex", format!("^{escaped}/.*(?:o|[^/.]\\.)")),
            "glob-no-separator" => ("--glob", "*work*".to_string()),
            _ => unreachable!("{mode}"),
        };
        let out = common::rtrash(&sb.root)
            .env_remove("HOME")
            .env_remove("USERPROFILE")
            .env_remove("XDG_DATA_HOME")
            .args(["rm", &pin, flag, &pattern])
            .current_dir(sb.work())
            .output()
            .unwrap();
        let mut left = trash_names(&sb);
        left.sort();
        let expected_status = if flag == "--glob" { Some(1) } else { Some(2) };
        if out.status.code() != expected_status
            || left
                != vec![
                    "a.txt".to_string(),
                    "b.txt".to_string(),
                    "outside.txt".to_string(),
                ]
        {
            failures.push(format!(
                "{mode} {pattern}: status {:?}, left {left:?}, stderr {}",
                out.status.code(),
                stderr_of(&out)
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn matching_modes_follow_the_same_rule_with_known_and_unknown_roots() {
    let cases = [
        ("keep-glob", "--glob"),
        ("cache-glob", "--glob"),
        ("object-glob", "--glob"),
        ("contains-o-glob", "--glob"),
        ("narrow-glob", "--glob"),
        ("ancestor-glob", "--glob"),
        ("path-glob", "--glob"),
        ("one-path-glob", "--glob"),
        ("anchored-regex", "--regex"),
        ("escaped-regex", "--regex"),
        ("hidden-regex", "--regex"),
        ("wild-parent-regex", "--regex"),
        ("unanchored-regex", "--regex"),
        ("wrapped-regex", "--regex"),
        ("alternative-regex", "--regex"),
        ("flags-regex", "--regex"),
        ("probe-bypass-regex", "--regex"),
        ("narrow-regex", "--regex"),
        ("object-regex", "--regex"),
        ("letter-regex", "--regex"),
        ("one-path-regex", "--regex"),
        ("directory-substring", "--substring"),
        ("path-substring", "--substring"),
    ];
    for roots_known in [false, true] {
        for (n, (mode, flag)) in cases.into_iter().enumerate() {
            let tag = format!("rmtbl{}-{n}", usize::from(roots_known));
            let sb = Sandbox::new(&tag);
            let pin = format!("--trash-dir={}", sb.trash().display());
            let dir = sb.work().join("wrk");
            let hidden = sb.work().join(".config");
            let one = sb.work().join("one");
            for path in [&dir, &hidden, &one] {
                fs::create_dir_all(path).unwrap();
            }
            fs::create_dir_all(dir.join("nest")).unwrap();
            let files = [
                dir.join("foo"),
                dir.join("notes.txt"),
                dir.join("nest/a.o"),
                dir.join("b.c"),
                dir.join("alpha.txt"),
                dir.join("cache.bin"),
                dir.join("keep.txt"),
                hidden.join("hidden.txt"),
                one.join("only.txt"),
                sb.root.join("spared.txt"),
            ];
            for path in &files {
                fs::write(path, b"x").unwrap();
            }
            let mut put_args = vec!["put".to_string(), pin.clone()];
            put_args.extend(files.iter().map(|path| path.to_string_lossy().into_owned()));
            let put_refs: Vec<&str> = put_args.iter().map(String::as_str).collect();
            let put = sb.run(&put_refs);
            assert!(put.status.success(), "{mode}: {}", stderr_of(&put));

            let dir_text = dir.to_string_lossy();
            let dir_regex = regex_escape(&dir_text);
            let hidden_regex = regex_escape(hidden.to_str().unwrap());
            let one_regex = regex_escape(one.to_str().unwrap());
            let pattern = match mode {
                "keep-glob" => "keep*".to_string(),
                "cache-glob" => "cache*".to_string(),
                "object-glob" => "*.o".to_string(),
                "contains-o-glob" => "*o*".to_string(),
                "narrow-glob" => "a*".to_string(),
                "ancestor-glob" => "*wrk*".to_string(),
                "path-glob" => format!("{dir_text}/*"),
                "one-path-glob" => format!("{}/*", one.display()),
                "anchored-regex" => format!("^{dir_text}/.*"),
                "escaped-regex" => format!("^{dir_regex}/.*"),
                "hidden-regex" => format!("^{hidden_regex}/.*"),
                "wild-parent-regex" => r"^.*/wrk/.*".to_string(),
                "unanchored-regex" => format!("{dir_text}/.*"),
                "wrapped-regex" => format!("(?:^{dir_regex}/.*)"),
                "alternative-regex" => format!("QQ|^{dir_regex}/.*"),
                "flags-regex" => format!("(?i)^{dir_regex}/.*"),
                "probe-bypass-regex" => {
                    format!("^{dir_regex}/.*(?:o|[^/.]\\.)")
                }
                "narrow-regex" => format!("^{dir_regex}/f"),
                "object-regex" => r"\.o$".to_string(),
                "letter-regex" => "o".to_string(),
                "one-path-regex" => format!("^{one_regex}/.*"),
                "directory-substring" => format!("{dir_text}/"),
                "path-substring" => dir.join("notes.txt").to_string_lossy().into_owned(),
                _ => unreachable!("{mode}"),
            };
            let mut cmd = common::rtrash(&sb.root);
            if roots_known {
                let home = sb.root.join("home");
                fs::create_dir_all(&home).unwrap();
                cmd.env("HOME", &home).env("USERPROFILE", &home);
            } else {
                cmd.env_remove("HOME")
                    .env_remove("USERPROFILE")
                    .env_remove("XDG_DATA_HOME");
            }
            let out = cmd
                .args(["rm", &pin, flag, &pattern])
                .current_dir(sb.work())
                .output()
                .unwrap();
            let basename_glob = flag == "--glob" && !pattern.contains(['/', '\\']);
            if !roots_known && !basename_glob {
                let err = stderr_of(&out);
                assert_eq!(out.status.code(), Some(2), "{mode}: {err}");
                assert!(err.contains("When the scope is unknown"), "{mode}: {err}");
                assert_eq!(trash_names(&sb).len(), files.len(), "{mode}: {err}");
            } else if roots_known && mode == "letter-regex" && out.status.code() == Some(2) {
                let err = stderr_of(&out);
                assert!(err.contains("refusing mass pattern"), "{mode}: {err}");
                assert_eq!(trash_names(&sb).len(), files.len(), "{mode}: {err}");
            } else {
                assert_ne!(
                    out.status.code(),
                    Some(2),
                    "{mode} ({pattern}): {}",
                    stderr_of(&out)
                );
                assert!(
                    trash_names(&sb).contains(&"spared.txt".to_string()),
                    "{mode}: {}",
                    stderr_of(&out)
                );
            }
        }
    }
}

#[test]
fn unknown_scope_narrow_patterns_are_deterministic() {
    for n in 0..50 {
        let sb = Sandbox::new(&format!("rmdet-{n}"));
        let pin = format!("--trash-dir={}", sb.trash().display());
        let dir = sb.work().join("wrk");
        fs::create_dir_all(&dir).unwrap();
        for name in ["foo", "a.txt"] {
            fs::write(dir.join(name), name).unwrap();
        }
        fs::write(sb.root.join("outside.txt"), b"outside").unwrap();
        let put = sb.run(&[
            "put",
            &pin,
            dir.join("foo").to_str().unwrap(),
            dir.join("a.txt").to_str().unwrap(),
            sb.root.join("outside.txt").to_str().unwrap(),
        ]);
        assert!(put.status.success(), "{n}: {}", stderr_of(&put));
        let pattern = format!("^{}/f", regex_escape(dir.to_str().unwrap()));
        let regex = common::rtrash(&sb.root)
            .env_remove("HOME")
            .env_remove("USERPROFILE")
            .env_remove("XDG_DATA_HOME")
            .args(["rm", &pin, "--regex", &pattern])
            .current_dir(sb.work())
            .output()
            .unwrap();
        assert_eq!(regex.status.code(), Some(2), "{n}: {}", stderr_of(&regex));
        let glob = common::rtrash(&sb.root)
            .env_remove("HOME")
            .env_remove("USERPROFILE")
            .env_remove("XDG_DATA_HOME")
            .args(["rm", &pin, "--glob", "a*"])
            .current_dir(sb.work())
            .output()
            .unwrap();
        assert!(glob.status.success(), "{n}: {}", stderr_of(&glob));
        let mut left = trash_names(&sb);
        left.sort();
        assert_eq!(
            left,
            vec!["foo".to_string(), "outside.txt".to_string()],
            "{n}"
        );
    }
}

#[test]
fn path_globs_and_substrings_need_force_without_scope_roots() {
    for (n, mode) in ["glob", "substring"].into_iter().enumerate() {
        let tag = format!("rm-narrow-path-{n}");
        let sb = Sandbox::new(&tag);
        let pin = format!("--trash-dir={}", sb.trash().display());
        let dir = sb.work().join("work");
        fs::create_dir_all(&dir).unwrap();
        for name in ["a.c", "b.c", "notes.txt"] {
            fs::write(dir.join(name), name).unwrap();
        }
        fs::write(sb.root.join("outside.txt"), b"outside").unwrap();
        let put = sb.run(&[
            "put",
            &pin,
            dir.join("a.c").to_str().unwrap(),
            dir.join("b.c").to_str().unwrap(),
            dir.join("notes.txt").to_str().unwrap(),
            sb.root.join("outside.txt").to_str().unwrap(),
        ]);
        assert!(put.status.success(), "{mode}: {}", stderr_of(&put));
        let (flag, pattern) = match mode {
            "glob" => ("--glob", format!("{}/*.c", dir.display())),
            "substring" => (
                "--substring",
                dir.join("a.c").to_string_lossy().into_owned(),
            ),
            _ => unreachable!("{mode}"),
        };
        let out = common::rtrash(&sb.root)
            .env_remove("HOME")
            .env_remove("USERPROFILE")
            .env_remove("XDG_DATA_HOME")
            .args(["rm", &pin, flag, &pattern])
            .current_dir(sb.work())
            .output()
            .unwrap();
        let err = stderr_of(&out);
        assert_eq!(out.status.code(), Some(2), "{mode}: {err}");
        assert!(err.contains("When the scope is unknown"), "{mode}: {err}");
        let mut left = trash_names(&sb);
        left.sort();
        assert_eq!(
            left,
            vec![
                "a.c".to_string(),
                "b.c".to_string(),
                "notes.txt".to_string(),
                "outside.txt".to_string()
            ],
            "{mode}"
        );
    }
}

#[test]
fn a_substring_directory_is_refused_when_home_is_unset() {
    let sb = Sandbox::new("rm-substring-nohome");
    let pin = format!("--trash-dir={}", sb.trash().display());
    let dir = sb.work().join("user");
    fs::create_dir_all(&dir).unwrap();
    for name in ["b.log", "z.txt"] {
        fs::write(dir.join(name), name).unwrap();
    }
    sb.touch("outside.txt");
    let put = sb.run(&[
        "put",
        &pin,
        dir.join("b.log").to_str().unwrap(),
        dir.join("z.txt").to_str().unwrap(),
        sb.work().join("outside.txt").to_str().unwrap(),
    ]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let pattern = format!("{}/", dir.display());
    let out = common::rtrash(&sb.root)
        .env_remove("HOME")
        .env_remove("USERPROFILE")
        .env_remove("XDG_DATA_HOME")
        .args(["rm", &pin, "--substring", &pattern])
        .current_dir(sb.work())
        .output()
        .unwrap();
    let err = stderr_of(&out);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("When the scope is unknown"), "{err}");
    let mut left = trash_names(&sb);
    left.sort();
    assert_eq!(
        left,
        vec![
            "b.log".to_string(),
            "outside.txt".to_string(),
            "z.txt".to_string()
        ]
    );
}

#[test]
fn a_substring_directory_stays_narrow_when_scope_roots_are_known() {
    let sb = Sandbox::new("rm-substring-known-home");
    let pin = format!("--trash-dir={}", sb.trash().display());
    let home = sb.work().join("home");
    let other = sb.work().join("other");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&other).unwrap();
    fs::write(home.join("mine.txt"), b"mine").unwrap();
    for name in ["a.txt", "b.txt"] {
        fs::write(other.join(name), name).unwrap();
    }
    sb.touch("outside.txt");
    let put = sb.run(&[
        "put",
        &pin,
        home.join("mine.txt").to_str().unwrap(),
        other.join("a.txt").to_str().unwrap(),
        other.join("b.txt").to_str().unwrap(),
        sb.work().join("outside.txt").to_str().unwrap(),
    ]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let pattern = format!("{}/", other.display());
    let out = common::rtrash(&sb.root)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .args(["rm", &pin, "--substring", &pattern])
        .current_dir(sb.work())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    let mut left = trash_names(&sb);
    left.sort();
    assert_eq!(
        left,
        vec!["mine.txt".to_string(), "outside.txt".to_string()]
    );
}

fn regex_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if "\\.+*?()[]{}^$|".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn home_scope_pattern(home: &str, mode: &str) -> (&'static str, String) {
    let bare = home.trim_start_matches('/');
    let esc = regex_escape(home);
    match mode {
        "dotstar" => ("--regex", format!("^{esc}/.*")),
        "plus" => ("--regex", format!("^{esc}/.+")),
        "any" => ("--regex", format!("{esc}/.*")),
        "class" => ("--regex", format!("^{esc}/[^#]+$")),
        "gstar" => ("--glob", format!("*{home}/*")),
        "gquest" => ("--glob", format!("?{bare}/*")),
        "gclass" => ("--glob", format!("[/]{bare}/*")),
        _ => unreachable!("{mode}"),
    }
}

/// Two files under `HOME` and one outside it. A pattern of the home that
/// the probe list misses must still leave every file.
fn refuse_home_scope(tag: &str, mode: &str) {
    let sb = Sandbox::new(tag);
    let user = sb.work().join("user");
    fs::create_dir_all(&user).unwrap();
    fs::write(user.join("a.txt"), b"a").unwrap();
    fs::write(user.join("b.txt"), b"b").unwrap();
    sb.touch("other.txt");
    let pin = format!("--trash-dir={}", sb.trash().display());
    let put = sb.run(&["put", &pin, "user/a.txt", "user/b.txt", "other.txt"]);
    assert!(put.status.success(), "{tag}: {}", stderr_of(&put));
    let home = fs::canonicalize(&user).unwrap();
    let home = home.to_str().unwrap();
    let (flag, pattern) = home_scope_pattern(home, mode);
    let out = common::rtrash(&sb.root)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .args(["rm", pin.as_str(), flag, pattern.as_str()])
        .current_dir(sb.work())
        .output()
        .unwrap();
    let err = stderr_of(&out);
    assert_eq!(out.status.code(), Some(2), "{tag} {pattern}: {err}");
    assert!(err.contains("refusing mass pattern"), "{tag}: {err}");
    let mut left = trash_names(&sb);
    left.sort();
    assert_eq!(left.len(), 3, "{tag} removed some: {left:?} {err}");
}

#[test]
fn a_home_regex_the_probes_miss_does_not_wipe_home() {
    for (tag, mode) in [
        ("rm-re-home-star", "dotstar"),
        ("rm-re-home-plus", "plus"),
        ("rm-re-home-any", "any"),
        ("rm-re-home-class", "class"),
    ] {
        refuse_home_scope(tag, mode);
    }
}

#[test]
fn a_home_glob_with_no_literal_does_not_wipe_home() {
    for (tag, mode) in [
        ("rm-glob-home-star", "gstar"),
        ("rm-glob-home-quest", "gquest"),
        ("rm-glob-home-class", "gclass"),
    ] {
        refuse_home_scope(tag, mode);
    }
}

#[test]
fn a_broad_pattern_cannot_empty_a_scope_one_item_at_a_time() {
    let sb = Sandbox::new("rm-last-scope-item");
    let home = sb.work().join("user");
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join("a.log"), b"a").unwrap();
    fs::write(home.join("b.log"), b"b").unwrap();
    fs::write(sb.work().join("outside.txt"), b"outside").unwrap();
    let pin = format!("--trash-dir={}", sb.trash().display());
    let put = sb.run(&[
        "put",
        &pin,
        home.join("a.log").to_str().unwrap(),
        home.join("b.log").to_str().unwrap(),
        sb.work().join("outside.txt").to_str().unwrap(),
    ]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let run = |pattern: String| {
        common::rtrash(&sb.root)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .args(["rm", &pin, "--glob", &pattern])
            .current_dir(sb.work())
            .output()
            .unwrap()
    };
    let first = run(format!("{}/a.*", home.display()));
    assert!(first.status.success(), "{}", stderr_of(&first));
    let last = run(format!("{}/b.*", home.display()));
    let err = stderr_of(&last);
    assert_eq!(last.status.code(), Some(2), "{err}");
    assert!(err.contains("refusing mass pattern"), "{err}");
    let mut left = trash_names(&sb);
    left.sort();
    assert_eq!(left, vec!["b.log".to_string(), "outside.txt".to_string()]);
}

#[test]
fn a_prefix_of_the_resolved_home_does_not_wipe_it() {
    let sb = Sandbox::new("rm-prefix-real-home");
    let real = sb.root.join("real_home");
    fs::create_dir_all(&real).unwrap();
    fs::write(real.join("a.txt"), b"a").unwrap();
    fs::write(real.join("b.txt"), b"b").unwrap();
    let link = sb.root.join("link_home");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    sb.touch("other.txt");
    let pin = format!("--trash-dir={}", sb.trash().display());
    let a = real.join("a.txt");
    let b = real.join("b.txt");
    let other = sb.work().join("other.txt");
    let put = sb.run(&[
        "put",
        &pin,
        a.to_str().unwrap(),
        b.to_str().unwrap(),
        other.to_str().unwrap(),
    ]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let real = fs::canonicalize(&real).unwrap();
    let out = common::rtrash(&sb.root)
        .env("HOME", &link)
        .env("USERPROFILE", &link)
        .args(["rm", &pin, "--prefix", real.to_str().unwrap()])
        .current_dir(sb.work())
        .output()
        .unwrap();
    let err = stderr_of(&out);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("refusing mass pattern"), "{err}");
    let mut left = trash_names(&sb);
    left.sort();
    assert_eq!(left.len(), 3, "{left:?} {err}");
}

#[test]
fn a_pattern_of_the_resolved_xdg_data_home_does_not_wipe_it() {
    let sb = Sandbox::new("rm-pattern-real-xdg");
    let real = sb.root.join("real_xdg");
    fs::create_dir_all(&real).unwrap();
    fs::write(real.join("a.txt"), b"a").unwrap();
    fs::write(real.join("b.txt"), b"b").unwrap();
    let link = sb.root.join("link_xdg");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    sb.touch("other.txt");
    let pin = format!("--trash-dir={}", sb.trash().display());
    let put = sb.run(&[
        "put",
        &pin,
        real.join("a.txt").to_str().unwrap(),
        real.join("b.txt").to_str().unwrap(),
        sb.work().join("other.txt").to_str().unwrap(),
    ]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let real = fs::canonicalize(&real).unwrap();
    let pattern = format!("^{}/[^#]+$", regex_escape(real.to_str().unwrap()));
    let out = common::rtrash(&sb.root)
        .env("XDG_DATA_HOME", &link)
        .args(["rm", &pin, "--regex", &pattern])
        .current_dir(sb.work())
        .output()
        .unwrap();
    let err = stderr_of(&out);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("refusing mass pattern"), "{err}");
    let mut left = trash_names(&sb);
    left.sort();
    assert_eq!(left.len(), 3, "{left:?} {err}");
}

fn write_keep(sb: &Sandbox, line: &str) {
    let dir = sb.root.join(".config/rtrash");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("keep"), format!("{line}\n")).unwrap();
}

fn recorded_path(sb: &Sandbox, name: &str) -> String {
    let info = fs::read_to_string(sb.trash().join(format!("info/{name}.trashinfo"))).unwrap();
    info.lines()
        .find_map(|l| l.strip_prefix("Path="))
        .unwrap()
        .to_string()
}

#[test]
fn a_keep_line_through_a_symlinked_parent_protects_the_recorded_path() {
    let sb = Sandbox::new("keep-link-parent");
    let real = sb.work().join("real");
    fs::create_dir_all(&real).unwrap();
    fs::write(real.join("secret"), b"s").unwrap();
    let link = sb.work().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    write_keep(&sb, link.join("secret").to_str().unwrap());
    let pin = format!("--trash-dir={}", sb.trash().display());
    let put = sb.run(&["put", &pin, real.join("secret").to_str().unwrap()]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let recorded = recorded_path(&sb, "secret");
    let out = sb.run(&["rm", &pin, &recorded]);
    let err = stderr_of(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("listed in the keep file"), "{err}");
    assert_eq!(trash_names(&sb).len(), 1, "{err}");
    let json = sb.run(&["rm", &pin, "--json", &recorded]);
    assert!(json.status.success(), "{}", stderr_of(&json));
    let body = stdout_of(&json);
    let value: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("{e}: {body}"));
    assert_eq!(value["removed"], 0, "{body}");
    assert_eq!(value["matches"], serde_json::json!([]), "{body}");
}

#[test]
fn a_keep_line_that_is_a_symlinked_directory_protects_the_recorded_path() {
    let sb = Sandbox::new("keep-link-dir");
    let real = sb.work().join("real_dir");
    fs::create_dir_all(&real).unwrap();
    fs::write(real.join("inside"), b"s").unwrap();
    let link = sb.work().join("link_dir");
    std::os::unix::fs::symlink(fs::canonicalize(&real).unwrap(), &link).unwrap();
    write_keep(&sb, link.to_str().unwrap());
    let pin = format!("--trash-dir={}", sb.trash().display());
    let put = sb.run(&["put", &pin, "-r", real.to_str().unwrap()]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let recorded = recorded_path(&sb, "real_dir");
    let out = sb.run(&["rm", &pin, &recorded]);
    let err = stderr_of(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("listed in the keep file"), "{err}");
    assert!(trash_names(&sb).iter().any(|n| n == "real_dir"), "{err}");
}

#[test]
fn a_keep_line_with_dotdot_across_a_symlink_protects_the_recorded_path() {
    let sb = Sandbox::new("keep-dotdot-link");
    let real = sb.work().join("real");
    fs::create_dir_all(&real).unwrap();
    fs::write(sb.work().join("secret"), b"s").unwrap();
    let hole = sb.work().join("hole");
    fs::create_dir_all(&hole).unwrap();
    let link = hole.join("link");
    std::os::unix::fs::symlink(fs::canonicalize(&real).unwrap(), &link).unwrap();
    write_keep(&sb, link.join("..").join("secret").to_str().unwrap());
    let pin = format!("--trash-dir={}", sb.trash().display());
    let put = sb.run(&["put", &pin, "secret"]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let recorded = recorded_path(&sb, "secret");
    let out = sb.run(&["rm", &pin, &recorded]);
    let err = stderr_of(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("listed in the keep file"), "{err}");
    assert_eq!(trash_names(&sb).len(), 1, "{err}");
}

#[test]
fn a_keep_line_of_the_root_is_an_error_not_a_silent_no_op() {
    let sb = Sandbox::new("keep-root");
    write_keep(&sb, "/");
    sb.touch("only.txt");
    let pin = format!("--trash-dir={}", sb.trash().display());
    assert!(sb.run(&["put", &pin, "only.txt"]).status.success());
    let recorded = recorded_path(&sb, "only.txt");
    let out = sb.run(&["rm", &pin, &recorded]);
    let err = stderr_of(&out);
    assert_ne!(out.status.code(), Some(0), "{err}");
    assert!(err.contains("whole volume"), "{err}");
    assert_eq!(trash_names(&sb), vec!["only.txt".to_string()]);
}

#[test]
fn a_keep_line_of_dot_from_the_root_is_an_error() {
    let sb = Sandbox::new("keep-dot-root");
    write_keep(&sb, ".");
    sb.touch("only.txt");
    let pin = format!("--trash-dir={}", sb.trash().display());
    assert!(sb.run(&["put", &pin, "only.txt"]).status.success());
    let recorded = recorded_path(&sb, "only.txt");
    let out = common::rtrash(&sb.root)
        .args(["rm", pin.as_str(), recorded.as_str()])
        .current_dir("/")
        .output()
        .unwrap();
    let err = stderr_of(&out);
    assert_ne!(out.status.code(), Some(0), "{err}");
    assert!(err.contains("whole volume"), "{err}");
    assert_eq!(trash_names(&sb), vec!["only.txt".to_string()]);
}

#[test]
fn a_narrow_prefix_and_glob_agree_when_only_xdg_is_set() {
    for (tag, glob) in [("rm-xdg-prefix", false), ("rm-xdg-glob", true)] {
        let sb = Sandbox::new(tag);
        fs::create_dir_all(sb.work().join("proj")).unwrap();
        fs::write(sb.work().join("proj/a.txt"), b"a").unwrap();
        sb.touch("other.txt");
        let pin = format!("--trash-dir={}", sb.trash().display());
        let put = sb.run(&["put", &pin, "proj/a.txt", "other.txt"]);
        assert!(put.status.success(), "{tag}: {}", stderr_of(&put));
        let proj = fs::canonicalize(sb.work().join("proj")).unwrap();
        let pattern = if glob {
            format!("{}/*", proj.display())
        } else {
            proj.display().to_string()
        };
        let flag = if glob { "--glob" } else { "--prefix" };
        let out = common::rtrash(&sb.root)
            .env_remove("HOME")
            .env_remove("USERPROFILE")
            .args(["rm", pin.as_str(), flag, pattern.as_str()])
            .current_dir(sb.work())
            .output()
            .unwrap();
        let err = stderr_of(&out);
        assert!(out.status.success(), "{tag}: {err}");
        let mut left = trash_names(&sb);
        left.sort();
        assert_eq!(left, vec!["other.txt".to_string()], "{tag}: {left:?} {err}");
    }
}

fn plant_volumes(sb: &Sandbox) {
    let files = sb.trash().join("files");
    let info = sb.trash().join("info");
    fs::create_dir_all(&files).unwrap();
    fs::create_dir_all(&info).unwrap();
    for (name, path) in [
        ("e-one", r"E:\one.txt"),
        ("e-two", r"E:\two.txt"),
        ("c-other", r"C:\other.txt"),
    ] {
        fs::write(files.join(name), b"x").unwrap();
        fs::write(
            info.join(format!("{name}.trashinfo")),
            format!("[Trash Info]\nPath={path}\nDeletionDate=2026-01-02T03:04:05\n"),
        )
        .unwrap();
    }
}

#[test]
fn a_pattern_of_one_drive_does_not_wipe_it_when_another_drive_has_an_entry() {
    for (tag, flag, pattern) in [
        ("rm-drive-e-glob", "--glob", r"E:\*"),
        ("rm-drive-e-regex", "--regex", r"^E:\\.*"),
    ] {
        let sb = Sandbox::new(tag);
        plant_volumes(&sb);
        let pin = format!("--trash-dir={}", sb.trash().display());
        let out = sb.run(&["rm", &pin, flag, pattern]);
        let err = stderr_of(&out);
        assert_eq!(out.status.code(), Some(2), "{tag}: {err}");
        assert!(err.contains("refusing mass pattern"), "{tag}: {err}");
        let mut left = trash_names(&sb);
        left.sort();
        assert_eq!(
            left,
            vec![
                "c-other".to_string(),
                "e-one".to_string(),
                "e-two".to_string()
            ],
            "{tag}: {left:?} {err}"
        );
    }
}

#[test]
fn one_drive_file_is_removed_when_another_on_that_drive_stays() {
    let sb = Sandbox::new("rm-drive-e-one");
    plant_volumes(&sb);
    let pin = format!("--trash-dir={}", sb.trash().display());
    let out = sb.run(&["rm", &pin, "--glob", r"E:\one.txt"]);
    let err = stderr_of(&out);
    assert!(out.status.success(), "{err}");
    let mut left = trash_names(&sb);
    left.sort();
    assert_eq!(
        left,
        vec!["c-other".to_string(), "e-two".to_string()],
        "{left:?} {err}"
    );
}

#[test]
fn the_exact_path_of_the_only_item_is_removed() {
    let sb = Sandbox::new("rm-exact-only");
    sb.touch("only.txt");
    let pin = format!("--trash-dir={}", sb.trash().display());
    assert!(sb.run(&["put", &pin, "only.txt"]).status.success());
    let recorded = recorded_path(&sb, "only.txt");
    let out = sb.run(&["rm", &pin, &recorded]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(trash_names(&sb).is_empty());
}

#[test]
fn every_item_but_one_is_still_removed() {
    let sb = Sandbox::new("rm-all-but-one");
    sb.touch("a.txt");
    sb.touch("b.txt");
    sb.touch("keep.dat");
    let pin = format!("--trash-dir={}", sb.trash().display());
    assert!(sb
        .run(&["put", &pin, "a.txt", "b.txt", "keep.dat"])
        .status
        .success());
    let out = sb.run(&["rm", &pin, "--glob", "*.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(trash_names(&sb), vec!["keep.dat".to_string()]);
}

#[test]
fn an_id_selects_only_that_entry() {
    let sb = Sandbox::new("rm-id-one");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("same.txt");
    assert!(sb.run(&["put", &pin, "same.txt"]).status.success());
    sb.touch("same.txt");
    assert!(sb.run(&["put", &pin, "same.txt"]).status.success());
    let mut names = trash_names(&sb);
    names.sort();
    assert_eq!(names.len(), 2, "{names:?}");
    let id = format!("--id={}", names[0]);
    let path = sb.work().join("same.txt");
    let out = sb.run(&["rm", &pin, &id, path.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let mut left = trash_names(&sb);
    left.sort();
    assert_eq!(left, vec![names[1].clone()]);
}

#[test]
fn batch_last_with_an_id_removes_only_that_item() {
    let sb = Sandbox::new("rm-batch-id");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("a.txt");
    sb.touch("b.txt");
    assert!(sb.run(&["put", &pin, "a.txt", "b.txt"]).status.success());
    let out = sb.run(&["rm", &pin, "--batch=last", "--id=a.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(trash_names(&sb), vec!["b.txt".to_string()]);
    let rest = sb.run(&["rm", &pin, "--batch=last"]);
    assert!(rest.status.success(), "{}", stderr_of(&rest));
    assert!(trash_names(&sb).is_empty());
}

#[test]
fn keep_is_counted_before_the_cap() {
    let sb = Sandbox::new("rm-keep-cap");
    fs::create_dir_all(sb.root.join(".config/rtrash")).unwrap();
    let kept = sb.work().join("kept.txt");
    fs::write(&kept, b"k").unwrap();
    fs::write(sb.work().join("drop.txt"), b"d").unwrap();
    fs::write(sb.work().join("plain.dat"), b"p").unwrap();
    fs::write(
        sb.root.join(".config/rtrash/keep"),
        format!("{}\n", kept.display()),
    )
    .unwrap();
    let pin = format!("--trash-dir={}", sb.trash().display());
    assert!(sb
        .run(&["put", &pin, "kept.txt", "drop.txt", "plain.dat"])
        .status
        .success());
    let out = sb.run(&["rm", &pin, "--max-count=1", "--glob", "*.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains("listed in the keep file"));
    let mut left = trash_names(&sb);
    left.sort();
    assert_eq!(left, vec!["kept.txt".to_string(), "plain.dat".to_string()]);
}

#[test]
fn keep_is_left_out_of_the_question() {
    let sb = Sandbox::new("rm-keep-ask");
    fs::create_dir_all(sb.root.join(".config/rtrash")).unwrap();
    let kept = sb.work().join("kept.txt");
    fs::write(&kept, b"k").unwrap();
    fs::write(sb.work().join("drop.txt"), b"d").unwrap();
    fs::write(sb.work().join("plain.dat"), b"p").unwrap();
    fs::write(
        sb.root.join(".config/rtrash/keep"),
        format!("{}\n", kept.display()),
    )
    .unwrap();
    let pin = format!("--trash-dir={}", sb.trash().display());
    assert!(sb
        .run(&["put", &pin, "kept.txt", "drop.txt", "plain.dat"])
        .status
        .success());
    let out = sb.run_with_stdin(&["rm", &pin, "-i", "--glob", "*.txt"], "y\n");
    let err = stderr_of(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("Permanently remove 1 item"), "{err}");
    assert!(err.contains("listed in the keep file"), "{err}");
    let mut left = trash_names(&sb);
    left.sort();
    assert_eq!(left, vec!["kept.txt".to_string(), "plain.dat".to_string()]);
}

#[test]
fn a_kept_file_keeps_the_directory_that_holds_it() {
    let sb = Sandbox::new("keep-inside");
    fs::create_dir_all(sb.root.join(".config/rtrash")).unwrap();
    let dir = sb.work().join("dir");
    fs::create_dir_all(&dir).unwrap();
    let secret = dir.join("secret");
    fs::write(&secret, b"s").unwrap();
    fs::write(dir.join("other"), b"o").unwrap();
    fs::write(
        sb.root.join(".config/rtrash/keep"),
        format!("{}\n", secret.display()),
    )
    .unwrap();
    let pin = format!("--trash-dir={}", sb.trash().display());
    assert!(sb.run(&["put", &pin, "-r", "dir"]).status.success());
    let out = sb.run(&["rm", &pin, dir.to_str().unwrap()]);
    let err = stderr_of(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("listed in the keep file"), "{err}");
    assert!(
        trash_names(&sb).iter().any(|n| n == "dir"),
        "{:?}",
        trash_names(&sb)
    );
    let empty = sb.run(&["empty", &pin, "--max-size=0"]);
    assert!(empty.status.success(), "{}", stderr_of(&empty));
    assert!(
        trash_names(&sb).iter().any(|n| n == "dir"),
        "empty: {:?}",
        trash_names(&sb)
    );
}

#[test]
fn a_relative_keep_path_protects_the_file_in_the_working_directory() {
    let sb = Sandbox::new("keep-relative");
    fs::create_dir_all(sb.root.join(".config/rtrash")).unwrap();
    fs::write(sb.root.join(".config/rtrash/keep"), "secret.txt\n").unwrap();
    sb.touch("secret.txt");
    sb.touch("other.txt");
    let pin = format!("--trash-dir={}", sb.trash().display());
    assert!(sb
        .run(&["put", &pin, "secret.txt", "other.txt"])
        .status
        .success());
    let secret = sb.work().join("secret.txt");
    let out = sb.run(&["rm", &pin, secret.to_str().unwrap()]);
    let err = stderr_of(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("listed in the keep file"), "{err}");
    assert!(trash_names(&sb).iter().any(|n| n == "secret.txt"));
    let other = sb.work().join("other.txt");
    let gone = sb.run(&["rm", &pin, other.to_str().unwrap()]);
    assert!(gone.status.success(), "{}", stderr_of(&gone));
    assert!(!trash_names(&sb).iter().any(|n| n == "other.txt"));
    assert!(trash_names(&sb).iter().any(|n| n == "secret.txt"));
}

#[test]
fn reap_keeps_a_directory_that_holds_a_kept_file() {
    let sb = Sandbox::new("keep-reap-inside");
    fs::create_dir_all(sb.root.join(".config/rtrash")).unwrap();
    let scratch = sb.work().join("scratch");
    let cache = scratch.join("cache");
    fs::create_dir_all(&cache).unwrap();
    let secret = cache.join("secret");
    fs::write(&secret, b"s").unwrap();
    fs::write(cache.join("old"), b"o").unwrap();
    age_tree(&cache);
    fs::write(
        sb.root.join(".config/rtrash/keep"),
        format!("{}\n", secret.display()),
    )
    .unwrap();
    let out = sb.run(&[
        "reap",
        "--permanent",
        "--older-than=1h",
        "--age-by=m",
        scratch.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "{}{}",
        stderr_of(&out),
        stdout_of(&out)
    );
    assert!(secret.exists(), "{}{}", stderr_of(&out), stdout_of(&out));
    assert!(cache.exists(), "{}{}", stderr_of(&out), stdout_of(&out));
}

#[test]
fn rm_refuses_two_spellings_of_one_original() {
    let sb = Sandbox::new("rm-match-key");
    let pin = format!("--trash-dir={}", sb.trash().display());
    let real = sb.work().join("real");
    fs::create_dir(&real).unwrap();
    fs::write(real.join("a.txt"), b"payload").unwrap();
    let put = sb.run(&["put", &pin, "real/a.txt"]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let info_path = sb.trash().join("info/a.txt.trashinfo");
    let info = fs::read_to_string(&info_path).unwrap();
    let recorded = info
        .lines()
        .find_map(|l| l.strip_prefix("Path="))
        .unwrap()
        .to_string();
    let link = sb.work().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let via_link = link.join("a.txt");
    let via_link = via_link.to_str().unwrap();
    let alt = info.replacen(&format!("Path={recorded}"), &format!("Path={via_link}"), 1);
    fs::write(sb.trash().join("info/a-link.txt.trashinfo"), alt).unwrap();
    fs::copy(
        sb.trash().join("files/a.txt"),
        sb.trash().join("files/a-link.txt"),
    )
    .unwrap();
    let out = sb.run(&["rm", &pin, via_link]);
    let err = stderr_of(&out);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("refusing 2 trash items"), "{err}");
    assert!(err.contains("--all-matches"), "{err}");
    let mut names = trash_names(&sb);
    names.sort();
    assert_eq!(names, vec!["a-link.txt".to_string(), "a.txt".to_string()]);
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
    let out = sb.run(&["rm", "-m", "substring", "old-a"]);
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
    // A shorter path is not the file. new.txt stays, and is named.
    let out = sb.run(&["rm", "--match=exact", "new"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains("near miss"), "{}", stderr_of(&out));
    assert_eq!(trash_names(&sb), ["new.txt"]);
    // A substring every path holds needs --force; a bad kind exits 2.
    let mass = sb.run(&["rm", "-m", "substring", "/"]);
    assert_eq!(mass.status.code(), Some(2), "{}", stderr_of(&mass));
    let bad = sb.run(&["rm", "--match=fuzzy", "x"]);
    assert_eq!(bad.status.code(), Some(2), "{}", stderr_of(&bad));
    assert_eq!(trash_names(&sb), ["new.txt"]);
}

/// Original paths `list --json` shows, in list order. `rm --match=exact`
/// compares against these, not against a basename or a glob.
fn listed_originals(sb: &Sandbox) -> Vec<String> {
    let text = stdout_of(&sb.run(&["list", "--home-only", "--json"]));
    let key = "\"original\":\"";
    let mut rest = text.as_str();
    let mut out = Vec::new();
    while let Some(i) = rest.find(key) {
        rest = &rest[i + key.len()..];
        let end = rest.find('"').expect("original value");
        out.push(rest[..end].to_string());
        rest = &rest[end..];
    }
    out
}

/// A name that is itself a glob matches only that listed path. The exact
/// default takes the file named `*.log` and leaves `notes.log`. A glob of
/// every item in the trash is a mass delete and waits for `--force`.
#[test]
fn rm_exact_path_keeps_a_glob_name_to_one_item() {
    let sb = Sandbox::new("rm-exact-glob");
    fs::write(sb.work().join("*.log"), b"star").unwrap();
    fs::write(sb.work().join("notes.log"), b"notes").unwrap();
    assert!(sb
        .run(&["put", "--home-only", "*.log", "notes.log"])
        .status
        .success());
    let dry = sb.run(&["rm", "--home-only", "-n", "*.log"]);
    let shown = format!("{}{}", stdout_of(&dry), stderr_of(&dry));
    assert!(
        shown.contains("*.log")
            && !shown.contains("notes.log")
            && shown.contains("Would permanently remove 1 item"),
        "{shown}"
    );
    let globbed = sb.run(&["rm", "--home-only", "-n", "--glob", "*.log"]);
    assert_eq!(globbed.status.code(), Some(2), "{}", stderr_of(&globbed));
    assert!(
        stderr_of(&globbed).contains("refusing mass pattern"),
        "{}",
        stderr_of(&globbed)
    );
    assert_eq!(listed_originals(&sb).len(), 2);
    let forced = sb.run(&["rm", "--home-only", "-n", "--force", "--glob", "*.log"]);
    let both = format!("{}{}", stdout_of(&forced), stderr_of(&forced));
    assert!(
        both.contains("*.log")
            && both.contains("notes.log")
            && both.contains("Would permanently remove 2 items"),
        "{both}"
    );
    let star = listed_originals(&sb)
        .into_iter()
        .find(|p| p.ends_with("/*.log"))
        .expect("listed *.log");
    let asked = sb.run_with_stdin(
        &[
            "rm",
            "--home-only",
            "-i",
            "--max-count=1",
            "--match=exact",
            &star,
        ],
        "",
    );
    let err = stderr_of(&asked);
    assert!(
        err.contains("(y/N)") && err.contains("nothing removed"),
        "{err}"
    );
    assert_eq!(listed_originals(&sb).len(), 2);
    let yes = sb.run_with_stdin(
        &[
            "rm",
            "--home-only",
            "-i",
            "--max-count=1",
            "--match=exact",
            &star,
        ],
        "y\n",
    );
    assert!(yes.status.success(), "{}", stderr_of(&yes));
    let left = listed_originals(&sb);
    assert!(left.iter().all(|p| !p.ends_with("/*.log")), "{left:?}");
    assert!(left.iter().any(|p| p.ends_with("/notes.log")), "{left:?}");
}

/// Two trashed files with the same basename: a glob of that name matches
/// both deliberately even when that is the whole trash, and the listed
/// original path of one removes only that one.
#[test]
fn rm_exact_path_keeps_a_duplicate_basename() {
    let sb = Sandbox::new("rm-exact-dup");
    fs::create_dir(sb.work().join("a")).unwrap();
    fs::create_dir(sb.work().join("b")).unwrap();
    fs::write(sb.work().join("a/foo"), b"a").unwrap();
    fs::write(sb.work().join("b/foo"), b"b").unwrap();
    assert!(sb
        .run(&["put", "--home-only", "a/foo", "b/foo"])
        .status
        .success());
    let dry = sb.run(&["rm", "--home-only", "-n", "--glob", "foo"]);
    assert!(dry.status.success(), "{}", stderr_of(&dry));
    assert!(
        stderr_of(&dry).contains("Would permanently remove 2 items"),
        "{}",
        stderr_of(&dry)
    );
    let forced = sb.run(&["rm", "--home-only", "-n", "--force", "--glob", "foo"]);
    assert!(
        stderr_of(&forced).contains("Would permanently remove 2 items"),
        "{}",
        stderr_of(&forced)
    );
    let wanted = listed_originals(&sb)
        .into_iter()
        .find(|p| p.ends_with("/a/foo"))
        .expect("listed a/foo");
    let yes = sb.run_with_stdin(
        &[
            "rm",
            "--home-only",
            "-i",
            "--max-count=1",
            "--match=exact",
            &wanted,
        ],
        "y\n",
    );
    assert!(yes.status.success(), "{}", stderr_of(&yes));
    let left = listed_originals(&sb);
    assert_eq!(left.len(), 1, "{left:?}");
    assert!(left[0].ends_with("/b/foo"), "{left:?}");
}

#[test]
fn rm_exact_with_no_match_exits_one() {
    let sb = Sandbox::new("rm-exact-no-match");
    sb.touch("kept.txt");
    assert!(sb.run(&["put", "--home-only", "kept.txt"]).status.success());
    let missing = sb.work().join("missing.txt");
    let out = sb.run(&[
        "rm",
        "--home-only",
        "--json",
        "--match=exact",
        missing.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("no trash items match"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(trash_names(&sb), vec!["kept.txt".to_string()]);
}

/// Two copies of one original path: an exact match refuses both until
/// `--all-matches`, and `--max-count=1` then deletes neither.
#[test]
fn rm_exact_path_stops_at_one_copy_of_a_path() {
    let sb = Sandbox::new("rm-exact-cap");
    sb.touch("same.txt");
    assert!(sb.run(&["put", "--home-only", "same.txt"]).status.success());
    sb.touch("same.txt");
    assert!(sb.run(&["put", "--home-only", "same.txt"]).status.success());
    let paths = listed_originals(&sb);
    assert_eq!(paths.len(), 2, "{paths:?}");
    assert!(paths.iter().all(|p| p.ends_with("/same.txt")), "{paths:?}");
    let dry = sb.run(&["rm", "--home-only", "-n", "--match=exact", &paths[0]]);
    assert_eq!(dry.status.code(), Some(2), "{}", stderr_of(&dry));
    assert!(
        stderr_of(&dry).contains("select one with --id=NAME, or pass --all-matches"),
        "{}",
        stderr_of(&dry)
    );
    assert_eq!(listed_originals(&sb).len(), 2);
    let all = sb.run(&[
        "rm",
        "--home-only",
        "-n",
        "--all-matches",
        "--match=exact",
        &paths[0],
    ]);
    assert!(
        stderr_of(&all).contains("Would permanently remove 2 items"),
        "{}",
        stderr_of(&all)
    );
    let out = sb.run_with_stdin(
        &[
            "rm",
            "--home-only",
            "-i",
            "--max-count=1",
            "--all-matches",
            "--match=exact",
            &paths[0],
        ],
        "y\n",
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("more than the cap of 1"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(listed_originals(&sb).len(), 2);
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
        "--match=glob",
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
            sb.run(&["rm", &pin, bad, "--glob", "*"]).status.code(),
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
fn restore_plain_order_takes_direction_flags_and_the_date_alias() {
    let sb = Sandbox::new("restore-order");
    let old = sb.touch("old.txt");
    let new = sb.touch("new.txt");
    assert!(sb.run(&["put", "old.txt", "new.txt"]).status.success());
    backdate_trashinfo(&sb, "old.txt", "2001-01-01T00:00:00");
    let pin = format!("--trash-dir={}", sb.trash().display());

    // Numbered table defaults oldest first: index 0 is the backdated item,
    // and the newest row is last, next to the index prompt.
    let out = sb.run_with_stdin(&["restore", &pin, "--plain"], "0\n");
    assert!(out.status.success(), "{}", stderr_of(&out));
    let table = stdout_of(&out);
    let rows: Vec<_> = table
        .lines()
        .filter(|l| l.trim_start().starts_with(|c: char| c.is_ascii_digit()))
        .collect();
    assert!(
        rows.first().is_some_and(|l| l.contains("old.txt")),
        "{table}"
    );
    assert!(
        rows.last().is_some_and(|l| l.contains("new.txt")),
        "{table}"
    );
    assert!(old.exists() && !new.exists());

    // Both items back in the trash. --oldest-first keeps the oldest at 0.
    assert!(sb.run(&["put", "old.txt"]).status.success());
    backdate_trashinfo(&sb, "old.txt", "2001-01-01T00:00:00");
    let out = sb.run_with_stdin(&["restore", &pin, "--plain", "--oldest-first"], "0\n");
    assert!(out.status.success(), "{}", stderr_of(&out));
    let table = stdout_of(&out);
    let first = table.lines().next().unwrap_or("");
    assert!(first.contains("old.txt"), "{table}");
    assert!(old.exists() && !new.exists());

    // --newest-first puts the latest item at index 0.
    assert!(sb.run(&["put", "old.txt"]).status.success());
    backdate_trashinfo(&sb, "old.txt", "2001-01-01T00:00:00");
    let out = sb.run_with_stdin(&["restore", &pin, "--plain", "--newest-first"], "0\n");
    assert!(out.status.success(), "{}", stderr_of(&out));
    let table = stdout_of(&out);
    let first = table.lines().next().unwrap_or("");
    assert!(first.contains("new.txt"), "{table}");
    assert!(new.exists() && !old.exists());

    // --sort=oldest takes index 0 from the oldest row.
    assert!(sb.run(&["put", "new.txt"]).status.success());
    let out = sb.run_with_stdin(&["restore", &pin, "--plain", "--sort=oldest"], "0\n");
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(old.exists() && !new.exists());

    // trash-restore compatibility: --sort=date is an alias of oldest.
    assert!(sb.run(&["put", "old.txt"]).status.success());
    backdate_trashinfo(&sb, "old.txt", "2001-01-01T00:00:00");
    let out = sb.run_with_stdin(&["restore", &pin, "--plain", "--sort=date"], "0\n");
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(old.exists() && !new.exists());

    // --sort=newest takes index 0 from the newest row.
    assert!(sb.run(&["put", "old.txt"]).status.success());
    backdate_trashinfo(&sb, "old.txt", "2001-01-01T00:00:00");
    let out = sb.run_with_stdin(&["restore", &pin, "--plain", "--sort=newest"], "0\n");
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(new.exists() && !old.exists());
}

/// Three deletion dates: the numbered table is oldest first, so the newest
/// row is last, next to where the index prompt is printed. `--sort=newest`
/// puts that row at index 0.
#[test]
fn restore_plain_date_order_is_oldest_first_and_sort_newest_flips_it() {
    let sb = Sandbox::new("restore-date-rows");
    sb.touch("old.txt");
    sb.touch("mid.txt");
    sb.touch("new.txt");
    assert!(sb
        .run(&["put", "old.txt", "mid.txt", "new.txt"])
        .status
        .success());
    backdate_trashinfo(&sb, "old.txt", "2001-01-01T00:00:00");
    backdate_trashinfo(&sb, "mid.txt", "2010-06-15T12:00:00");
    backdate_trashinfo(&sb, "new.txt", "2020-03-03T03:03:03");
    let pin = format!("--trash-dir={}", sb.trash().display());
    let names = |text: &str| -> Vec<&str> {
        text.lines()
            .filter(|l| l.trim_start().starts_with(|c: char| c.is_ascii_digit()))
            .filter_map(|l| {
                ["old.txt", "mid.txt", "new.txt"]
                    .into_iter()
                    .find(|n| l.contains(n))
            })
            .collect()
    };
    let table = |args: &[&str]| {
        let mut full = vec!["restore", &pin, "--plain", "--dry-run"];
        full.extend_from_slice(args);
        let out = sb.run_with_stdin(&full, "0\n");
        assert!(out.status.success(), "{}", stderr_of(&out));
        stdout_of(&out)
    };

    let plain = table(&[]);
    assert_eq!(
        names(&plain),
        ["old.txt", "mid.txt", "new.txt"],
        "default oldest-first, newest row last: {plain}"
    );
    let newest = table(&["--sort=newest"]);
    assert_eq!(
        names(&newest),
        ["new.txt", "mid.txt", "old.txt"],
        "--sort=newest puts the latest row at index 0: {newest}"
    );
    let oldest = table(&["--sort=oldest"]);
    assert_eq!(
        names(&oldest),
        ["old.txt", "mid.txt", "new.txt"],
        "{oldest}"
    );
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
    // Date order defaults oldest first.
    let out = stdout_of(&sb.run(&["list", &pin]));
    let rows: Vec<&str> = out.lines().collect();
    assert!(rows.first().is_some_and(|r| r.ends_with("/big")), "{out}");
    let out = stdout_of(&sb.run(&["list", &pin, "--sort=newest"]));
    let rows: Vec<&str> = out.lines().collect();
    assert!(rows.first().is_some_and(|r| r.ends_with("/small")), "{out}");
    let out = stdout_of(&sb.run(&["list", &pin, "--sort=oldest"]));
    let rows: Vec<&str> = out.lines().collect();
    assert!(rows.first().is_some_and(|r| r.ends_with("/big")), "{out}");
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
    let dry = sb.run(&[
        "rm",
        &pin,
        "--match=glob",
        "-n",
        "--smaller-than=32K",
        "--force",
        "*",
    ]);
    assert_eq!(stdout_of(&dry).lines().count(), 1, "{}", stdout_of(&dry));
    assert!(stdout_of(&dry).trim_end().ends_with("small"));
    let gone = sb.run(&[
        "rm",
        &pin,
        "--match=glob",
        "--larger-than=100K",
        "--force",
        "*",
    ]);
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
fn size_units_in_any_case_filter_alike() {
    let sb = Sandbox::new("size-units");
    fs::write(sb.work().join("big"), vec![1u8; 64 * 1024]).unwrap();
    fs::write(sb.work().join("small"), b"x").unwrap();
    assert!(sb.run(&["put", "big", "small"]).status.success());
    let pin = format!("--trash-dir={}", sb.trash().display());
    let listed = |size: &str| -> Vec<String> {
        let flag = format!("--larger-than={size}");
        let out = sb.run(&["list", &pin, &flag]);
        assert!(out.status.success(), "{size}: {}", stderr_of(&out));
        stdout_of(&out)
            .lines()
            .map(|l| l.rsplit(['/', '\\']).next().unwrap_or("").to_string())
            .collect()
    };
    let upper = listed("32K");
    assert_eq!(upper, ["big"]);
    // Every spelling of 32 KiB keeps the same rows.
    for size in ["32k", "32kb", "32KB", "32kib", "32KiB", "32768", "3.2768e4"] {
        assert_eq!(listed(size), upper, "{size}");
    }
    for bad in ["32kbb", "32iB", "32X"] {
        let flag = format!("--larger-than={bad}");
        assert_eq!(
            sb.run(&["list", &pin, &flag]).status.code(),
            Some(2),
            "{bad}"
        );
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
    let original = sb.work().join("via-trash.txt");
    let out = run_as("trash-rm", &[original.to_str().unwrap()]);
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
    assert_hint(
        &stderr_of(&out),
        "rtrash list",
        &trash_root,
        "3 trash records could not be listed; `rtrash status --check` shows them",
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
    assert_hint(
        lines[1],
        "trash-list",
        &trash_root,
        "2 trash records could not be listed; `rtrash status --check` shows them",
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
    assert_hint(
        &stderr_of(&out),
        "rtrash list",
        &t,
        "1 trash record could not be listed; `rtrash status --check` shows it",
    );
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
    assert_hint(
        lines[3],
        "trash-list",
        &t,
        "2 trash records could not be listed; `rtrash status --check` shows them",
    );

    let out = sb.run(&["list", &pin]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(out.stdout, clean.stdout);
    assert_hint(
        &stderr_of(&out),
        "rtrash list",
        &t,
        "5 trash records could not be listed; `rtrash status --check` shows them",
    );
}

/// `text` is the one line `list` prints for the trash at `root`:
/// `PROG: ROOT: WHAT`. The root may be printed resolved (a volume trash's
/// top), so only its last component is matched.
fn assert_hint(text: &str, prog: &str, root: &Path, what: &str) {
    let line = text.strip_suffix('\n').unwrap_or(text);
    assert!(!line.contains('\n'), "exactly one hint line: {text:?}");
    let leaf = root.file_name().unwrap().to_string_lossy();
    assert!(
        line.starts_with(&format!("{prog}: /")) && line.ends_with(&format!("{leaf}: {what}")),
        "{text:?}"
    );
}

/// trash-cli #428: one line per trash folder that has records `list`
/// left out or names in `info/` that are not records, after the rows and
/// in the order of the trashes, and nothing for a clean trash. Under the
/// trash-list name a name that is not a record gets no line, as in
/// trash-cli 0.26.9.29.
#[test]
fn list_prints_one_hint_line_per_trash_folder_with_broken_records() {
    let sb = Sandbox::new("list-folder-hints");
    let make = |name: &str| {
        let t = sb.root.join(name);
        fs::create_dir_all(t.join("files")).unwrap();
        fs::create_dir_all(t.join("info")).unwrap();
        fs::write(t.join("files/ok.txt"), b"x").unwrap();
        fs::write(
            t.join("info/ok.txt.trashinfo"),
            "[Trash Info]\nPath=/tmp/ok.txt\nDeletionDate=2026-01-02T03:04:05\n",
        )
        .unwrap();
        t
    };
    let (a, b, c) = (make("trash-a"), make("trash-b"), make("trash-c"));
    let pins: Vec<String> = [&a, &b, &c]
        .iter()
        .map(|t| format!("--trash-dir={}", t.display()))
        .collect();
    let mut args = vec!["list"];
    args.extend(pins.iter().map(String::as_str));
    let clean = sb.run(&args);
    assert!(clean.status.success(), "{}", stderr_of(&clean));
    assert_eq!(stderr_of(&clean), "", "clean trashes get no line");

    // a: two records with no Path= line; c: a .DS_Store and one such record.
    for (t, name) in [(&a, "p1"), (&a, "p2"), (&c, "p3")] {
        let record = t.join("info").join(format!("{name}.trashinfo"));
        fs::write(record, "[Trash Info]\nDeletionDate=2026-01-02T03:04:05\n").unwrap();
    }
    fs::write(c.join("info/.DS_Store"), b"Bud1").unwrap();
    let out = sb.run(&args);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(out.stdout, clean.stdout, "the rows stay the same");
    let err = stderr_of(&out);
    let lines: Vec<&str> = err.lines().collect();
    assert_eq!(lines.len(), 2, "one line per folder with problems: {err}");
    assert_hint(
        lines[0],
        "rtrash list",
        &a,
        "2 trash records could not be listed; `rtrash status --check` shows them",
    );
    assert_hint(
        lines[1],
        "rtrash list",
        &c,
        "1 trash record could not be listed, and 1 name in info/ is not a trash record; \
         `rtrash status --check` shows them",
    );

    // Only the stray name left: its own line, and none under trash-list.
    fs::remove_file(c.join("info/p3.trashinfo")).unwrap();
    for name in ["p1", "p2"] {
        fs::remove_file(a.join("info").join(format!("{name}.trashinfo"))).unwrap();
    }
    let out = sb.run(&args);
    assert_hint(
        &stderr_of(&out),
        "rtrash list",
        &c,
        "1 name in info/ is not a trash record; `rtrash status --check` shows it",
    );
    let check = sb.run(&["status", "--check", &pins[2]]);
    let report = format!("{}{}", stdout_of(&check), stderr_of(&check));
    assert!(
        report.contains("stray:") && report.contains(".DS_Store"),
        "{report}"
    );
    let bindir = sb.root.join("bin");
    fs::create_dir_all(&bindir).unwrap();
    std::os::unix::fs::symlink(bin(), bindir.join("trash-list")).unwrap();
    let cli = Command::new(bindir.join("trash-list"))
        .args(&pins)
        .current_dir(sb.work())
        .env("XDG_DATA_HOME", sb.root.join("xdg"))
        .env("HOME", &sb.root)
        .output()
        .unwrap();
    assert!(cli.status.success(), "{}", stderr_of(&cli));
    assert_eq!(
        stderr_of(&cli),
        "",
        "trash-cli says nothing of a stray name"
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
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
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
    let out = sb.run(&["rm", "--home-only", "--match=glob", "*.o"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let mut names = trash_names(&sb);
    names.sort();
    assert_eq!(names, vec!["keep.dat"], "only non-matching entry remains");
    assert!(sb.trash().join("info/keep.dat.trashinfo").exists());
    assert!(!sb.trash().join("info/drop.o.trashinfo").exists());
    assert!(!sb.trash().join("files/drop.o").exists());
}

/// A bare path is the original path put recorded. A sibling and a child
/// stay, and a second try names them instead of removing them.
#[test]
fn rm_matches_the_original_path_exactly() {
    let sb = Sandbox::new("exact-rm");
    fs::create_dir(sb.work().join("a")).unwrap();
    fs::write(sb.work().join("a/b"), b"child").unwrap();
    assert!(sb.run(&["put", "a/b"]).status.success());
    fs::remove_dir(sb.work().join("a")).unwrap();
    fs::write(sb.work().join("a"), b"exact").unwrap();
    fs::write(sb.work().join("ab"), b"sib").unwrap();
    assert!(sb.run(&["put", "a", "ab"]).status.success());

    let dry = sb.run(&["rm", "--home-only", "-n", "a/"]);
    assert!(dry.status.success(), "{}", stderr_of(&dry));
    let shown = stdout_of(&dry);
    let lines: Vec<_> = shown.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1, "{}", stdout_of(&dry));
    assert!(lines[0].ends_with("/a"), "{}", stdout_of(&dry));

    let sub = sb.work().join("elsewhere");
    fs::create_dir(&sub).unwrap();
    let from = common::rtrash(&sb.root)
        .args(["rm", "--home-only", "-n", "../a"])
        .current_dir(&sub)
        .output()
        .unwrap();
    assert!(from.status.success(), "{}", stderr_of(&from));
    assert_eq!(
        stdout_of(&from).lines().filter(|l| !l.is_empty()).count(),
        1,
        "{}",
        stdout_of(&from)
    );

    let out = sb.run(&["rm", "--home-only", "a"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let mut names = trash_names(&sb);
    names.sort();
    assert_eq!(names, vec!["ab".to_string(), "b".to_string()]);

    let dry_again = sb.run(&["rm", "--home-only", "-n", "a"]);
    assert_eq!(
        dry_again.status.code(),
        Some(1),
        "{}",
        stderr_of(&dry_again)
    );
    let dry_err = stderr_of(&dry_again);
    assert!(dry_err.contains("no exact match"), "{dry_err}");
    assert!(dry_err.contains("near miss"), "{dry_err}");

    let again = sb.run(&["rm", "--home-only", "a"]);
    assert_eq!(again.status.code(), Some(1), "{}", stderr_of(&again));
    let err = stderr_of(&again);
    assert!(err.contains("no exact match"), "{err}");
    assert!(err.contains("near miss"), "{err}");
    assert!(err.contains("/ab"), "{err}");
    assert!(err.contains("/a/b"), "{err}");

    for args in [
        &["rm", "--home-only", "--json", "a"][..],
        &["rm", "--home-only", "-n", "--json", "a"][..],
    ] {
        let out = sb.run(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", stderr_of(&out));
        let body = stdout_of(&out);
        let value: serde_json::Value =
            serde_json::from_str(&body).unwrap_or_else(|e| panic!("{args:?}: {e}: {body}"));
        assert_eq!(value["dry_run"], args.contains(&"-n"), "{body}");
        assert_eq!(value["removed"], 0, "{body}");
        assert_eq!(value["bytes"], 0, "{body}");
        assert_eq!(value["matches"], serde_json::json!([]), "{body}");
        assert!(
            stderr_of(&out).contains("no trash items match"),
            "{args:?}: {}",
            stderr_of(&out)
        );
    }
    assert_eq!(trash_names(&sb).len(), 2);
}

/// A composed `é` and a decomposed one are two files on a file system that
/// keeps both. `rm` of the composed spelling removes that item and leaves
/// the decomposed one.
#[test]
fn rm_removes_one_spelling_of_an_accented_name() {
    let sb = Sandbox::new("exact-nfc");
    let composed = "item-\u{e9}.txt";
    let decomposed = "item-e\u{301}.txt";
    let a = sb.work().join(composed);
    let b = sb.work().join(decomposed);
    fs::write(&a, b"composed").unwrap();
    fs::write(&b, b"decomposed").unwrap();
    if fs::read(&a).unwrap() == b"decomposed" {
        common::skip("this file system stores one Unicode spelling");
        return;
    }
    assert!(
        sb.run(&["put", composed, decomposed]).status.success(),
        "put failed"
    );
    assert_eq!(trash_names(&sb).len(), 2, "put did not keep both spellings");
    let out = sb.run(&["rm", "--home-only", composed]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let left: Vec<_> = fs::read_dir(sb.trash().join("files"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(left.len(), 1, "{}", stderr_of(&out));
    assert_eq!(fs::read(&left[0]).unwrap(), b"decomposed");
}

#[test]
fn glob_matches_an_nfc_pattern_to_an_nfd_path() {
    let sb = Sandbox::new("glob-nfc");
    let composed = "item-\u{e9}.txt";
    let decomposed = "item-e\u{301}.txt";
    fs::write(sb.work().join(decomposed), b"wanted").unwrap();
    fs::write(sb.work().join("other.txt"), b"other").unwrap();
    assert!(sb.run(&["put", decomposed, "other.txt"]).status.success());

    let out = sb.run(&["rm", "--home-only", "--glob", composed]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!sb.trash().join("files").join(decomposed).exists());
    assert!(sb.trash().join("files/other.txt").exists());
}

/// A parent traversal after a link resolves the link before `..`. Its
/// lexical collapse names the decoy, which must stay in the trash.
#[cfg(unix)]
#[test]
fn rm_resolves_a_link_before_climbing_to_the_original_path() {
    let sb = Sandbox::new("exact-link-up");
    fs::create_dir_all(sb.work().join("real/sub")).unwrap();
    std::os::unix::fs::symlink("real/sub", sb.work().join("link")).unwrap();
    fs::write(sb.work().join("other.txt"), b"decoy").unwrap();
    fs::write(sb.work().join("real/other.txt"), b"wanted").unwrap();
    assert!(sb.run(&["put", "other.txt"]).status.success());
    assert!(sb.run(&["put", "link/../other.txt"]).status.success());

    let out = sb.run(&["rm", "--home-only", "link/../other.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let left: Vec<_> = fs::read_dir(sb.trash().join("files"))
        .unwrap()
        .flatten()
        .map(|entry| fs::read(entry.path()).unwrap())
        .collect();
    assert_eq!(left, vec![b"decoy".to_vec()]);
}

/// A relative exact path still matches after its first component is gone.
#[test]
fn rm_matches_a_relative_path_whose_first_component_is_gone() {
    let sb = Sandbox::new("exact-gone-first");
    fs::create_dir(sb.work().join("gone")).unwrap();
    fs::write(sb.work().join("gone/foo.txt"), b"wanted").unwrap();
    assert!(sb.run(&["put", "gone/foo.txt"]).status.success());
    fs::remove_dir(sb.work().join("gone")).unwrap();

    let out = sb.run(&["rm", "--home-only", "gone/foo.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(trash_names(&sb).is_empty(), "{}", stderr_of(&out));
}

/// A relative path with `..` still matches when the component before it
/// no longer exists.
#[test]
fn rm_matches_dotdot_after_a_missing_first_component() {
    let sb = Sandbox::new("exact-missing-up");
    fs::create_dir(sb.work().join("a")).unwrap();
    fs::write(sb.work().join("b.txt"), b"wanted").unwrap();
    assert!(sb.run(&["put", "a/../b.txt"]).status.success());
    fs::remove_dir(sb.work().join("a")).unwrap();

    let out = sb.run(&["rm", "--home-only", "a/../b.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(trash_names(&sb).is_empty(), "{}", stderr_of(&out));
}

fn set_deletion_date(sb: &Sandbox, name: &str, date: &str) {
    let path = sb.trash().join(format!("info/{name}.trashinfo"));
    let text = fs::read_to_string(&path).unwrap();
    let mut out = String::new();
    for line in text.lines() {
        if line.starts_with("DeletionDate=") {
            out.push_str("DeletionDate=");
            out.push_str(date);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    fs::write(path, out).unwrap();
}

/// `a` is in the trash and too new for `--older-than`. `ab` is old enough.
/// The hint names `a` as outside the window and leaves `ab` alone.
#[test]
fn rm_names_an_exact_path_outside_the_age_window() {
    let sb = Sandbox::new("exact-age");
    fs::write(sb.work().join("a"), b"exact").unwrap();
    fs::write(sb.work().join("ab"), b"sibling-is-long-enough").unwrap();
    assert!(sb.run(&["put", "a", "ab"]).status.success());
    set_deletion_date(&sb, "a", "2099-01-01T00:00:00");
    set_deletion_date(&sb, "ab", "2000-01-01T00:00:00");

    let dry = sb.run(&["rm", "--home-only", "-n", "--older-than=1d", "a"]);
    assert_eq!(dry.status.code(), Some(1), "{}", stderr_of(&dry));
    let err = stderr_of(&dry);
    assert!(err.contains("outside the age window"), "{err}");
    assert!(!err.contains("no exact match"), "{err}");
    assert!(!err.contains("near miss"), "{err}");
    assert!(!stdout_of(&dry).contains("/ab"), "{}", stdout_of(&dry));

    let out = sb.run(&["rm", "--home-only", "--older-than=1d", "a"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    let mut names = trash_names(&sb);
    names.sort();
    assert_eq!(names, vec!["a".to_string(), "ab".to_string()]);
}

/// `empty PATH` with an age cut names a path that is too new, and the
/// older sibling stays.
#[test]
fn empty_names_an_exact_path_outside_the_age_window() {
    let sb = Sandbox::new("exact-empty-age");
    fs::write(sb.work().join("a"), b"exact").unwrap();
    fs::write(sb.work().join("ab"), b"sibling-is-long-enough").unwrap();
    assert!(sb.run(&["put", "a", "ab"]).status.success());
    set_deletion_date(&sb, "a", "2099-01-01T00:00:00");
    set_deletion_date(&sb, "ab", "2000-01-01T00:00:00");

    let out = sb.run(&["empty", "--home-only", "--plain", "--older-than=1d", "a"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(err.contains("outside the age window"), "{err}");
    assert!(err.contains("no trash items match"), "{err}");
    assert!(!err.contains("no exact match"), "{err}");
    assert!(!err.contains("near miss"), "{err}");
    let mut names = trash_names(&sb);
    names.sort();
    assert_eq!(names, vec!["a".to_string(), "ab".to_string()]);
}

#[test]
fn empty_i_of_a_path_asks_before_it_removes() {
    let sb = Sandbox::new("empty-i-path");
    fs::write(sb.work().join("chosen.txt"), b"chosen").unwrap();
    fs::write(sb.work().join("kept.txt"), b"kept").unwrap();
    assert!(sb.run(&["put", "chosen.txt", "kept.txt"]).status.success());

    let no = sb.run_with_stdin(&["empty", "--home-only", "-i", "chosen.txt"], "no\n");
    assert!(no.status.success(), "{}", stderr_of(&no));
    assert!(
        stderr_of(&no).contains("Permanently remove 1 item"),
        "{}",
        stderr_of(&no)
    );
    let mut names = trash_names(&sb);
    names.sort();
    assert_eq!(
        names,
        vec!["chosen.txt".to_string(), "kept.txt".to_string()]
    );

    let yes = sb.run_with_stdin(&["empty", "--home-only", "-i", "chosen.txt"], "y\n");
    assert!(yes.status.success(), "{}", stderr_of(&yes));
    assert_eq!(trash_names(&sb), vec!["kept.txt".to_string()]);
}

/// `a` and `ab` are far under `--larger-than=1G`, so the exact path is in
/// the trash and outside the size window. The hint says so and leaves
/// both items in place.
#[test]
fn rm_names_an_exact_path_outside_the_size_window() {
    let sb = Sandbox::new("exact-size");
    fs::write(sb.work().join("a"), b"x").unwrap();
    fs::write(sb.work().join("ab"), b"yy").unwrap();
    assert!(sb.run(&["put", "a", "ab"]).status.success());

    let out = sb.run(&["rm", "--home-only", "--larger-than=1G", "a"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(err.contains("outside the size window"), "{err}");
    assert!(!err.contains("no exact match"), "{err}");
    assert!(!err.contains("near miss"), "{err}");
    let mut names = trash_names(&sb);
    names.sort();
    assert_eq!(names, vec!["a".to_string(), "ab".to_string()]);
}

/// `empty PATH` removes that original path only, and names a near miss.
#[test]
fn empty_matches_the_original_path_exactly() {
    let sb = Sandbox::new("exact-empty");
    fs::create_dir(sb.work().join("a")).unwrap();
    fs::write(sb.work().join("a/b"), b"child").unwrap();
    assert!(sb.run(&["put", "a/b"]).status.success());
    fs::remove_dir(sb.work().join("a")).unwrap();
    fs::write(sb.work().join("a"), b"exact").unwrap();
    fs::write(sb.work().join("ab"), b"sib").unwrap();
    assert!(sb.run(&["put", "a", "ab"]).status.success());

    let out = sb.run(&["empty", "--home-only", "--plain", "--json", "-v", "a/"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let body = stdout_of(&out);
    let value: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("{e}: {body}"));
    assert_eq!(value["removed"], 1, "{body}");
    assert!(stderr_of(&out).contains("removed"), "{}", stderr_of(&out));
    let mut names = trash_names(&sb);
    names.sort();
    assert_eq!(names, vec!["ab".to_string(), "b".to_string()]);

    let sub = sb.work().join("elsewhere");
    fs::create_dir(&sub).unwrap();
    let from = common::rtrash(&sb.root)
        .args(["empty", "--home-only", "--plain", "../ab"])
        .current_dir(&sub)
        .output()
        .unwrap();
    assert!(from.status.success(), "{}", stderr_of(&from));
    assert_eq!(trash_names(&sb), vec!["b".to_string()]);

    let again = sb.run(&["empty", "--home-only", "--plain", "a"]);
    assert_eq!(again.status.code(), Some(1), "{}", stderr_of(&again));
    let err = stderr_of(&again);
    assert!(err.contains("no exact match"), "{err}");
    assert!(err.contains("near miss"), "{err}");
    assert!(err.contains("/a/b"), "{err}");
    assert!(err.contains("no trash items match"), "{err}");
    let dry = sb.run(&["empty", "--home-only", "--plain", "-n", "a"]);
    assert_eq!(dry.status.code(), Some(1), "{}", stderr_of(&dry));
    assert!(stderr_of(&dry).contains("near miss"));
    assert!(stderr_of(&dry).contains("no trash items match"));
    let json = sb.run(&["empty", "--home-only", "--plain", "-n", "--json", "a"]);
    assert_eq!(json.status.code(), Some(1), "{}", stderr_of(&json));
    let body = stdout_of(&json);
    let value: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("{e}: {body}"));
    assert_eq!(value["removed"], 0, "{body}");
    assert!(stderr_of(&json).contains("near miss"));
    assert!(stderr_of(&json).contains("no trash items match"));
    assert_eq!(trash_names(&sb), vec!["b".to_string()]);
}

#[test]
fn interactive_empty_of_an_exact_path_honors_the_reply() {
    let sb = Sandbox::new("exact-empty-i");
    fs::write(sb.work().join("chosen"), b"chosen").unwrap();
    fs::write(sb.work().join("kept"), b"kept").unwrap();
    assert!(sb.run(&["put", "chosen", "kept"]).status.success());

    let no = sb.run_with_stdin(&["empty", "--home-only", "-i", "chosen"], "n\n");
    assert!(no.status.success(), "{}", stderr_of(&no));
    assert!(
        stderr_of(&no).contains("Permanently remove 1 item"),
        "{}",
        stderr_of(&no)
    );
    assert_eq!(trash_names(&sb).len(), 2);

    let yes = sb.run_with_stdin(&["empty", "--home-only", "-i", "chosen"], "yes\n");
    assert!(yes.status.success(), "{}", stderr_of(&yes));
    assert_eq!(trash_names(&sb), vec!["kept".to_string()]);
}

#[test]
fn trash_rm_defaults_to_the_exact_original_path() {
    let sb = Sandbox::new("trash-rm-literal");
    sb.touch("note");
    sb.touch("notebook");
    assert!(sb.run(&["put", "note", "notebook"]).status.success());
    let out = sb.run(&["rm", "--home-only", "not"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert_eq!(trash_names(&sb).len(), 2);
    let out = sb.run(&["rm", "--home-only", "note"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let mut names = trash_names(&sb);
    names.sort();
    assert_eq!(names, vec!["notebook"]);
}

#[test]
fn trash_rm_refuses_duplicate_originals_without_all_matches() {
    let sb = Sandbox::new("rm-ambiguous");
    sb.touch("same.txt");
    assert!(sb.run(&["put", "same.txt"]).status.success());
    sb.touch("same.txt");
    assert!(sb.run(&["put", "same.txt"]).status.success());
    let original = sb.work().join("same.txt");
    let path = original.to_str().unwrap();
    let refused = sb.run(&["rm", path]);
    assert_eq!(refused.status.code(), Some(2), "{}", stderr_of(&refused));
    assert_eq!(trash_names(&sb).len(), 2);
    let removed = sb.run(&["rm", "--all-matches", path]);
    assert!(removed.status.success(), "{}", stderr_of(&removed));
    assert!(trash_names(&sb).is_empty());
}

#[test]
fn empty_exact_refuses_duplicate_originals_until_an_id_selects_one() {
    let sb = Sandbox::new("empty-exact-ambiguous");
    sb.touch("same.txt");
    assert!(sb.run(&["put", "same.txt"]).status.success());
    sb.touch("same.txt");
    assert!(sb.run(&["put", "same.txt"]).status.success());
    let original = sb.work().join("same.txt");
    let path = original.to_str().unwrap();
    let pin = format!("--trash-dir={}", sb.trash().display());

    let refused = sb.run(&["empty", "-f", &pin, path]);
    assert_eq!(refused.status.code(), Some(2), "{}", stderr_of(&refused));
    assert!(
        stderr_of(&refused).contains("refusing 2 trash items with original path"),
        "{}",
        stderr_of(&refused)
    );
    let mut names = trash_names(&sb);
    names.sort();
    assert_eq!(names.len(), 2, "{names:?}");

    let id = format!("--id={}", names[0]);
    let removed = sb.run(&["rm", &pin, &id, path]);
    assert!(removed.status.success(), "{}", stderr_of(&removed));
    assert_eq!(trash_names(&sb).len(), 1);
}

#[test]
fn keep_file_protects_an_exact_empty_path() {
    let sb = Sandbox::new("empty-exact-keep");
    fs::create_dir_all(sb.root.join(".config/rtrash")).unwrap();
    let kept = sb.work().join("kept.txt");
    let other = sb.work().join("other.txt");
    fs::write(
        sb.root.join(".config/rtrash/keep"),
        format!("{}\n", kept.display()),
    )
    .unwrap();
    fs::write(&kept, b"kept").unwrap();
    fs::write(&other, b"other").unwrap();
    assert!(sb.run(&["put", "kept.txt", "other.txt"]).status.success());
    let pin = format!("--trash-dir={}", sb.trash().display());

    let out = sb.run(&["empty", "-f", &pin, kept.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("listed in the keep file"),
        "{}",
        stderr_of(&out)
    );
    let mut names = trash_names(&sb);
    names.sort();
    assert_eq!(names, vec!["kept.txt", "other.txt"]);
}

#[test]
fn keep_file_protects_rm_empty_and_retention() {
    let sb = Sandbox::new("keep-permanent");
    fs::create_dir_all(sb.root.join(".config/rtrash")).unwrap();
    let kept = sb.work().join("kept");
    fs::write(
        sb.root.join(".config/rtrash/keep"),
        format!("# permanent deletion exclusions\n{}\n", kept.display()),
    )
    .unwrap();
    for name in ["kept/a", "drop"] {
        let path = sb.work().join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"x").unwrap();
    }
    assert!(sb.run(&["put", "-r", "kept", "drop"]).status.success());
    let rm = sb.run(&["rm", kept.to_str().unwrap()]);
    assert!(rm.status.success(), "{}", stderr_of(&rm));
    assert!(stderr_of(&rm).contains("listed in the keep file"));
    let pin = format!("--trash-dir={}", sb.trash().display());
    let dry = sb.run(&["empty", &pin, "--plain", "--dry-run"]);
    assert!(dry.status.success(), "{}", stderr_of(&dry));
    assert!(stdout_of(&dry).contains("drop"), "{}", stdout_of(&dry));
    assert!(!stdout_of(&dry).contains("kept/a"), "{}", stdout_of(&dry));
    let empty = sb.run(&["empty", &pin, "--plain", "--max-size=0"]);
    assert!(empty.status.success(), "{}", stderr_of(&empty));
    assert_eq!(trash_names(&sb), vec!["kept"]);
}

#[test]
fn keep_file_protects_permanent_reap() {
    let sb = Sandbox::new("keep-reap");
    fs::create_dir_all(sb.root.join(".config/rtrash")).unwrap();
    let scratch = sb.work().join("scratch");
    let kept = scratch.join("kept");
    fs::create_dir_all(&scratch).unwrap();
    fs::write(&kept, b"x").unwrap();
    age_tree(&kept);
    fs::write(
        sb.root.join(".config/rtrash/keep"),
        format!("{}\n", kept.display()),
    )
    .unwrap();
    let out = sb.run(&[
        "reap",
        "--permanent",
        "--older-than=1h",
        "--age-by=m",
        scratch.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(kept.exists());
    assert!(stdout_of(&out).contains("listed in the keep file"));
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

/// A cache line another tool wrote with only `%` encoded, for a name with
/// a space: status reads it, and a later put keeps it as one line in
/// rtrash's encoding instead of adding a second line for the same name.
#[test]
fn directorysizes_lines_from_other_encoders_are_read_and_kept() {
    use std::os::unix::fs::MetadataExt;
    let sb = Sandbox::new("dirsizes-foreign");
    for d in ["My Dir", "later"] {
        fs::create_dir(sb.work().join(d)).unwrap();
        fs::write(sb.work().join(d).join("inner"), vec![b'q'; 2048]).unwrap();
    }
    assert!(sb.run(&["put", "-r", "My Dir"]).status.success());
    let info = sb.trash().join("info/My Dir.trashinfo");
    let mtime = fs::metadata(&info).unwrap().mtime();
    let cache = sb.trash().join("directorysizes");
    let ours = fs::read_to_string(&cache).unwrap();
    let foreign = format!("777000000 {mtime} My Dir\n");
    assert_ne!(
        ours, foreign,
        "the forged line must differ from rtrash's own"
    );
    fs::write(&cache, &foreign).unwrap();
    let out = sb.run(&["status", "--home-only"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let s = stdout_of(&out);
    assert!(
        s.lines().any(|l| l == "Total: 1 item (741.0 MiB)"),
        "status must read the line: {s}"
    );
    assert_eq!(
        fs::read_to_string(&cache).unwrap(),
        foreign,
        "a hit writes nothing"
    );

    assert!(sb.run(&["put", "-r", "later"]).status.success());
    let body = fs::read_to_string(&cache).unwrap();
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 2, "one line per directory: {body}");
    assert_eq!(lines[0], format!("777000000 {mtime} My%20Dir"));
    assert!(lines[1].ends_with(" later"), "{body}");
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
    let dry = sb.run(&["empty", &pin, "--dry-run", "--json", "-v"]);
    assert!(dry.status.success(), "{}", stderr_of(&dry));
    let s = stdout_of(&dry);
    let json: serde_json::Value = serde_json::from_str(&s).unwrap_or_else(|e| panic!("{e}: {s}"));
    assert_eq!(json["dry_run"], true, "{s}");
    assert_eq!(json["removed"], 1, "{s}");
    assert!(json["bytes"].is_u64(), "{s}");
    // Still present after dry-run.
    assert!(sb.trash().join("files/e.txt").is_file());
    let real = sb.run(&["empty", &pin, "--json", "--older-than=0", "-v"]);
    assert!(real.status.success(), "{}", stderr_of(&real));
    let r = stdout_of(&real);
    let json: serde_json::Value = serde_json::from_str(&r).unwrap_or_else(|e| panic!("{e}: {r}"));
    assert_eq!(json["dry_run"], false, "{r}");
    assert_eq!(json["removed"], 1, "{r}");
    assert_eq!(json["days"], 0, "{r}");
    assert!(!sb.trash().join("files/e.txt").exists());
}

#[test]
fn empty_json_dry_run_with_age_is_one_document() {
    let sb = Sandbox::new("empty-json-age");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("old.txt");
    assert!(sb.run(&["put", "old.txt"]).status.success());
    let aged = sb.run(&["empty", &pin, "--older-than=0", "-n", "--json", "-v"]);
    assert!(aged.status.success(), "{}", stderr_of(&aged));
    let text = stdout_of(&aged);
    let json: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
    assert_eq!(json["dry_run"], true, "{text}");
    assert_eq!(json["removed"], 1, "{text}");
    assert_eq!(json["older_than_secs"], 0, "{text}");
    assert!(
        stderr_of(&aged).contains("would remove") && stderr_of(&aged).contains("old.txt"),
        "{}",
        stderr_of(&aged)
    );
    assert!(sb.trash().join("files/old.txt").is_file());
    let capped = sb.run(&["empty", &pin, "--max-size=0", "-n", "--json", "-v"]);
    assert!(capped.status.success(), "{}", stderr_of(&capped));
    let text = stdout_of(&capped);
    let json: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
    assert_eq!(json["dry_run"], true, "{text}");
    assert_eq!(json["removed"], 1, "{text}");
    assert_eq!(json["max_size"], 0, "{text}");
    assert!(
        stderr_of(&capped).contains("would remove") && stderr_of(&capped).contains("old.txt"),
        "{}",
        stderr_of(&capped)
    );
}

#[test]
fn rm_json_and_age_filters() {
    let sb = Sandbox::new("rm-json");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("keep.c");
    sb.touch("drop.o");
    assert!(sb.run(&["put", "keep.c"]).status.success());
    assert!(sb.run(&["put", "drop.o"]).status.success());
    let j = sb.run(&["rm", &pin, "--match=glob", "--json", "-n", "-v", "*.o"]);
    assert!(j.status.success(), "{}", stderr_of(&j));
    let s = stdout_of(&j);
    assert!(
        s.contains("\"removed\":1") || s.contains("\"removed\": 1"),
        "{s}"
    );
    assert!(s.contains("drop.o"), "{s}");
    assert!(!s.contains("keep.c"), "{s}");
    assert!(stderr_of(&j).contains("drop.o"), "{}", stderr_of(&j));
    // dry-run keeps files
    assert!(sb.trash().join("files/drop.o").is_file());
    let gone = sb.run(&["rm", &pin, "--match=glob", "--json", "*.o"]);
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
    let dry = sb.empty(&["--max-size=100K", "-n", "--json", "-v"]);
    assert!(dry.status.success(), "{}", stderr_of(&dry));
    let s = stdout_of(&dry);
    let json: serde_json::Value = serde_json::from_str(&s).unwrap_or_else(|e| panic!("{e}: {s}"));
    assert_eq!(json["dry_run"], true, "{s}");
    assert_eq!(json["removed"], 1, "{s}");
    assert_eq!(json["max_size"], 102_400, "{s}");
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

/// The parent is a non-UTF-8 symlink. `put` records the target, so `rm` of
/// the same bytes has to resolve that parent. A `Path` built from the
/// private-use form looks up a directory that is not there.
#[cfg(target_os = "linux")]
#[test]
fn an_exact_rm_resolves_a_parent_that_is_not_utf8() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let sb = Sandbox::new("nonutf8parent");
    let real = sb.work().join("real");
    fs::create_dir(&real).unwrap();
    fs::write(real.join("f"), "z").unwrap();
    let link = sb.work().join(OsStr::from_bytes(b"d\xff"));
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let mut path = link.into_os_string();
    path.push("/f");
    let put = run_os(&sb, &[OsStr::new("put"), path.as_os_str()]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let rm = run_os(
        &sb,
        &[
            OsStr::new("rm"),
            OsStr::new("--home-only"),
            OsStr::new("-n"),
            path.as_os_str(),
        ],
    );
    assert!(rm.status.success(), "{}", stderr_of(&rm));
    let out = stdout_of(&rm);
    assert_eq!(out.lines().count(), 1, "{out}{}", stderr_of(&rm));
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
    let Some(script) = script_bin() else {
        common::skip("skipping: no script(1) to give rtrash a terminal");
        return;
    };
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
    let tty = Command::new(&script)
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

/// Run `args` as root: directly when the tests already run as root, else
/// through `sudo -n`. False when that can't run or the command fails.
#[cfg(target_os = "linux")]
fn as_root(args: &[&str]) -> bool {
    let mut cmd = if unsafe { libc::geteuid() } == 0 {
        Command::new(args[0])
    } else {
        let mut c = Command::new("sudo");
        c.arg("-n").arg(args[0]);
        c
    };
    cmd.args(&args[1..])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Mounts made with [`as_root`], undone (lazily, the last made first) when
/// dropped, before the sandbox that holds them goes.
#[cfg(target_os = "linux")]
struct Binds(Vec<PathBuf>);

#[cfg(target_os = "linux")]
impl Drop for Binds {
    fn drop(&mut self) {
        for m in self.0.iter().rev() {
            as_root(&["umount", "-l", m.to_str().unwrap()]);
        }
    }
}

/// trash-cli #301 and #341, gtrash #20: one volume trash seen through two
/// bind mounts is one trash. list and `empty --dry-run` name its item once,
/// and restore puts the item back once, under the first mount.
#[cfg(target_os = "linux")]
#[test]
fn a_trash_seen_through_two_bind_mounts_counts_once() {
    use std::os::unix::fs::PermissionsExt;
    if !as_root(&["true"]) {
        common::skip("sudo unavailable; skipping");
        return;
    }
    let _global = global_state();
    let sb = Sandbox::new("bind-twice");
    let vol = sb.root.join("vol");
    let trash = vol.join(format!(".Trash-{}", unsafe { libc::getuid() }));
    fs::create_dir_all(trash.join("files")).unwrap();
    fs::create_dir_all(trash.join("info")).unwrap();
    fs::set_permissions(&trash, fs::Permissions::from_mode(0o700)).unwrap();
    let name = "rtrash-bind-twice-item".to_string();
    fs::write(trash.join("files").join(&name), b"x").unwrap();
    let record = format!("[Trash Info]\nPath={name}\nDeletionDate=2024-01-02T03:04:05\n");
    fs::write(trash.join("info").join(format!("{name}.trashinfo")), record).unwrap();
    let mut binds = Binds(Vec::new());
    for at in ["a", "b"] {
        let m = sb.root.join(at);
        fs::create_dir_all(&m).unwrap();
        let bound = as_root(&[
            "mount",
            "--bind",
            vol.to_str().unwrap(),
            m.to_str().unwrap(),
        ]);
        assert!(bound, "sudo -n mount --bind failed for {}", m.display());
        binds.0.push(m);
    }
    let rows = |out: &Output| stdout_of(out).matches(name.as_str()).count();
    let list = sb.run(&["list"]);
    assert_eq!(rows(&list), 1, "{}", stdout_of(&list));
    let dry = sb.run(&["empty", "--dry-run", "--verbose"]);
    assert_eq!(rows(&dry), 1, "{}", stdout_of(&dry));
    let restore = sb.run(&["restore", &format!("--name={name}")]);
    assert!(restore.status.success(), "{}", stderr_of(&restore));
    assert!(
        sb.root.join("a").join(&name).exists(),
        "{}",
        stderr_of(&restore)
    );
    assert!(
        !trash.join("files").join(&name).exists(),
        "{}",
        stderr_of(&restore)
    );
    drop(binds);
}

/// trashy #48 and #117, trash-cli #129: volumes mounted inside volumes,
/// as ZFS datasets and drives under /run/media are. put takes each file
/// to the trash at the top of its own mount, never the one around it, and
/// never fails with EXDEV; a home trash on a nested mount takes the files
/// of that mount. One list or empty reaches every volume trash, whatever
/// the current folder, --home-only and --trash-dir narrow it, and restore
/// puts a file back at the top of its volume.
#[cfg(target_os = "linux")]
#[test]
fn put_and_empty_follow_volumes_mounted_inside_volumes() {
    if !as_root(&["true"]) {
        common::skip("sudo unavailable; skipping");
        return;
    }
    let _global = global_state();
    let sb = Sandbox::new("nested-mounts");
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let vol = sb.root.join("vol");
    let inner = vol.join("inner");
    let local = vol.join(".local");
    let opts = format!("size=16m,mode=0755,uid={uid},gid={gid}");
    let mut mounts = Binds(Vec::new());
    // Each mount comes before the folder for the next, which is inside it.
    for m in [&vol, &inner, &local] {
        fs::create_dir_all(m).unwrap();
        let at = m.to_str().unwrap();
        let ok = as_root(&["mount", "-t", "tmpfs", "-o", &opts, "tmpfs", at]);
        assert!(ok, "sudo -n mount -t tmpfs failed for {at}");
        mounts.0.push(m.clone());
    }
    let xdg = local.join("share");
    fs::create_dir_all(&xdg).unwrap();
    let env = [("XDG_DATA_HOME", xdg.to_str().unwrap())];
    let names = [
        "rtrash-mnt-top.txt",
        "rtrash-mnt-nest.txt",
        "rtrash-mnt-near.txt",
    ];
    let top = vol.join(names[0]);
    let nest = inner.join("deep").join(names[1]);
    let near = local.join(names[2]);
    fs::create_dir_all(inner.join("deep")).unwrap();
    for f in [&top, &nest, &near] {
        fs::write(f, b"x").unwrap();
    }
    let put = sb.run_env(
        &[
            "put",
            top.to_str().unwrap(),
            nest.to_str().unwrap(),
            near.to_str().unwrap(),
        ],
        &env,
    );
    assert!(put.status.success(), "{}", stderr_of(&put));
    let vol_trash = vol.join(format!(".Trash-{uid}"));
    let inner_trash = inner.join(format!(".Trash-{uid}"));
    let home_trash = xdg.join("Trash");
    assert_eq!(
        names_in(&vol_trash.join("files")),
        vec![names[0].to_string()]
    );
    assert_eq!(
        names_in(&inner_trash.join("files")),
        vec![names[1].to_string()]
    );
    assert_eq!(
        names_in(&home_trash.join("files")),
        vec![names[2].to_string()]
    );
    assert!(!local.join(format!(".Trash-{uid}")).exists());
    let record = fs::read_to_string(
        inner_trash
            .join("info")
            .join(format!("{}.trashinfo", names[1])),
    )
    .unwrap();
    assert!(
        record.contains("\nPath=deep/rtrash-mnt-nest.txt\n"),
        "{record}"
    );

    let count = |out: &Output, name: &str| stdout_of(out).matches(name).count();
    let list = sb.run_env(&["list"], &env);
    let home_list = sb.run_env(&["list", "--home-only"], &env);
    let dry = sb.run_env(&["empty", "--dry-run", "--verbose"], &env);
    let home_dry = sb.run_env(&["empty", "--dry-run", "--verbose", "--home-only"], &env);
    for (i, name) in names.iter().enumerate() {
        let home = usize::from(i == 2);
        assert_eq!(count(&list, name), 1, "{name}: {}", stdout_of(&list));
        assert_eq!(count(&dry, name), 1, "{name}: {}", stdout_of(&dry));
        assert_eq!(
            count(&home_list, name),
            home,
            "{name}: {}",
            stdout_of(&home_list)
        );
        assert_eq!(
            count(&home_dry, name),
            home,
            "{name}: {}",
            stdout_of(&home_dry)
        );
    }

    // A pinned empty clears that volume trash alone.
    let pin = format!("--trash-dir={}", inner_trash.display());
    let out = sb.run_env(&["empty", &pin], &env);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(names_in(&inner_trash.join("files")).is_empty());
    assert_eq!(
        names_in(&vol_trash.join("files")),
        vec![names[0].to_string()]
    );
    assert_eq!(
        names_in(&home_trash.join("files")),
        vec![names[2].to_string()]
    );

    // trashy #117: the file goes back to the top of its own volume.
    let restore = sb.run_env(&["restore", &format!("--name={}", names[0])], &env);
    assert!(restore.status.success(), "{}", stderr_of(&restore));
    assert!(top.exists(), "{}", stderr_of(&restore));
    assert!(names_in(&vol_trash.join("files")).is_empty());
    drop(mounts);
}

/// trashy #115: links in the trash that point into a volume, which is
/// unmounted later. While it is mounted, put sends each link to the home
/// trash, as the link lives there, and empty counts and removes the link
/// alone, never what it points at. Once the volume is gone, every listing
/// and the dangling-record sweep finish and keep both items, since their
/// payloads are the links themselves; a full empty then removes them.
#[cfg(target_os = "linux")]
#[test]
fn trashed_links_into_an_unmounted_volume_go_as_links() {
    let _global = global_state();
    if !as_root(&["true"]) {
        common::skip("sudo unavailable; skipping");
        return;
    }
    let sb = Sandbox::new("link-unmounted");
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let vol = sb.root.join("vol");
    fs::create_dir_all(&vol).unwrap();
    let opts = format!("size=16m,mode=0755,uid={uid},gid={gid}");
    let at = vol.to_str().unwrap();
    let mut mounts = Binds(Vec::new());
    let ok = as_root(&["mount", "-t", "tmpfs", "-o", &opts, "tmpfs", at]);
    assert!(ok, "sudo -n mount -t tmpfs failed for {at}");
    mounts.0.push(vol.clone());
    fs::create_dir_all(vol.join("sub")).unwrap();
    let keep = vol.join("sub").join("keep.bin");
    fs::write(&keep, vec![7u8; 2_000_000]).unwrap();
    let names = ["rtrash-link-vol", "rtrash-link-sub", "rtrash-link-file"];
    let targets = [vol.clone(), vol.join("sub"), keep.clone()];
    for (name, to) in names.iter().zip(&targets) {
        std::os::unix::fs::symlink(to, sb.work().join(name)).unwrap();
    }
    let put = |name: &str| {
        let out = sb.run(&["put", name]);
        assert!(out.status.success(), "{name}: {}", stderr_of(&out));
    };

    // Mounted: the link to a folder goes, the folder and its file stay.
    put(names[1]);
    assert_eq!(names_in_trash(&sb), vec![names[1].to_string()]);
    let out = sb.empty(&[]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(trash_names(&sb).is_empty(), "{:?}", trash_names(&sb));
    assert_eq!(fs::read(&keep).unwrap().len(), 2_000_000);

    // A dry run counts the links, not the 2 MB behind them.
    put(names[0]);
    put(names[2]);
    assert!(!vol.join(format!(".Trash-{uid}")).exists());
    let dry = sb.empty(&["--dry-run", "--json"]);
    let text = stdout_of(&dry);
    assert!(dry.status.success(), "{}", stderr_of(&dry));
    assert!(text.contains(r#""removed":2,"#), "{text}");
    let from = text.find(r#""bytes":"#).unwrap() + r#""bytes":"#.len();
    let bytes: u64 = text[from..].split(',').next().unwrap().parse().unwrap();
    assert!(bytes < 1_000_000, "{text}");

    // Unmounted: both links now point at nothing there.
    assert!(
        as_root(&["umount", "-l", at]),
        "sudo -n umount -l failed for {at}"
    );
    mounts.0.clear();
    assert!(!keep.exists());
    let runs: [&[&str]; 3] = [
        &["list", "--home-only"],
        &["list", "--home-only", "--size"],
        &["status", "--home-only"],
    ];
    for args in runs {
        let out = sb.run(args);
        assert!(out.status.success(), "{args:?}: {}", stderr_of(&out));
    }
    let listed = stdout_of(&sb.run(&["list", "--home-only"]));
    for name in [names[0], names[2]] {
        assert!(listed.contains(name), "{name}: {listed}");
    }
    let left = vec![names[2].to_string(), names[0].to_string()];
    let out = sb.empty(&["--dangling"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(names_in_trash(&sb), left);
    let records: Vec<String> = left.iter().map(|n| format!("{n}.trashinfo")).collect();
    assert_eq!(names_in(&sb.trash().join("info")), records);
    let out = sb.empty(&[]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(trash_names(&sb).is_empty(), "{:?}", trash_names(&sb));
    assert!(vol.is_dir());
    drop(mounts);
}

/// The mount point `path` is on: the longest one above it in
/// `/proc/self/mounts`.
#[cfg(target_os = "linux")]
fn mount_top(path: &Path) -> PathBuf {
    let table = fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    table
        .lines()
        .filter_map(|l| l.split(' ').nth(1))
        .map(|m| PathBuf::from(m.replace("\\040", " ")))
        .filter(|m| path.starts_with(m))
        .max_by_key(|m| m.components().count())
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// glib #883: with `$XDG_DATA_HOME` on another file system than a file,
/// and no trash of ours on the file's volume, a put copies the file to
/// the home trash, where list and restore find it. The test runs only
/// where that volume's top directory holds no trash of ours and takes
/// none (an ordinary user under a root-owned `/`), so it never makes or
/// uses a trash outside the sandbox.
#[cfg(target_os = "linux")]
#[test]
fn a_home_trash_on_another_device_takes_files_its_volume_cannot() {
    use std::os::unix::ffi::OsStrExt;
    let _global = global_state();
    let sb = Sandbox::new("xdg-other-device");
    let Some(xdg) = other_device_dir(&sb.root, "xdg") else {
        common::skip("no second file system here; skipping");
        return;
    };
    let top = mount_top(&sb.root);
    let uid = unsafe { libc::getuid() };
    let c_top = std::ffi::CString::new(top.as_os_str().as_bytes()).unwrap();
    // SAFETY: a valid C string.
    let writable = unsafe { libc::access(c_top.as_ptr(), libc::W_OK) } == 0;
    let present = |name: String| fs::symlink_metadata(top.join(name)).is_ok();
    if writable || present(".Trash".into()) || present(format!(".Trash-{uid}")) {
        common::skip(&format!(
            "{} may hold a trash of ours; skipping",
            top.display()
        ));
        let _ = fs::remove_dir_all(&xdg);
        return;
    }
    let f = sb.work().join("far.txt");
    fs::write(&f, b"far away").unwrap();
    let env = [("XDG_DATA_HOME", xdg.to_str().unwrap())];
    let out = sb.run_env(&["put", "--allow-cross-device", "far.txt"], &env);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!f.exists());
    let trash = xdg.join("Trash");
    assert_eq!(names_in(&trash.join("files")), vec!["far.txt".to_string()]);
    let info = fs::read_to_string(trash.join("info/far.txt.trashinfo")).unwrap();
    assert!(info.contains(&format!("Path={}\n", f.display())), "{info}");
    let listed = stdout_of(&sb.run_env(&["list"], &env));
    assert!(listed.contains(&f.display().to_string()), "{listed}");
    let back = sb.run_env(&["restore", "far.txt"], &env);
    assert!(back.status.success(), "{}", stderr_of(&back));
    assert_eq!(fs::read(&f).unwrap(), b"far away");
    assert_eq!(names_in(&trash.join("files")), Vec::<String>::new());
    let _ = fs::remove_dir_all(&xdg);
}

/// glib #1640: a home trash that is a symlink to a folder elsewhere. Put,
/// list, restore and empty all work through the link, which stays a link,
/// and trashing the link or the folder holding it is refused.
#[cfg(unix)]
#[test]
fn a_symlinked_home_trash_works_through_the_link() {
    let sb = Sandbox::new("trash-link");
    let real = sb.root.join("disk/Trash");
    fs::create_dir_all(&real).unwrap();
    fs::create_dir_all(sb.root.join("xdg")).unwrap();
    std::os::unix::fs::symlink(&real, sb.trash()).unwrap();
    let a = sb.touch("a.txt");
    let out = sb.run(&["put", "a.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(names_in(&real.join("files")), vec!["a.txt".to_string()]);
    let listed = stdout_of(&sb.run(&["list"]));
    assert!(listed.contains(&a.display().to_string()), "{listed}");
    let back = sb.run(&["restore", "a.txt"]);
    assert!(back.status.success(), "{}", stderr_of(&back));
    assert!(a.exists());
    assert!(sb.run(&["put", "a.txt"]).status.success());
    let emptied = sb.run(&["empty", "--home-only", "--all"]);
    assert!(emptied.status.success(), "{}", stderr_of(&emptied));
    assert_eq!(names_in(&real.join("files")), Vec::<String>::new());
    for target in [sb.trash(), sb.root.join("xdg")] {
        let name = target.display().to_string();
        let out = sb.run(&["put", "-r", &name]);
        assert_eq!(out.status.code(), Some(1), "{name}: {}", stderr_of(&out));
        let err = stderr_of(&out);
        assert!(
            err.contains("it contains the trash directory"),
            "{name}: {err}"
        );
        let link = fs::symlink_metadata(sb.trash()).unwrap();
        assert!(link.file_type().is_symlink(), "{name}");
    }
}

/// glib #1640: a home trash symlinked to another file system keeps its
/// items there, so a file from that file system goes to the home trash
/// by a rename, not to a new trash at the top of that file system.
#[cfg(target_os = "linux")]
#[test]
fn a_home_trash_linked_to_another_device_takes_files_from_there() {
    let _global = global_state();
    let sb = Sandbox::new("trash-link-device");
    let Some(real) = other_device_dir(&sb.root, "linked-trash") else {
        common::skip("no second file system here; skipping");
        return;
    };
    let Some(near) = other_device_dir(&sb.root, "linked-files") else {
        common::skip("no second file system here; skipping");
        let _ = fs::remove_dir_all(&real);
        return;
    };
    fs::create_dir_all(sb.root.join("xdg")).unwrap();
    std::os::unix::fs::symlink(&real, sb.trash()).unwrap();
    let f = near.join("near.txt");
    fs::write(&f, b"near").unwrap();
    let uid = unsafe { libc::getuid() };
    let volume_trash = mount_top(&f).join(format!(".Trash-{uid}"));
    let had_volume_trash = fs::symlink_metadata(&volume_trash).is_ok();
    let name = f.display().to_string();
    let out = sb.run(&["put", &name]);
    let made = !had_volume_trash && fs::symlink_metadata(&volume_trash).is_ok();
    if made {
        // Only what this run made goes; a trash that was there stays.
        let _ = fs::remove_dir_all(&volume_trash);
    }
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!made, "put made {}", volume_trash.display());
    assert_eq!(names_in(&real.join("files")), vec!["near.txt".to_string()]);
    let _ = fs::remove_dir_all(&real);
    let _ = fs::remove_dir_all(&near);
}

/// glib #605: a mount that never answers, as a hard NFS mount of a dead
/// server does, is skipped with a warning once `RTRASH_MOUNT_WAIT` runs
/// out, so list, status, empty and du --devices still finish. The
/// debug-only seam `RTRASH_TEST_STALL_MOUNT` makes every look at `/` hang.
#[cfg(all(target_os = "linux", debug_assertions))]
#[test]
fn a_mount_that_never_answers_is_skipped_in_time() {
    let sb = Sandbox::new("stalled-mount");
    let a = sb.touch("a.txt");
    assert!(sb.run(&["put", "a.txt"]).status.success());
    let env = [
        ("RTRASH_TEST_STALL_MOUNT", "/"),
        ("RTRASH_MOUNT_WAIT", "0.2"),
    ];
    let runs: [&[&str]; 4] = [
        &["list"],
        &["status"],
        &["empty", "--dry-run", "--all"],
        &["du", "--devices"],
    ];
    for args in runs {
        let start = std::time::Instant::now();
        let out = sb.run_env(args, &env);
        let took = start.elapsed();
        let err = stderr_of(&out);
        assert!(out.status.success(), "{args:?}: {err}");
        assert!(err.contains("skipping"), "{args:?}: {err}");
        assert!(err.contains("'/'"), "{args:?}: {err}");
        assert!(
            took < std::time::Duration::from_secs(30),
            "{args:?}: {took:?}"
        );
    }
    let listed = stdout_of(&sb.run_env(&["list"], &env));
    assert!(listed.contains(&a.display().to_string()), "{listed}");
}

/// trash-cli #427: a put to another file system copies, then removes the
/// original, so a tree it cannot wholly remove ends up half gone. rtrash
/// refuses before it copies: a read-only directory deep inside, a parent
/// that does not allow deleting (for a directory and for a file), a
/// read-only directory itself, and another user's file in another user's
/// sticky directory. Each stays
/// whole and the trash stays empty. Root may remove all of these, so the
/// test needs an ordinary user; the sticky case also needs `sudo -n`.
#[cfg(target_os = "linux")]
#[test]
fn put_across_devices_refuses_what_it_could_only_half_remove() {
    use std::os::unix::fs::PermissionsExt;
    let sb = Sandbox::new("xdev-unremovable");
    let Some(other) = other_device_dir(&sb.root, "unremovable") else {
        common::skip("no second file system here; skipping");
        return;
    };
    if unsafe { libc::geteuid() } == 0 {
        common::skip("root may remove every case here; skipping");
        let _ = fs::remove_dir_all(&other);
        return;
    }
    let w = sb.work();
    let chmod = |p: &Path, m: u32| fs::set_permissions(p, fs::Permissions::from_mode(m)).unwrap();
    fs::create_dir_all(w.join("deep/a/ro")).unwrap();
    fs::write(w.join("deep/a/ro/f"), b"x").unwrap();
    chmod(&w.join("deep/a/ro"), 0o555);
    fs::create_dir_all(w.join("p/d")).unwrap();
    fs::write(w.join("p/d/f"), b"x").unwrap();
    fs::write(w.join("p/x"), b"x").unwrap();
    chmod(&w.join("p"), 0o555);
    fs::create_dir_all(w.join("top/sub")).unwrap();
    fs::write(w.join("top/sub/g"), b"x").unwrap();
    chmod(&w.join("top"), 0o555);
    let mut cases = vec![
        ("deep", "deep/a/ro/f"),
        ("p/d", "p/d/f"),
        ("p/x", "p/x"),
        ("top", "top/sub/g"),
    ];
    fs::create_dir_all(w.join("st/s")).unwrap();
    fs::write(w.join("st/s/g"), b"x").unwrap();
    chmod(&w.join("st/s"), 0o1777);
    let (s, g) = (w.join("st/s"), w.join("st/s/g"));
    let sticky = as_root(&["chown", "65534", s.to_str().unwrap(), g.to_str().unwrap()]);
    if sticky {
        cases.push(("st", "st/s/g"));
    } else {
        common::skip("sudo -n chown failed; sticky case skipped");
    }
    let pin = format!("--trash-dir={}", other.display());
    for (op, keep) in cases {
        let out = sb.run(&["put", "-r", "--allow-cross-device", &pin, op]);
        assert_eq!(out.status.code(), Some(1), "{op}: {}", stderr_of(&out));
        assert!(w.join(keep).exists(), "{op}: {keep} is gone");
        assert_eq!(names_in(&other.join("files")), Vec::<String>::new(), "{op}");
        assert_eq!(names_in(&other.join("info")), Vec::<String>::new(), "{op}");
    }
    chmod(&w.join("deep/a/ro"), 0o755);
    chmod(&w.join("p"), 0o755);
    chmod(&w.join("top"), 0o755);
    if sticky {
        as_root(&["rm", "-rf", s.to_str().unwrap()]);
    }
    let _ = fs::remove_dir_all(&other);
}

/// An immutable or append-only file (chattr +i or +a) cannot be unlinked,
/// by root either, so a put across devices refuses its tree before it
/// copies (trash-cli #427). Each case needs chattr as root (directly or
/// through `sudo -n`) and a file system that keeps the flag; it is
/// skipped otherwise.
#[cfg(target_os = "linux")]
#[test]
fn put_across_devices_refuses_a_tree_with_an_immutable_file() {
    let sb = Sandbox::new("xdev-immutable");
    let Some(other) = other_device_dir(&sb.root, "immutable") else {
        common::skip("no second file system here; skipping");
        return;
    };
    let w = sb.work();
    let pin = format!("--trash-dir={}", other.display());
    for (set, unset, dir) in [("+i", "-i", "imm"), ("+a", "-a", "app")] {
        fs::create_dir_all(w.join(dir)).unwrap();
        let f = w.join(dir).join("f");
        fs::write(&f, b"x").unwrap();
        let path = f.to_str().unwrap();
        if !as_root(&["chattr", set, path]) {
            common::skip(&format!("chattr {set} failed here; {dir} case skipped"));
            continue;
        }
        let out = sb.run(&["put", "-r", "--allow-cross-device", &pin, dir]);
        as_root(&["chattr", unset, path]);
        assert_eq!(out.status.code(), Some(1), "{dir}: {}", stderr_of(&out));
        let err = stderr_of(&out);
        assert!(err.contains("immutable or append-only"), "{dir}: {err}");
        assert!(f.exists(), "{dir}: f is gone");
        assert_eq!(
            names_in(&other.join("files")),
            Vec::<String>::new(),
            "{dir}"
        );
    }
    let _ = fs::remove_dir_all(&other);
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
        common::skip("no POSIX ACLs on this file system; skipping");
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
    let out = run(&[
        "put",
        "-r",
        "--allow-cross-device",
        "--no-space-check",
        &pin,
        "f.txt",
        "d",
    ]);
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
        common::skip("RTRASH_TEST_OTHER_VOLUME unset; skipping");
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
    let out = run(&[
        "put",
        "-r",
        "--allow-cross-device",
        "--no-space-check",
        &pin,
        "f.txt",
        "d",
    ]);
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
        .args(["put", "--allow-cross-device", &pin, "big"])
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
        .args([
            "put",
            "--allow-cross-device",
            "--no-space-check",
            &pin,
            "big",
        ])
        .current_dir(sb.work())
        .env("RTRASH_MIN_FREE", "1000000T")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!sb.work().join("big").exists());
    assert!(other.join("files/big").exists());
    let _ = fs::remove_dir_all(&other);
}

/// A put onto a trash on another device is refused before it copies.
/// The error names both devices and the bytes. `--allow-cross-device`
/// copies, and the trash then holds the same bytes. `--json` keeps that
/// refusal in the same summary other put failures use. A same-device put
/// still needs no flag.
#[test]
fn put_refuses_a_cross_device_copy_until_asked_and_then_checks_it() {
    use std::os::unix::fs::MetadataExt;
    let sb = Sandbox::new("xdev-guard");
    fs::write(sb.work().join("same.txt"), b"same").unwrap();
    let same = sb.run(&["put", "same.txt"]);
    assert!(same.status.success(), "{}", stderr_of(&same));
    assert!(!sb.work().join("same.txt").exists());

    let Some(other) = other_device_dir(&sb.root, "guard") else {
        common::skip("no second file system here; skipping");
        return;
    };
    let file = sb.work().join("note.txt");
    fs::write(&file, b"hello-cross").unwrap();
    fs::create_dir(sb.work().join("tree")).unwrap();
    fs::write(sb.work().join("tree/inner.txt"), b"inside").unwrap();
    let pin = format!("--trash-dir={}", other.display());
    let meta = fs::symlink_metadata(&file).unwrap();
    let src_dev = meta.dev();
    let dst_dev = fs::symlink_metadata(other.join("files")).unwrap().dev();
    assert_ne!(src_dev, dst_dev);
    let need = rtrash::space::tree_need(&file, &meta).unwrap().0;

    let out = sb.run(&["put", &pin, "note.txt"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(err.contains("--allow-cross-device"), "{err}");
    assert!(err.contains(&format!("{need} bytes")), "{err}");
    assert!(err.contains(&format!("device {src_dev}")), "{err}");
    assert!(err.contains(&format!("device {dst_dev}")), "{err}");
    assert_eq!(fs::read(&file).unwrap(), b"hello-cross");
    assert_eq!(names_in(&other.join("files")), Vec::<String>::new());
    assert_eq!(names_in(&other.join("info")), Vec::<String>::new());

    let out = sb.run(&["put", "--json", &pin, "note.txt"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert_eq!(stdout_of(&out).trim(), r#"{"trashed":[],"failed":1}"#);
    assert!(
        stderr_of(&out).contains("--allow-cross-device"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(fs::read(&file).unwrap(), b"hello-cross");

    let out = sb.run(&[
        "put",
        "--allow-cross-device",
        "-r",
        &pin,
        "note.txt",
        "tree",
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!file.exists());
    assert!(!sb.work().join("tree").exists());
    assert_eq!(
        fs::read(other.join("files/note.txt")).unwrap(),
        b"hello-cross"
    );
    assert_eq!(
        fs::read(other.join("files/tree/inner.txt")).unwrap(),
        b"inside"
    );
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
            let text = String::from_utf8_lossy(&out.stdout);
            if text.contains(&format!("\"pid\":{}", child.id()))
                && text.contains("\"command\":\"sleep\"")
            {
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
    let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout).unwrap();
    let user = user.trim();
    let out = sb.run(&["who", "held.log"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    let line = text
        .lines()
        .find(|l| l.contains(&format!("pid {}", child.pid())) && l.contains("fd 3"))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(line.contains("held.log"), "{line}");
    assert!(line.contains(&format!("user {user}")), "{line}");
    assert!(line.contains("command sleep"), "{line}");
    let out = sb.run(&["who", "--json", "."]);
    assert!(out.status.success());
    let json = stdout_of(&out);
    assert_eq!(json.lines().count(), 1, "{json}");
    assert!(json.starts_with('{'), "{json}");
    assert!(
        json.contains(&format!("\"pid\":{},", child.pid())),
        "{json}"
    );
    assert!(json.contains(&format!("\"user\":\"{user}\"")), "{json}");
    assert!(json.contains("\"cmdline\":"), "{json}");
    assert!(!json.contains("no process holds"), "{json}");
    let out = sb.run(&["who", "idle.log"]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout_of(&out));
    let idle = stdout_of(&out);
    assert!(
        idle.contains("idle.log: no process holds this path"),
        "{idle}"
    );
    let counted = stdout_of(&sb.run(&["who", "--json", "idle.log"]));
    assert_eq!(counted.lines().count(), 1, "{counted}");
    let unread = counted
        .trim()
        .rsplit_once("\"unreadable\":")
        .and_then(|(_, n)| n.trim_end_matches('}').parse::<u64>().ok())
        .unwrap_or(0);
    if unread > 0 {
        assert!(
            idle.contains("the result is partial") && idle.contains("could not be inspected"),
            "{idle}"
        );
        let err = stderr_of(&out);
        assert!(
            err.contains("could not be inspected") && err.contains("run as root"),
            "unreadable {unread}: {err}"
        );
    } else {
        assert!(!idle.contains("partial"), "{idle}");
    }
}

/// The same operand twice is one path. A holder and a free path each
/// print once.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn who_prints_a_duplicate_held_path_and_a_duplicate_free_path_once() {
    let sb = Sandbox::new("who-dup");
    let held = sb.touch("held.log");
    sb.touch("idle.log");
    let child = Holder::open(&held);
    let out = sb.run(&["who", "held.log", "held.log"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    let hits = text
        .lines()
        .filter(|l| l.contains(&format!("pid {}", child.pid())) && l.contains("held.log"))
        .count();
    assert_eq!(hits, 1, "{text}");
    let out = sb.run(&["who", "idle.log", "idle.log"]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout_of(&out));
    let idle = stdout_of(&out);
    let free = idle
        .lines()
        .filter(|l| l.contains("idle.log: no process holds this path"))
        .count();
    assert_eq!(free, 1, "{idle}");
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
    // Without root some processes cannot be inspected, and refuse then
    // leaves even a file nobody holds, unless every process was seen. Each
    // pair is a `who` before the put and a `who` after it. Other tests
    // start and end processes beside this one, so a pair is tried again
    // only when the count of processes that could not be inspected is seen
    // to change, in the two `who` runs or in the put's own message. A pair
    // whose count held must agree, and any mismatch there fails the test.
    let mut changed = Vec::new();
    for _ in 0..IN_USE_PAIRS {
        sb.touch("idle.log");
        let before = who_unreadable(&stdout_of(&sb.run(&["who", "--json", "idle.log"])));
        let out = sb.run(&["put", &pin, "--in-use=refuse", "idle.log"]);
        let after = who_unreadable(&stdout_of(&sb.run(&["who", "--json", "idle.log"])));
        let said = stderr_of(&out);
        match judge_in_use_pair(before, after, out.status.code(), &said) {
            InUsePair::Agreed => return,
            InUsePair::Changed => changed.push(format!(
                "before {before}, after {after}, put {:?}: {said}",
                out.status.code()
            )),
            InUsePair::Mismatch(why) => panic!(
                "the unreadable count held at {before} and put disagreed: {why}\n\
                 put exit {:?}, stderr: {said}",
                out.status.code()
            ),
        }
    }
    panic!(
        "no pair of {IN_USE_PAIRS} had a steady unreadable count; the counts were\n{}",
        changed.join("\n")
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
const IN_USE_PAIRS: usize = 50;

/// What one `who`, put, `who` triple showed.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, PartialEq)]
enum InUsePair {
    /// The unreadable count held and put did what that count asks.
    Agreed,
    /// The unreadable count moved, so the pair says nothing.
    Changed,
    /// The unreadable count held and put did something else.
    Mismatch(String),
}

/// The `unreadable` field of a `who --json` run.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn who_unreadable(json: &str) -> usize {
    let tail = json
        .split("\"unreadable\":")
        .nth(1)
        .unwrap_or_else(|| panic!("no unreadable count in {json}"));
    let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
    digits
        .parse()
        .unwrap_or_else(|_| panic!("no unreadable count in {json}"))
}

/// The count put names when it says "N processes could not be inspected".
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn put_unreadable(said: &str) -> Option<usize> {
    let (head, _) = said.split_once("could not be inspected")?;
    let mut words = head.split_whitespace().rev();
    words.next()?;
    words.next()?.parse().ok()
}

/// Judges one pair. `before` and `after` are the counts of the `who` runs
/// around a `put --in-use=refuse` of a file nobody holds, whose exit code
/// and stderr are `code` and `said`. With no unreadable process the put
/// must succeed without a word, and with some it must refuse for them. A
/// put that names another count than the `who` runs saw a count that moved.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn judge_in_use_pair(before: usize, after: usize, code: Option<i32>, said: &str) -> InUsePair {
    if before != after {
        return InUsePair::Changed;
    }
    if let Some(named) = put_unreadable(said) {
        if named != before {
            return InUsePair::Changed;
        }
    }
    let agreed = if before == 0 {
        code == Some(0) && said.is_empty()
    } else {
        code == Some(1) && put_unreadable(said) == Some(before)
    };
    if agreed {
        InUsePair::Agreed
    } else {
        InUsePair::Mismatch(format!(
            "with {before} unreadable processes put exited {code:?}"
        ))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn an_in_use_pair_is_judged_only_when_the_unreadable_count_held() {
    use InUsePair::{Agreed, Changed, Mismatch};
    let refused = concat!(
        "rtrash: cannot trash 'idle.log': it may be in use; ",
        "4 processes could not be inspected\n",
        "rtrash: use -f or --in-use=warn to trash it anyway\n"
    );
    let one =
        "rtrash: cannot trash 'idle.log': it may be in use; 1 process could not be inspected\n";
    assert_eq!(judge_in_use_pair(4, 4, Some(1), refused), Agreed);
    assert_eq!(judge_in_use_pair(0, 0, Some(0), ""), Agreed);
    assert_eq!(judge_in_use_pair(1, 1, Some(1), one), Agreed);
    // The count moved between the runs, or the put saw another one.
    assert_eq!(judge_in_use_pair(0, 1, Some(0), ""), Changed);
    assert_eq!(judge_in_use_pair(4, 0, Some(1), refused), Changed);
    assert_eq!(judge_in_use_pair(0, 0, Some(1), one), Changed);
    assert_eq!(judge_in_use_pair(4, 4, Some(1), one), Changed);
    // The count held and put did the other thing.
    assert!(matches!(judge_in_use_pair(4, 4, Some(0), ""), Mismatch(_)));
    assert!(matches!(
        judge_in_use_pair(0, 0, Some(1), "rtrash: no\n"),
        Mismatch(_)
    ));
    assert!(matches!(
        judge_in_use_pair(0, 0, Some(0), "warning\n"),
        Mismatch(_)
    ));
    assert!(matches!(
        judge_in_use_pair(4, 4, Some(1), "rtrash: no\n"),
        Mismatch(_)
    ));
    // A message without "could not be inspected" names no count, however
    // its last words read.
    let other = "rtrash: cannot trash 'idle.log': 4 errors\n";
    assert_eq!(put_unreadable(other), None);
    assert!(matches!(
        judge_in_use_pair(4, 4, Some(1), other),
        Mismatch(_)
    ));
    assert!(matches!(
        judge_in_use_pair(3, 3, Some(1), other),
        Mismatch(_)
    ));
    assert_eq!(who_unreadable("{\"holders\":[],\"unreadable\":4}"), 4);
    assert_eq!(who_unreadable("{\"unreadable\":0}"), 0);
}

/// A stand-in `/proc` for the debug seam `RTRASH_TEST_PROC`: each of
/// `holders` is a process with `path` open as fd 3, each of `denied` one
/// this user may not inspect (the `denied` file marks it, since a test run
/// as root cannot get that from file modes).
#[cfg(all(target_os = "linux", debug_assertions))]
fn fake_proc(sb: &Sandbox, name: &str, holders: &[(u32, &Path)], denied: &[u32]) -> PathBuf {
    let dir = sb.root.join(name);
    fs::create_dir_all(&dir).unwrap();
    for (pid, path) in holders {
        let fd = dir.join(pid.to_string()).join("fd");
        fs::create_dir_all(&fd).unwrap();
        std::os::unix::fs::symlink(fs::canonicalize(path).unwrap(), fd.join("3")).unwrap();
        fs::write(dir.join(pid.to_string()).join("comm"), "fakeholder\n").unwrap();
    }
    for pid in denied {
        let one = dir.join(pid.to_string());
        fs::create_dir_all(&one).unwrap();
        fs::write(one.join("denied"), "").unwrap();
    }
    dir
}

/// Debug builds on Linux only: as a normal user `who` lists what it can
/// see and says on stderr how many processes it could not inspect, so an
/// empty list is not taken for a free file. A stand-in /proc gives one
/// holder and two processes this user may not read.
#[cfg(all(target_os = "linux", debug_assertions))]
#[test]
fn who_says_how_many_processes_it_could_not_inspect() {
    let sb = Sandbox::new("who-noroot");
    let held = sb.touch("w.log");
    let proc = fake_proc(
        &sb,
        "proc-some",
        &[(4_000_001, &held)],
        &[4_000_002, 4_000_003],
    );
    let env = [("RTRASH_TEST_PROC", proc.to_str().unwrap())];
    let out = sb.run_env(&["who", "w.log"], &env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(
        text.contains("4000001") && text.contains("fakeholder"),
        "{text}"
    );
    let err = stderr_of(&out);
    assert!(err.contains("2 processes could not be inspected"), "{err}");
    assert!(err.contains("the list may be incomplete"), "{err}");
    // SAFETY: geteuid has no preconditions. This is the check `as_root`
    // makes: who prints the hint for a normal user and leaves it off for root.
    if unsafe { libc::geteuid() } == 0 {
        assert!(!err.contains("run as root"), "{err}");
    } else {
        assert!(err.contains("run as root"), "{err}");
    }
    let out = sb.run_env(&["who", "--json", "w.log"], &env);
    assert!(
        stdout_of(&out).contains("\"unreadable\":2}"),
        "{}",
        stdout_of(&out)
    );
    assert!(
        stderr_of(&out).contains("2 processes could not be inspected"),
        "{}",
        stderr_of(&out)
    );
    let one = fake_proc(&sb, "proc-one", &[], &[4_000_002]);
    let env = [("RTRASH_TEST_PROC", one.to_str().unwrap())];
    let out = sb.run_env(&["who", "w.log"], &env);
    assert_eq!(out.status.code(), Some(1), "{}", stdout_of(&out));
    let text = stdout_of(&out);
    assert!(text.contains("w.log: no process holds this path"), "{text}");
    assert!(
        text.contains("the result is partial") && text.contains("could not be inspected"),
        "{text}"
    );
    let err = stderr_of(&out);
    assert!(
        err.contains("no holder seen, but 1 process could not be inspected"),
        "{err}"
    );
    assert!(err.contains("a path may still be in use"), "{err}");
    let none = fake_proc(&sb, "proc-none", &[], &[]);
    let env = [("RTRASH_TEST_PROC", none.to_str().unwrap())];
    let out = sb.run_env(&["who", "w.log"], &env);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stderr_of(&out), "");
    let text = stdout_of(&out);
    assert!(text.contains("w.log: no process holds this path"), "{text}");
    assert!(!text.contains("partial"), "{text}");
}

/// Debug builds on Linux only: `--in-use=refuse` never takes a process it
/// could not inspect for no process at all. Put (and `rm`, which runs
/// put) leaves the item, naming why, unless -f accepts the risk; warn
/// stays quiet, as it was; a process list that cannot be read at all
/// refuses too.
#[cfg(all(target_os = "linux", debug_assertions))]
#[test]
fn put_refuse_does_not_take_an_uninspected_process_for_a_free_file() {
    let sb = Sandbox::new("put-refuse-noroot");
    let f = sb.touch("r.log");
    let pin = format!("--trash-dir={}", sb.trash().display());
    let blind = fake_proc(&sb, "proc-blind", &[], &[4_000_002]);
    let env = [("RTRASH_TEST_PROC", blind.to_str().unwrap())];
    let out = sb.run_env(&["put", &pin, "--in-use=refuse", "r.log"], &env);
    let err = stderr_of(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("cannot trash 'r.log': it may be in use; 1 process could not be inspected"),
        "{err}"
    );
    assert!(err.contains("use -f or --in-use=warn"), "{err}");
    assert!(f.exists());
    let out = sb.run_env(
        &["put", &pin, "--dry-run", "--in-use=refuse", "r.log"],
        &env,
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    let envs = [env[0], ("RTRASH_IN_USE", "refuse")];
    let out = sb.run_env(&["put", &pin, "r.log"], &envs);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(f.exists());
    let bindir = sb.root.join("bin");
    fs::create_dir_all(&bindir).unwrap();
    std::os::unix::fs::symlink(bin(), bindir.join("rm")).unwrap();
    let out = Command::new(bindir.join("rm"))
        .args([&pin, "r.log"])
        .current_dir(sb.work())
        .env("HOME", &sb.root)
        .env("XDG_DATA_HOME", sb.root.join("xdg"))
        .env("RTRASH_TEST_PROC", &blind)
        .env("RTRASH_IN_USE", "refuse")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(f.exists());
    let out = sb.run_env(&["put", &pin, "--in-use=warn", "r.log"], &env);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(stderr_of(&out), "");
    assert!(!f.exists());
    let f = sb.touch("r.log");
    let out = sb.run_env(&["put", &pin, "-f", "--in-use=refuse", "r.log"], &env);
    let err = stderr_of(&out);
    assert!(out.status.success(), "{err}");
    assert!(
        err.contains("warning: 'r.log' may be in use; 1 process could not be inspected"),
        "{err}"
    );
    assert!(!f.exists());
    let f = sb.touch("r.log");
    let seen = fake_proc(&sb, "proc-seen", &[], &[]);
    let env_seen = [("RTRASH_TEST_PROC", seen.to_str().unwrap())];
    let out = sb.run_env(&["put", &pin, "--in-use=refuse", "r.log"], &env_seen);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(stderr_of(&out), "");
    assert!(!f.exists());
    let f = sb.touch("r.log");
    let gone = sb.root.join("no-such-proc");
    let env_gone = [("RTRASH_TEST_PROC", gone.to_str().unwrap())];
    let out = sb.run_env(&["put", &pin, "--in-use=refuse", "r.log"], &env_gone);
    let err = stderr_of(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("it may be in use; cannot tell which processes hold files on this platform"),
        "{err}"
    );
    assert!(f.exists());
}

/// Debug builds on Linux only: a path the process query could not check
/// in full is refused under `--in-use=refuse`, and a path checked in full
/// next to it is not, whichever comes first on the command line.
/// `RTRASH_TEST_PARTIAL` makes the path with that name an unchecked one,
/// as a failed Windows query makes it.
#[cfg(all(target_os = "linux", debug_assertions))]
#[test]
fn put_refuse_holds_only_the_unchecked_path_to_its_gap() {
    let sb = Sandbox::new("put-refuse-partial");
    let pin = format!("--trash-dir={}", sb.trash().display());
    let seen = fake_proc(&sb, "proc-seen", &[], &[]);
    let env = [
        ("RTRASH_TEST_PROC", seen.to_str().unwrap()),
        ("RTRASH_TEST_PARTIAL", "a.log"),
    ];
    for order in [["a.log", "b.log"], ["b.log", "a.log"]] {
        let a = sb.touch("a.log");
        let b = sb.touch("b.log");
        let out = sb.run_env(&["put", &pin, "--in-use=refuse", order[0], order[1]], &env);
        let err = stderr_of(&out);
        assert_eq!(out.status.code(), Some(1), "{order:?}: {err}");
        assert!(
            err.contains(
                "cannot trash 'a.log': it may be in use; 1 path could not be checked in full"
            ),
            "{order:?}: {err}"
        );
        assert!(!err.contains("b.log"), "{order:?}: {err}");
        assert!(a.exists(), "{order:?}");
        assert!(!b.exists(), "{order:?}");
        fs::remove_file(&a).unwrap();
        assert!(sb.run(&["empty", &pin]).status.success());
    }
    let a = sb.touch("a.log");
    let out = sb.run_env(&["put", &pin, "-f", "--in-use=refuse", "a.log"], &env);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("warning: 'a.log' may be in use; 1 path could not be checked"),
        "{}",
        stderr_of(&out)
    );
    assert!(!a.exists());
}

/// Debug builds on Linux only: `who` counts the paths it could not check
/// in full, and says nothing for a path it checked in full. The debug seam
/// marks every path with one file name, so two such paths count as two
/// and one of them beside a whole path stays one.
#[cfg(all(target_os = "linux", debug_assertions))]
#[test]
fn who_counts_only_the_paths_it_could_not_check() {
    let sb = Sandbox::new("who-partial");
    sb.touch("a.log");
    sb.touch("b.log");
    let seen = fake_proc(&sb, "proc-seen", &[], &[]);
    let env = [
        ("RTRASH_TEST_PROC", seen.to_str().unwrap()),
        ("RTRASH_TEST_PARTIAL", "a.log"),
    ];
    let out = sb.run_env(&["who", "b.log", "a.log"], &env);
    let err = stderr_of(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("no holder seen, but 1 path could not be checked in full"),
        "{err}"
    );
    assert!(!err.contains("run as root"), "{err}");
    let text = stdout_of(&out);
    let a_line = text
        .lines()
        .find(|l| l.starts_with("a.log:"))
        .unwrap_or_else(|| panic!("{text}"));
    let b_line = text
        .lines()
        .find(|l| l.starts_with("b.log:"))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(
        a_line.contains("the result is partial") && a_line.contains("could not be checked in full"),
        "{a_line}"
    );
    assert!(!b_line.contains("partial"), "{b_line}");
    let out = sb.run_env(&["who", "b.log"], &env);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stderr_of(&out), "");
    let alone = stdout_of(&out);
    assert!(
        alone.contains("b.log: no process holds this path"),
        "{alone}"
    );
    assert!(!alone.contains("partial"), "{alone}");
    fs::create_dir_all(sb.work().join("left")).unwrap();
    fs::create_dir_all(sb.work().join("right")).unwrap();
    sb.touch("left/a.log");
    sb.touch("right/a.log");
    let out = sb.run_env(&["who", "left/a.log", "right/a.log"], &env);
    let err = stderr_of(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("no holder seen, but 2 paths could not be checked in full"),
        "{err}"
    );
    let text = stdout_of(&out);
    for prefix in ["left/a.log:", "right/a.log:"] {
        let line = text
            .lines()
            .find(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("{text}"));
        assert!(line.contains("the result is partial"), "{line}");
    }
    let out = sb.run_env(&["who", "left/a.log", "b.log"], &env);
    let err = stderr_of(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("no holder seen, but 1 path could not be checked in full"),
        "{err}"
    );
    let text = stdout_of(&out);
    let left = text
        .lines()
        .find(|l| l.starts_with("left/a.log:"))
        .unwrap_or_else(|| panic!("{text}"));
    let plain = text
        .lines()
        .find(|l| l.starts_with("b.log:"))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(left.contains("the result is partial"), "{left}");
    assert!(!plain.contains("partial"), "{plain}");
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
        stdout_of(&out).contains("would remove"),
        "{}",
        stdout_of(&out)
    );
    assert!(
        stderr_of(&out).contains("would empty 1 item"),
        "{}",
        stderr_of(&out)
    );
    let mut json: Vec<&str> = args.to_vec();
    json.extend(["-n", "-v", "--json", tmp_arg]);
    let out = reap_cmd(&sb, &json);
    serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", stdout_of(&out)));
    assert!(!stdout_of(&out).contains("would remove"));
    assert!(
        stderr_of(&out).contains("would remove") && stderr_of(&out).contains("gone.txt"),
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
        stderr_of(&out).contains("past RTRASH_WARN_SIZE")
            && stderr_of(&out).contains("rtrash list --sort=size")
            && stderr_of(&out).contains("rtrash rm -i --max-count=1 --match=id ID")
            && !stderr_of(&out).contains("rtrash empty")
            && !stderr_of(&out).contains("would lose the work"),
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
    let out = run(&["-n", "-v", "--json"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let body = stdout_of(&out);
    serde_json::from_str::<serde_json::Value>(&body).unwrap_or_else(|e| panic!("{e}: {body}"));
    assert!(
        files.iter().all(|p| p.exists()),
        "the JSON dry run removed a file"
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
    for p in [
        a.clone(),
        tree.parent().unwrap().parent().unwrap().to_path_buf(),
    ] {
        let out = run(&["put", "-r", p.to_str().unwrap()]);
        assert!(out.status.success(), "{}", stderr_of(&out));
    }
    let out = run(&["rm", "--home-only", a.to_str().unwrap()]);
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
    let out = run(&["rm", "--home-only", "--force", "--match=glob", "orphan.bin"]);
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
        stdout_of(&dry).contains("would remove") && stdout_of(&dry).contains("old.bin"),
        "{}",
        stdout_of(&dry)
    );
    assert!(
        stderr_of(&dry).contains("would empty 1 item"),
        "{}",
        stderr_of(&dry)
    );
    assert!(old.exists());
    let dry_json = run(
        &[
            "empty",
            "--home-only",
            "--until-free=30000",
            "-n",
            "-v",
            "--json",
        ],
        total,
    );
    serde_json::from_slice::<serde_json::Value>(&dry_json.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", stdout_of(&dry_json)));
    assert!(!stdout_of(&dry_json).contains("would remove"));
    assert!(
        stderr_of(&dry_json).contains("would remove") && stderr_of(&dry_json).contains("old.bin"),
        "{}",
        stderr_of(&dry_json)
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
        stderr_of(&out).contains("is low")
            && stderr_of(&out).contains("rtrash list --sort=size")
            && stderr_of(&out).contains("rtrash rm -i --max-count=1 --match=id ID")
            && !stderr_of(&out).contains("rtrash empty")
            && !stderr_of(&out).contains("would lose the work"),
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
/// The reap tests that call it run on Linux and macOS only.
#[cfg(any(target_os = "linux", target_os = "macos"))]
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

/// A first Ctrl-C during `reap` stops after the entry in hand. The entry
/// it trashed has its record, and the entries not reached stay where
/// they were.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn reap_stops_after_the_entry_in_hand_at_the_first_ctrl_c() {
    let sb = Sandbox::new("reap-ctrl-c");
    let tmp = sb.root.join("tmp");
    fs::create_dir_all(&tmp).unwrap();
    let names = ["s1.txt", "s2.txt", "s3.txt"];
    for n in names {
        stale_file(&tmp, n);
    }
    let pin = format!("--trash-dir={}", sb.trash().display());
    let tmp_arg = tmp.to_str().unwrap();
    let out = sb.run_env(
        &["reap", "--older-than=1h", "--age-by=m", &pin, tmp_arg],
        &[("RTRASH_TEST_SIGINT", "after-item:1")],
    );
    let said = stderr_of(&out);
    assert_eq!(out.status.code(), Some(130), "{said}");
    assert!(
        said.contains("stopped by Ctrl-C: 1 reaped, 2 not reached"),
        "{said}"
    );
    let left: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| tmp.join(n).exists())
        .collect();
    assert_eq!(left.len(), 2, "{left:?}");
    let trashed = trash_names(&sb);
    assert_eq!(trashed.len(), 1, "{trashed:?}");
    assert!(!left.contains(&trashed[0].as_str()), "{trashed:?} {left:?}");
    let record = format!("{}.trashinfo", trashed[0]);
    assert_eq!(names_in(&sb.trash().join("info")), [record]);
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

/// `p` as a quoted JSON string, for the Linux and macOS reap tests.
#[cfg(any(target_os = "linux", target_os = "macos"))]
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
    let out = reap_cmd(&sb, &["reap", "--list-profiles", "-v", "--json"]);
    let body = stdout_of(&out);
    serde_json::from_str::<serde_json::Value>(&body).unwrap_or_else(|e| panic!("{e}: {body}"));
    assert!(body.contains(r#""name":"mine","about":"my scratch","age":3600,"action":"permanent""#));

    let out = reap_cmd(
        &sb,
        &["reap", "-p", "mine", "--age-by=m", "-n", "-v", "--json"],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    let body = stdout_of(&out);
    serde_json::from_str::<serde_json::Value>(&body).unwrap_or_else(|e| panic!("{e}: {body}"));
    assert!(old.exists(), "the profile dry run removed its entry");

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

/// Run a sleeper under the file name of `as_exe`, in `cwd`.
///
/// The file name is the process name reap reads. Copying `/usr/bin/sleep`
/// does that only while the copy ignores its argv0. Where coreutils is
/// one binary, an unknown name exits at once. The tests compile a sleeper
/// instead. A fork elsewhere in the test binary can briefly hold the
/// fresh file open for writing, so spawning retries while it is busy.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn spawn_as(as_exe: &Path, cwd: &Path) -> std::process::Child {
    fs::create_dir_all(as_exe.parent().unwrap()).unwrap();
    let src = as_exe.with_extension("rs");
    fs::write(
        &src,
        "fn main() {\n    \
         let secs = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(60);\n    \
         std::thread::sleep(\n        \
         std::time::Duration::from_secs(secs));\n}\n",
    )
    .unwrap();
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let built = Command::new(rustc)
        .arg("--edition=2021")
        .arg("-o")
        .arg(as_exe)
        .arg(&src)
        .output()
        .unwrap_or_else(|e| panic!("cannot run rustc: {e}"));
    assert!(
        built.status.success(),
        "rustc stand-in {}: {}",
        as_exe.display(),
        String::from_utf8_lossy(&built.stderr)
    );
    let _ = fs::remove_file(&src);
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
    assert_eq!(last_items(), 1, "aged empty");
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
    let body = stdout_of(&out);
    let value: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("{e}: {body}"));
    assert_eq!(value["removed"], 1, "{body}");
    assert!(!ghost.exists() && odd.is_none_or(|p| p.exists()));
    assert!(info.join("keep.txt.trashinfo").exists() && files.join("orphan").exists());
    assert_eq!(
        sb.run(&["empty", "--home-only", "--dangling", "3"])
            .status
            .code(),
        Some(2)
    );
}

/// A record whose payload sits in a files/ folder that denies search is
/// not dangling: the payload may well be there, so `empty --dangling`
/// keeps the record (round 261; it took every such record for dangling).
#[test]
fn empty_dangling_keeps_records_whose_payload_cannot_be_reached() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        common::skip("root searches every folder; skipping");
        return;
    }
    let sb = Sandbox::new("dangling-shut");
    let keep = sb.work().join("keep.txt");
    fs::write(&keep, "k").unwrap();
    assert!(sb.run(&["put", keep.to_str().unwrap()]).status.success());
    let files = sb.trash().join("files");
    let record = sb.trash().join("info").join("keep.txt.trashinfo");
    fs::set_permissions(&files, fs::Permissions::from_mode(0o600)).unwrap();
    let out = sb.run(&["empty", "--home-only", "--dangling"]);
    fs::set_permissions(&files, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(record.exists(), "{} {}", stdout_of(&out), stderr_of(&out));
}

/// A file in a folder that denies search is not reported missing: put
/// names the real error (trash-cli #377), and `-f`, which ignores only
/// missing files, does not hide it.
#[test]
fn put_reports_a_file_it_cannot_reach_as_denied_not_missing() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        common::skip("root searches every folder; skipping");
        return;
    }
    let sb = Sandbox::new("put-denied");
    let shut = sb.work().join("shut");
    fs::create_dir(&shut).unwrap();
    fs::write(shut.join("f"), "x").unwrap();
    fs::set_permissions(&shut, fs::Permissions::from_mode(0o600)).unwrap();
    let out = sb.run(&["put", "-f", shut.join("f").to_str().unwrap()]);
    fs::set_permissions(&shut, fs::Permissions::from_mode(0o755)).unwrap();
    let err = stderr_of(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("Permission denied"), "{err}");
    assert!(!err.contains("No such file"), "{err}");
}

/// Debug builds on Linux only: with the mount table unreadable, as
/// Termux may make it (trash-cli #348), put, list and restore still use
/// the home trash without a word, and `du --devices` lists the file
/// systems it can find with one note saying the list is partial.
/// `RTRASH_TEST_NO_MOUNTS` fails every read of the table.
#[cfg(all(target_os = "linux", debug_assertions))]
#[test]
fn an_unreadable_mount_table_leaves_the_home_trash_working() {
    let sb = Sandbox::new("no-mounts");
    let f = sb.touch("nm-file");
    let deny = [("RTRASH_TEST_NO_MOUNTS", "1")];
    let out = sb.run_env(&["put", "nm-file"], &deny);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(stderr_of(&out), "");
    assert!(!f.exists());
    assert!(sb.trash().join("files/nm-file").exists());
    let out = sb.run_env(&["list"], &deny);
    assert!(stdout_of(&out).contains("nm-file"), "{}", stderr_of(&out));
    assert_eq!(stderr_of(&out), "");
    let out = sb.run_env(&["restore", "nm-file"], &deny);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(f.exists());
    let out = sb.run_env(&["du", "--devices"], &deny);
    let err = stderr_of(&out);
    assert!(out.status.success(), "{err}");
    assert_eq!(
        err.matches("cannot read the mount table").count(),
        1,
        "{err}"
    );
    assert!(err.contains("/proc/self/mounts"), "{err}");
    assert!(!stdout_of(&out).is_empty(), "{err}");
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
            && err.contains("rtrash list --sort=size")
            && err.contains("rtrash rm -i --max-count=1 --match=id ID")
            && !err.contains("rtrash empty")
            && !err.contains("would lose the work"),
        "{err}"
    );
    let quiet = write_at(&sb.root, "work/ok.bin", "x");
    let out = sb.run(&["put", "--home-only", quiet.to_str().unwrap()]);
    assert!(!stderr_of(&out).contains("is low"), "{}", stderr_of(&out));
}

/// A put that just moved git work flagged dirty or unpushed must not
/// suggest removing anything from the trash that received it. A plain
/// file put beside the repository is not named. Unknown shares that
/// flag (`GitState::is_risky`).
#[cfg(all(unix, debug_assertions))]
#[test]
fn put_space_hint_keeps_flagged_git_work() {
    if !common::have_git() {
        return;
    }
    let git = |repo: &Path, home: &Path, args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", home)
            .env("GIT_AUTHOR_NAME", "Trash Test")
            .env("GIT_AUTHOR_EMAIL", "trash-test@example.com")
            .env("GIT_COMMITTER_NAME", "Trash Test")
            .env("GIT_COMMITTER_EMAIL", "trash-test@example.com")
            .output()
            .unwrap()
    };
    for (tag, commit) in [("unpushed", true), ("dirty", false)] {
        let sb = Sandbox::new(&format!("put-git-{tag}"));
        let repo = sb.work().join("repo");
        fs::create_dir(&repo).unwrap();
        let init = git(&repo, &sb.root, &["init", "-q"]);
        if !init.status.success() {
            common::skip("git unavailable; skipping");
            return;
        }
        fs::write(repo.join("f"), vec![b'x'; 90_000]).unwrap();
        fs::write(sb.work().join("plain.txt"), b"ok").unwrap();
        if commit {
            let add = git(&repo, &sb.root, &["add", "f"]);
            let committed = git(&repo, &sb.root, &["commit", "-qm", "work"]);
            assert!(
                add.status.success() && committed.status.success(),
                "add: {}\ncommit: {}",
                String::from_utf8_lossy(&add.stderr),
                String::from_utf8_lossy(&committed.stderr)
            );
        }
        let plain = sb.work().join("plain.txt");
        let out = common::rtrash(&sb.root)
            .args([
                "put",
                "--home-only",
                "-r",
                repo.to_str().unwrap(),
                plain.to_str().unwrap(),
            ])
            .env_remove("RTRASH_WARN_SIZE")
            .env("RTRASH_WARN_SIZE", "1")
            .env_remove("RTRASH_MAX_SIZE")
            .env("RTRASH_TEST_FS", format!("100000:{}", sb.root.display()))
            .output()
            .unwrap();
        assert!(out.status.success(), "{tag}: {}", stderr_of(&out));
        let err = stderr_of(&out);
        let remedy = if tag == "dirty" {
            "uncommitted work is committed"
        } else {
            "until the work is pushed"
        };
        assert!(
            err.contains(&format!("with {tag} work"))
                && err.contains("permanently removing it would lose the work")
                && err.contains(&repo.display().to_string())
                && err.contains(remedy)
                && err.contains("`rtrash undo` brings it back")
                && err.contains("is low (")
                && !err.contains("plain.txt")
                && !err.contains("unrecoverable")
                && !err.contains("rtrash empty")
                && !err.contains("rtrash list")
                && !err.contains("rtrash rm"),
            "{tag}: {err}"
        );
        if tag == "dirty" {
            assert!(!err.contains("until the work is pushed"), "{tag}: {err}");
        }
    }
}

/// A repository with both an unpushed commit and an uncommitted file is
/// not described only as unpushed, and is not told only to wait for a push.
#[cfg(all(unix, debug_assertions))]
#[test]
fn put_space_hint_names_dirty_work_that_is_also_unpushed() {
    if !common::have_git() {
        return;
    }
    let sb = Sandbox::new("put-git-both");
    let repo = sb.work().join("repo");
    fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", &sb.root)
            .env("GIT_AUTHOR_NAME", "Trash Test")
            .env("GIT_AUTHOR_EMAIL", "trash-test@example.com")
            .env("GIT_COMMITTER_NAME", "Trash Test")
            .env("GIT_COMMITTER_EMAIL", "trash-test@example.com")
            .output()
            .unwrap()
    };
    let init = git(&["init", "-q"]);
    if !init.status.success() {
        common::skip("git unavailable; skipping");
        return;
    }
    fs::write(repo.join("f"), b"x").unwrap();
    let add = git(&["add", "f"]);
    let committed = git(&["commit", "-qm", "work"]);
    assert!(add.status.success() && committed.status.success());
    fs::write(repo.join("extra"), b"y").unwrap();
    let out = common::rtrash(&sb.root)
        .args(["put", "--home-only", "-r", repo.to_str().unwrap()])
        .env_remove("RTRASH_WARN_SIZE")
        .env("RTRASH_WARN_SIZE", "1")
        .env_remove("RTRASH_MAX_SIZE")
        .env("RTRASH_TEST_FS", format!("100000:{}", sb.root.display()))
        .output()
        .unwrap();
    let err = stderr_of(&out);
    assert!(out.status.success(), "{err}");
    assert!(
        err.contains("with dirty and unpushed work")
            && err.contains("uncommitted work is committed")
            && err.contains("commits are pushed")
            && err.contains("`rtrash undo` brings it back")
            && !err.contains("with unpushed work")
            && !err.contains("rtrash rm")
            && !err.contains("rtrash list")
            && !err.contains("rtrash empty"),
        "{err}"
    );
}

/// Git missing, and a repository git cannot read, are unknown. Neither
/// warning suggests removing the item.
#[cfg(all(unix, debug_assertions))]
#[test]
fn put_space_hint_keeps_a_repo_when_git_cannot_answer() {
    let put_repo = |tag: &str, hide_git: bool| {
        let sb = Sandbox::new(&format!("put-git-unk-{tag}"));
        let repo = sb.work().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(repo.join("f"), vec![b'x'; 90_000]).unwrap();
        let mut cmd = common::rtrash(&sb.root);
        cmd.args(["put", "--home-only", "-r", repo.to_str().unwrap()])
            .env_remove("RTRASH_WARN_SIZE")
            .env("RTRASH_WARN_SIZE", "1")
            .env_remove("RTRASH_MAX_SIZE")
            .env("RTRASH_TEST_FS", format!("100000:{}", sb.root.display()));
        if hide_git {
            let empty = sb.root.join("no-git");
            fs::create_dir_all(&empty).unwrap();
            cmd.env("PATH", &empty);
        }
        let out = cmd.output().unwrap();
        let err = stderr_of(&out);
        assert!(out.status.success(), "{tag}: {err}");
        assert!(
            err.contains("with unknown work")
                && err.contains("could not say")
                && !err.contains("until the work is pushed")
                && !err.contains("rtrash rm")
                && !err.contains("rtrash list")
                && !err.contains("rtrash empty"),
            "{tag}: {err}"
        );
    };
    put_repo("missing", true);
    put_repo("failed", false);
}

fn configure_test_fsmonitor(repo: &Path, home: &Path) -> PathBuf {
    let monitor = home.join("malicious-monitor");
    fs::write(
        &monitor,
        "#!/bin/sh\nprintf 'ran\\n' > \"$RTRASH_TEST_MONITOR_MARK\"\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&monitor, fs::Permissions::from_mode(0o755)).unwrap();
    let configured = git_env(&mut Command::new("git"), repo, home)
        .args(["config", "core.fsmonitor"])
        .arg(&monitor)
        .output()
        .unwrap();
    assert!(configured.status.success(), "{configured:?}");
    home.join("monitor-ran")
}

/// A repository can name a command as its file-system monitor. The Git
/// probes that inspect work before and after a put must not run it.
#[test]
fn put_does_not_run_a_repository_fsmonitor() {
    if !common::have_git() {
        return;
    }
    let sb = Sandbox::new("put-git-fsmonitor");
    let repo = sb.work().join("repo");
    fs::create_dir(&repo).unwrap();
    let init = git_env(&mut Command::new("git"), &repo, &sb.root)
        .args(["init", "-q"])
        .output()
        .unwrap();
    if !init.status.success() {
        common::skip("git unavailable; skipping");
        return;
    }
    let mark = configure_test_fsmonitor(&repo, &sb.root);
    fs::write(repo.join("untracked"), b"work").unwrap();

    let out = common::rtrash(&sb.root)
        .args(["put", "--home-only", "-r"])
        .arg(&repo)
        .env("RTRASH_TEST_MONITOR_MARK", &mark)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!mark.exists(), "repository core.fsmonitor ran");
}

/// An rm probe of a repository already in the trash must not run the
/// repository's file-system monitor.
#[test]
fn rm_does_not_run_a_trashed_repository_fsmonitor() {
    if !common::have_git() {
        return;
    }
    let sb = Sandbox::new("rm-git-fsmonitor");
    let repo = sb.work().join("repo");
    fs::create_dir(&repo).unwrap();
    let init = git_env(&mut Command::new("git"), &repo, &sb.root)
        .args(["init", "-q"])
        .output()
        .unwrap();
    if !init.status.success() {
        common::skip("git unavailable; skipping");
        return;
    }
    fs::write(repo.join("untracked"), b"work").unwrap();
    let original = repo.display().to_string();
    let put = sb.run(&["put", "--home-only", "-r", &original]);
    assert!(put.status.success(), "{}", stderr_of(&put));

    let payload = sb.trash().join("files/repo");
    let mark = configure_test_fsmonitor(&payload, &sb.root);
    let out = common::rtrash(&sb.root)
        .args(["rm", "--home-only", "--match=exact", &original])
        .env("RTRASH_TEST_MONITOR_MARK", &mark)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(!mark.exists(), "repository core.fsmonitor ran during rm");
    assert!(payload.exists());
}

/// A low-space hint probes repositories already in the trash without
/// running their configured file-system monitors.
#[cfg(debug_assertions)]
#[test]
fn put_space_hint_does_not_run_a_trashed_repository_fsmonitor() {
    if !common::have_git() {
        return;
    }
    let sb = Sandbox::new("space-git-fsmonitor");
    let repo = sb.work().join("repo");
    fs::create_dir(&repo).unwrap();
    let init = git_env(&mut Command::new("git"), &repo, &sb.root)
        .args(["init", "-q"])
        .output()
        .unwrap();
    if !init.status.success() {
        common::skip("git unavailable; skipping");
        return;
    }
    fs::write(repo.join("untracked"), b"work").unwrap();
    let put = sb.run(&["put", "--home-only", "-r", repo.to_str().unwrap()]);
    assert!(put.status.success(), "{}", stderr_of(&put));

    let payload = sb.trash().join("files/repo");
    let mark = configure_test_fsmonitor(&payload, &sb.root);
    let plain = sb.work().join("plain.bin");
    fs::write(&plain, vec![b'x'; 90_000]).unwrap();
    let out = common::rtrash(&sb.root)
        .args(["put", "--home-only"])
        .arg(&plain)
        .env_remove("RTRASH_WARN_SIZE")
        .env("RTRASH_WARN_SIZE", "1")
        .env_remove("RTRASH_MAX_SIZE")
        .env("RTRASH_TEST_FS", format!("100000:{}", sb.root.display()))
        .env("RTRASH_TEST_MONITOR_MARK", &mark)
        .output()
        .unwrap();
    let err = stderr_of(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("would lose the work"), "{err}");
    assert!(
        !mark.exists(),
        "repository core.fsmonitor ran during a space hint"
    );
}

/// Status can run a repository's named clean or process filter while it
/// hashes a tracked file. A put's work probe must disable that command too.
#[test]
fn put_does_not_run_a_repository_filter_process() {
    if !common::have_git() {
        return;
    }
    let sb = Sandbox::new("put-git-filter");
    let repo = sb.work().join("repo");
    fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        git_env(&mut Command::new("git"), &repo, &sb.root)
            .args(args)
            .output()
            .unwrap()
    };
    let init = git(&["init", "-q"]);
    if !init.status.success() {
        common::skip("git unavailable; skipping");
        return;
    }
    fs::write(repo.join(".gitattributes"), "tracked filter=hostile\n").unwrap();
    fs::write(repo.join("tracked"), b"first").unwrap();
    assert!(git(&["add", ".gitattributes", "tracked"]).status.success());
    assert!(git(&["commit", "-qm", "base"]).status.success());

    let filter = sb.root.join("malicious-filter");
    fs::write(
        &filter,
        "#!/bin/sh\nprintf 'ran\\n' > \"$RTRASH_TEST_FILTER_MARK\"\nexit 1\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&filter, fs::Permissions::from_mode(0o755)).unwrap();
    let configured = git_env(&mut Command::new("git"), &repo, &sb.root)
        .args(["config", "filter.hostile.process"])
        .arg(&filter)
        .output()
        .unwrap();
    assert!(configured.status.success(), "{configured:?}");
    fs::write(repo.join("tracked"), b"other").unwrap();

    let mark = sb.root.join("filter-ran");
    let out = common::rtrash(&sb.root)
        .args(["put", "--home-only", "-r"])
        .arg(&repo)
        .env("RTRASH_TEST_FILTER_MARK", &mark)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!mark.exists(), "repository filter process ran");
}

/// `git log` honors `log.showSignature`, but a repository cannot make the
/// probe run its configured signature verifier.
#[test]
fn put_does_not_run_a_repository_signature_verifier() {
    if !common::have_git() {
        return;
    }
    let sb = Sandbox::new("put-git-signature");
    let repo = sb.work().join("repo");
    fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        git_env(&mut Command::new("git"), &repo, &sb.root)
            .args(args)
            .output()
            .unwrap()
    };
    if !git(&["init", "-q"]).status.success() {
        common::skip("git unavailable; skipping");
        return;
    }
    fs::write(repo.join("tracked"), b"work").unwrap();
    assert!(git(&["add", "tracked"]).status.success());
    let tree = String::from_utf8_lossy(&git(&["write-tree"]).stdout)
        .trim()
        .to_string();
    let commit = format!(
        "tree {tree}\n\
author Trash Test <trash-test@example.com> 1 +0000\n\
committer Trash Test <trash-test@example.com> 1 +0000\n\
gpgsig -----BEGIN PGP SIGNATURE-----\n invalid\n -----END PGP SIGNATURE-----\n\n\
signed\n"
    );
    let mut hash = git_env(&mut Command::new("git"), &repo, &sb.root)
        .args(["hash-object", "-t", "commit", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    hash.stdin
        .take()
        .unwrap()
        .write_all(commit.as_bytes())
        .unwrap();
    let hash = hash.wait_with_output().unwrap();
    assert!(hash.status.success(), "{hash:?}");
    let oid = String::from_utf8_lossy(&hash.stdout).trim().to_string();
    assert!(git(&["update-ref", "HEAD", &oid]).status.success());

    let verifier = sb.root.join("malicious-verifier");
    fs::write(
        &verifier,
        "#!/bin/sh\nprintf 'ran\\n' > \"$RTRASH_TEST_GPG_MARK\"\nexit 0\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&verifier, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(git_env(&mut Command::new("git"), &repo, &sb.root)
        .args(["config", "log.showSignature", "true"])
        .status()
        .unwrap()
        .success());
    assert!(git_env(&mut Command::new("git"), &repo, &sb.root)
        .args(["config", "gpg.program"])
        .arg(&verifier)
        .status()
        .unwrap()
        .success());

    let mark = sb.root.join("gpg-ran");
    let out = common::rtrash(&sb.root)
        .args(["put", "--home-only", "-r"])
        .arg(&repo)
        .env("RTRASH_TEST_GPG_MARK", &mark)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!mark.exists(), "repository gpg.program ran");
}

/// Environment-pair overrides keep the whole key when a filter subsection
/// contains `=`; `-c key=` would split it at the wrong byte.
#[test]
fn put_does_not_run_a_filter_whose_subsection_contains_equals() {
    if !common::have_git() {
        return;
    }
    let sb = Sandbox::new("put-git-filter-equals");
    let repo = sb.work().join("repo");
    fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        git_env(&mut Command::new("git"), &repo, &sb.root)
            .args(args)
            .output()
            .unwrap()
    };
    if !git(&["init", "-q"]).status.success() {
        common::skip("git unavailable; skipping");
        return;
    }
    fs::write(repo.join(".gitattributes"), "tracked filter=x=y\n").unwrap();
    fs::write(repo.join("tracked"), b"first").unwrap();
    assert!(git(&["add", ".gitattributes", "tracked"]).status.success());
    assert!(git(&["commit", "-qm", "base"]).status.success());
    let filter = sb.root.join("malicious-equals-filter");
    fs::write(
        &filter,
        "#!/bin/sh\nprintf 'ran\\n' > \"$RTRASH_TEST_FILTER_MARK\"\nexit 1\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&filter, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(git_env(&mut Command::new("git"), &repo, &sb.root)
        .args(["config", "filter.x=y.process"])
        .arg(&filter)
        .status()
        .unwrap()
        .success());
    fs::write(repo.join("tracked"), b"other").unwrap();

    let mark = sb.root.join("equals-filter-ran");
    let out = common::rtrash(&sb.root)
        .args(["put", "--home-only", "-r"])
        .arg(&repo)
        .env("RTRASH_TEST_FILTER_MARK", &mark)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!mark.exists(), "repository filter.x=y.process ran");
}

/// `GIT_CONFIG` cannot hide repository program keys from the config scan
/// while status still reads and runs them.
#[test]
fn put_clears_git_config_before_scanning_repository_programs() {
    if !common::have_git() {
        return;
    }
    let sb = Sandbox::new("put-git-config-env");
    let repo = sb.work().join("repo");
    fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        git_env(&mut Command::new("git"), &repo, &sb.root)
            .args(args)
            .output()
            .unwrap()
    };
    if !git(&["init", "-q"]).status.success() {
        common::skip("git unavailable; skipping");
        return;
    }
    fs::write(repo.join(".gitattributes"), "tracked filter=hostile\n").unwrap();
    fs::write(repo.join("tracked"), b"first").unwrap();
    assert!(git(&["add", ".gitattributes", "tracked"]).status.success());
    assert!(git(&["commit", "-qm", "base"]).status.success());
    let filter = sb.root.join("malicious-hidden-filter");
    fs::write(
        &filter,
        "#!/bin/sh\nprintf 'ran\\n' > \"$RTRASH_TEST_FILTER_MARK\"\nexit 1\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&filter, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(git_env(&mut Command::new("git"), &repo, &sb.root)
        .args(["config", "filter.hostile.process"])
        .arg(&filter)
        .status()
        .unwrap()
        .success());
    fs::write(repo.join("tracked"), b"other").unwrap();

    let mark = sb.root.join("hidden-filter-ran");
    let out = common::rtrash(&sb.root)
        .args(["put", "--home-only", "-r"])
        .arg(&repo)
        .env("GIT_CONFIG", "/dev/null")
        .env("RTRASH_TEST_FILTER_MARK", &mark)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!mark.exists(), "GIT_CONFIG hid the repository filter");
}

/// Undo is named only once the item has moved. A dry run leaves it in place
/// and does not promise to bring it back.
#[test]
fn a_dry_run_does_not_promise_undo_before_the_move() {
    if !common::have_git() {
        return;
    }
    let sb = Sandbox::new("put-git-dry");
    let repo = sb.work().join("repo");
    fs::create_dir(&repo).unwrap();
    let init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", &sb.root)
        .output()
        .unwrap();
    if !init.status.success() {
        common::skip("git unavailable; skipping");
        return;
    }
    fs::write(repo.join("f"), b"x").unwrap();
    let dry = sb.run(&[
        "put",
        "--home-only",
        "--dry-run",
        "-r",
        repo.to_str().unwrap(),
    ]);
    let err = stderr_of(&dry);
    let out = stdout_of(&dry);
    assert!(dry.status.success(), "{err}");
    assert!(out.contains("would trash"), "{out}");
    assert!(!err.contains("rtrash undo"), "{err}");
    assert!(repo.join("f").is_file());
    let moved = sb.run(&["put", "--home-only", "-r", repo.to_str().unwrap()]);
    assert!(moved.status.success(), "{}", stderr_of(&moved));
    assert!(
        stderr_of(&moved).contains("`rtrash undo` brings it back"),
        "{}",
        stderr_of(&moved)
    );
    assert!(!repo.join("f").exists());
}

/// `put --json` keeps stderr to errors. The note about a trashed
/// repository is left out, and the summary still lists the item.
#[cfg(unix)]
#[test]
fn put_json_leaves_out_the_repository_note() {
    if !common::have_git() {
        return;
    }
    let sb = Sandbox::new("put-repo-json");
    let repo = sb.work().join("repo");
    fs::create_dir(&repo).unwrap();
    let init = git_env(&mut Command::new("git"), &repo, &sb.root)
        .args(["init", "-q"])
        .output()
        .unwrap();
    if !init.status.success() {
        common::skip("git unavailable; skipping");
        return;
    }
    fs::write(repo.join("f"), b"x").unwrap();
    let out = sb.run(&["put", "--json", "-r", "repo"]);
    let err = stderr_of(&out);
    assert!(out.status.success(), "{err}");
    assert!(!repo.join("f").exists(), "{err}");
    assert!(!err.contains("contains a git repository"), "{err}");
    assert!(!err.contains("rtrash undo"), "{err}");
    let json = stdout_of(&out);
    assert!(
        json.starts_with("{\"trashed\":[{\"original\":\"")
            && json.contains("/repo\",\"name\":\"repo\"")
            && json.trim_end().ends_with("\"failed\":0}"),
        "{json}"
    );
}

/// `--dry-run` does not claim to run the git work check.
#[test]
fn put_dry_run_help_does_not_claim_the_git_check() {
    let out = common::rtrash_real(common::Real::NoTrash)
        .args(["put", "--help"])
        .output()
        .unwrap();
    let text = stdout_of(&out);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(text.contains("Git work is not reported"), "{text}");
    assert!(!text.contains("failsafes, git work"), "{text}");
}

fn git_env<'a>(cmd: &'a mut Command, repo: &Path, home: &Path) -> &'a mut Command {
    cmd.current_dir(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", home)
        .env("GIT_AUTHOR_NAME", "Trash Test")
        .env("GIT_AUTHOR_EMAIL", "trash-test@example.com")
        .env("GIT_COMMITTER_NAME", "Trash Test")
        .env("GIT_COMMITTER_EMAIL", "trash-test@example.com")
}

/// A repository nested under the directory that was trashed is risky.
/// The warning does not suggest `rtrash rm`.
#[cfg(all(unix, debug_assertions))]
#[test]
fn put_of_a_parent_keeps_a_nested_dirty_repository() {
    if !common::have_git() {
        return;
    }
    let sb = Sandbox::new("put-git-nested");
    let repo = sb.work().join("parent").join("child");
    fs::create_dir_all(&repo).unwrap();
    let init = git_env(&mut Command::new("git"), &repo, &sb.root)
        .args(["init", "-q"])
        .output()
        .unwrap();
    if !init.status.success() {
        common::skip("git unavailable; skipping");
        return;
    }
    fs::write(repo.join("f"), b"x").unwrap();
    let parent = sb.work().join("parent");
    let out = common::rtrash(&sb.root)
        .args(["put", "--home-only", "-r", parent.to_str().unwrap()])
        .env_remove("RTRASH_WARN_SIZE")
        .env("RTRASH_WARN_SIZE", "1")
        .env("RTRASH_TEST_FS", format!("100000:{}", sb.root.display()))
        .output()
        .unwrap();
    let err = stderr_of(&out);
    assert!(out.status.success(), "{err}");
    assert!(
        err.contains("would lose the work")
            && err.contains("uncommitted")
            && !err.contains("rtrash rm")
            && !err.contains("rtrash list"),
        "{err}"
    );
}

/// A git shim whose log succeeds and whose status or stash probe fails
/// is unknown as well as unpushed. The warning does not suggest removal.
#[cfg(all(unix, debug_assertions))]
#[test]
fn a_partial_git_probe_stays_unknown() {
    if !common::have_git() {
        return;
    }
    let sb = Sandbox::new("put-git-shim");
    let shim = sb.root.join("shim");
    fs::create_dir(&shim).unwrap();
    let git = shim.join("git");
    fs::write(
        &git,
        "#!/bin/sh\n\
         ver=\"${FAKE_GIT_VER:-2.47.0}\"\n\
         for a in \"$@\"; do\n\
           case \"$a\" in --version) printf 'git version %s\\n' \"$ver\"; exit 0 ;; esac\n\
         done\n\
         cmd=\n\
         for a in \"$@\"; do\n\
           case \"$a\" in log|status|rev-parse) cmd=$a ;; esac\n\
         done\n\
         case \"$cmd\" in\n\
           log) printf 'abcdef\\n'; exit 0 ;;\n\
           status) exit \"${FAKE_GIT_STATUS:-0}\" ;;\n\
           rev-parse) exit \"${FAKE_GIT_REV:-1}\" ;;\n\
         esac\n\
         exit 0\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", shim.display(), std::env::var("PATH").unwrap());
    for (tag, ver, status, rev) in [
        ("status", "2.47.0", "2", "0"),
        ("stash", "2.34.0", "0", "128"),
    ] {
        let repo = sb.work().join(tag);
        let _ = fs::remove_dir_all(&repo);
        fs::create_dir_all(repo.join(".git")).unwrap();
        let out = common::rtrash(&sb.root)
            .args(["put", "--home-only", "-r", repo.to_str().unwrap()])
            .env("PATH", &path)
            .env("FAKE_GIT_VER", ver)
            .env("FAKE_GIT_STATUS", status)
            .env("FAKE_GIT_REV", rev)
            .env_remove("RTRASH_WARN_SIZE")
            .env("RTRASH_WARN_SIZE", "1")
            .env("RTRASH_TEST_FS", format!("100000:{}", sb.root.display()))
            .output()
            .unwrap();
        let err = stderr_of(&out);
        assert!(out.status.success(), "{tag}: {err}");
        assert!(
            err.contains("unpushed and unknown")
                && err.contains("until the work is pushed")
                && err.contains("could not say")
                && !err.contains("rtrash rm"),
            "{tag}: {err}"
        );
    }
}

/// A later status, and a later plain put into the same trash, still see
/// the repository and do not suggest removing it.
#[cfg(all(unix, debug_assertions))]
#[test]
fn a_later_status_and_put_keep_a_trashed_repository() {
    if !common::have_git() {
        return;
    }
    let sb = Sandbox::new("put-git-later");
    let repo = sb.work().join("repo");
    fs::create_dir(&repo).unwrap();
    let init = git_env(&mut Command::new("git"), &repo, &sb.root)
        .args(["init", "-q"])
        .output()
        .unwrap();
    if !init.status.success() {
        common::skip("git unavailable; skipping");
        return;
    }
    fs::write(repo.join("f"), b"x").unwrap();
    let fake = format!("100000:{}", sb.root.display());
    let put = common::rtrash(&sb.root)
        .args(["put", "--home-only", "-r", repo.to_str().unwrap()])
        .env("RTRASH_TEST_FS", &fake)
        .env("RTRASH_WARN_SIZE", "1")
        .output()
        .unwrap();
    assert!(put.status.success(), "{}", stderr_of(&put));
    let status = common::rtrash(&sb.root)
        .args(["status", "--home-only"])
        .env("RTRASH_TEST_FS", &fake)
        .output()
        .unwrap();
    let err = stderr_of(&status);
    assert!(
        err.contains("would lose the work") && !err.contains("rtrash rm"),
        "{err}"
    );
    let plain = sb.work().join("plain.txt");
    fs::write(&plain, b"ok").unwrap();
    let again = common::rtrash(&sb.root)
        .args(["put", "--home-only", plain.to_str().unwrap()])
        .env("RTRASH_TEST_FS", &fake)
        .env("RTRASH_WARN_SIZE", "1")
        .output()
        .unwrap();
    let err = stderr_of(&again);
    assert!(again.status.success(), "{err}");
    assert!(
        err.contains("would lose the work") && !err.contains("rtrash rm"),
        "{err}"
    );
}

/// rm leaves a payload that still holds risky git work, and `-f` removes it.
#[cfg(unix)]
#[test]
fn rm_refuses_a_repository_that_still_holds_risky_work() {
    if !common::have_git() {
        return;
    }
    let sb = Sandbox::new("rm-git-keep");
    let repo = sb.work().join("repo");
    fs::create_dir(&repo).unwrap();
    let init = git_env(&mut Command::new("git"), &repo, &sb.root)
        .args(["init", "-q"])
        .output()
        .unwrap();
    if !init.status.success() {
        common::skip("git unavailable; skipping");
        return;
    }
    fs::write(repo.join("f"), b"x").unwrap();
    let path = repo.to_str().unwrap();
    let put = sb.run(&["put", "--home-only", "-r", path]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let kept = sb.run(&["rm", "--home-only", "--match=exact", path]);
    let err = stderr_of(&kept);
    assert_eq!(kept.status.code(), Some(1), "{err}");
    assert!(err.contains("refusing to remove"), "{err}");
    assert!(err.contains("-f"), "{err}");
    let listed = stdout_of(&sb.run(&["list", "--home-only"]));
    assert!(listed.contains("repo"), "{listed}");
    let forced = sb.run(&["rm", "--home-only", "-f", "--match=exact", path]);
    assert!(forced.status.success(), "{}", stderr_of(&forced));
    let listed = stdout_of(&sb.run(&["list", "--home-only"]));
    assert!(!listed.contains("repo"), "{listed}");
}

/// A dangling record is removable: once the payload is gone, rm does not
/// probe git work and does not refuse it as unknown.
#[cfg(unix)]
#[test]
fn rm_removes_a_dangling_record_without_a_git_work_refusal() {
    if !common::have_git() {
        return;
    }
    let sb = Sandbox::new("rm-git-dangling");
    let repo = sb.work().join("repo");
    fs::create_dir(&repo).unwrap();
    let init = git_env(&mut Command::new("git"), &repo, &sb.root)
        .args(["init", "-q"])
        .output()
        .unwrap();
    if !init.status.success() {
        common::skip("git unavailable; skipping");
        return;
    }
    fs::write(repo.join("f"), b"x").unwrap();
    let path = repo.to_str().unwrap();
    let put = sb.run(&["put", "--home-only", "-r", path]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let files = sb.trash().join("files");
    let payload = fs::read_dir(&files)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::remove_dir_all(&payload).unwrap();
    let rm = sb.run(&["rm", "--home-only", "--match=exact", path]);
    let err = stderr_of(&rm);
    assert!(rm.status.success(), "{err}");
    assert!(!err.contains("it holds unknown git work"), "{err}");
    let listed = stdout_of(&sb.run(&["list", "--home-only"]));
    assert!(!listed.contains("repo"), "{listed}");
}

/// Work that appears after the first probe and before the move is kept.
#[cfg(all(unix, debug_assertions))]
#[test]
fn work_created_after_the_probe_is_still_flagged() {
    if !common::have_git() {
        return;
    }
    let sb = Sandbox::new("put-git-seam");
    let repo = sb.work().join("repo");
    fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        git_env(&mut Command::new("git"), &repo, &sb.root)
            .args(args)
            .output()
            .unwrap()
    };
    if !git(&["init", "-q"]).status.success() {
        common::skip("git unavailable; skipping");
        return;
    }
    fs::write(repo.join("f"), b"x").unwrap();
    assert!(git(&["add", "f"]).status.success());
    assert!(git(&["commit", "-qm", "work"]).status.success());
    let abs = fs::canonicalize(&repo).unwrap();
    let out = common::rtrash(&sb.root)
        .args(["put", "--home-only", "-r", abs.to_str().unwrap()])
        .env("RTRASH_TEST_AFTER_PROBE", abs.to_str().unwrap())
        .env("RTRASH_WARN_SIZE", "1")
        .env("RTRASH_TEST_FS", format!("100000:{}", sb.root.display()))
        .output()
        .unwrap();
    let err = stderr_of(&out);
    assert!(out.status.success(), "{err}");
    assert!(
        err.contains("with dirty and unpushed work")
            && err.contains("uncommitted")
            && !err.contains("with unpushed work")
            && !err.contains("rtrash rm"),
        "{err}"
    );
}

/// The id from `list` names that item. A later item at the same original
/// path has another id, and `rm --match=id` of the first removes nothing.
#[test]
fn rm_by_id_leaves_a_later_item_at_the_same_path() {
    let sb = Sandbox::new("rm-id");
    let f = sb.work().join("same.txt");
    fs::write(&f, b"one").unwrap();
    assert!(sb
        .run(&["put", "--home-only", f.to_str().unwrap()])
        .status
        .success());
    let listed = stdout_of(&sb.run(&["list", "--home-only", "--format={id} {path}"]));
    let id = listed.split_whitespace().next().unwrap().to_string();
    assert!(id.contains('@'), "{listed}");
    assert!(sb.run(&["undo"]).status.success());
    common::next_trash_second();
    fs::write(&f, b"two").unwrap();
    assert!(sb
        .run(&["put", "--home-only", f.to_str().unwrap()])
        .status
        .success());
    let rm = sb.run(&["rm", "--home-only", "--match=id", &id]);
    assert_eq!(rm.status.code(), Some(1), "{}", stderr_of(&rm));
    assert!(
        stderr_of(&rm).contains("no trash items match"),
        "{}",
        stderr_of(&rm)
    );
    assert!(stdout_of(&rm).is_empty(), "{}", stdout_of(&rm));
    let files = sb.trash().join("files");
    let mut bodies = Vec::new();
    for ent in fs::read_dir(&files).unwrap() {
        bodies.push(fs::read(ent.unwrap().path()).unwrap());
    }
    assert_eq!(bodies, vec![b"two".to_vec()]);
    assert!(!f.exists());
}

/// Two items with the same trash name and deletion second under different
/// trash roots get different ids, and rm by one id removes only that item.
#[test]
fn rm_by_id_distinguishes_items_from_different_trash_roots() {
    let sb = Sandbox::new("rm-id-roots");
    let roots = [sb.root.join("t1"), sb.root.join("t2")];
    let date = "2026-01-02T03:04:05";
    for (i, root) in roots.iter().enumerate() {
        fs::create_dir_all(root.join("files")).unwrap();
        fs::create_dir_all(root.join("info")).unwrap();
        fs::write(root.join("files/same.txt"), format!("v{i}")).unwrap();
        fs::write(
            root.join("info/same.txt.trashinfo"),
            format!(
                "[Trash Info]\nPath=/work/{i}/same.txt\nDeletionDate={date}\nX-Rtrash-Batch=1-1\n"
            ),
        )
        .unwrap();
    }
    let pin = |p: &PathBuf| format!("--trash-dir={}", p.display());
    let a = pin(&roots[0]);
    let b = pin(&roots[1]);
    let listed = stdout_of(&sb.run(&["list", &a, &b, "--format={id}\t{path}\n"]));
    let rows: Vec<(&str, &str)> = listed.lines().filter_map(|l| l.split_once('\t')).collect();
    assert_eq!(rows.len(), 2, "{listed}");
    assert_ne!(rows[0].0, rows[1].0, "{listed}");
    let rm = sb.run(&["rm", &a, &b, "--match=id", rows[0].0]);
    assert!(rm.status.success(), "{}", stderr_of(&rm));
    let left: usize = roots
        .iter()
        .map(|r| fs::read_dir(r.join("files")).unwrap().count())
        .sum();
    assert_eq!(left, 1);
}

/// A record whose payload is gone is not git work at risk: rm takes it
/// without -f instead of refusing it as unknown.
#[cfg(unix)]
#[test]
fn rm_takes_a_dangling_record_without_refusing_it_as_unknown_git_work() {
    let sb = Sandbox::new("rm-dangling");
    let f = sb.work().join("gone.txt");
    fs::write(&f, b"x").unwrap();
    let path = f.to_str().unwrap();
    assert!(sb.run(&["put", "--home-only", path]).status.success());
    fs::remove_file(sb.trash().join("files/gone.txt")).unwrap();
    let record = sb.trash().join("info/gone.txt.trashinfo");
    assert!(record.exists());
    let dry = sb.run(&["rm", "--home-only", "-n", "--match=exact", path]);
    assert!(
        !stderr_of(&dry).contains("refusing to remove"),
        "{}",
        stderr_of(&dry)
    );
    let rm = sb.run(&["rm", "--home-only", "--match=exact", path]);
    assert!(rm.status.success(), "{}", stderr_of(&rm));
    assert!(
        !stderr_of(&rm).contains("refusing to remove"),
        "{}",
        stderr_of(&rm)
    );
    assert!(!record.exists());
}

/// A backslash is a file name character on Unix, so rm of `plain\\` removes
/// that item and leaves `plain`, and the other way round.
#[cfg(unix)]
#[test]
fn rm_exact_keeps_a_name_that_differs_by_a_trailing_backslash() {
    let sb = Sandbox::new("rm-backslash");
    let plain = sb.work().join("plain");
    let slashed = sb.work().join("plain\\");
    fs::write(&plain, b"plain").unwrap();
    fs::write(&slashed, b"slashed").unwrap();
    for f in [&plain, &slashed] {
        let put = sb.run(&["put", "--home-only", f.to_str().unwrap()]);
        assert!(put.status.success(), "{}", stderr_of(&put));
    }
    let rm = sb.run(&[
        "rm",
        "--home-only",
        "--match=exact",
        slashed.to_str().unwrap(),
    ]);
    assert!(rm.status.success(), "{}", stderr_of(&rm));
    let left = listed_originals(&sb);
    assert_eq!(left, vec![plain.to_str().unwrap().to_string()], "{left:?}");
    let rm = sb.run(&[
        "rm",
        "--home-only",
        "--match=exact",
        plain.to_str().unwrap(),
    ]);
    assert!(rm.status.success(), "{}", stderr_of(&rm));
    assert!(listed_originals(&sb).is_empty());
}

/// `""` is not the working directory. rm and empty refuse it, and a
/// trashed copy of that directory stays.
#[test]
fn an_empty_operand_is_not_the_working_directory() {
    let sb = Sandbox::new("rm-empty-operand");
    let dir = sb.work().join("here");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("f"), b"x").unwrap();
    let put = sb.run(&["put", "--home-only", "-r", dir.to_str().unwrap()]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    fs::create_dir(&dir).unwrap();
    for mode in [
        &["rm", "--home-only", "--match=exact", ""][..],
        &["rm", "--home-only", ""][..],
        &["rm", "--home-only", "--", ""][..],
    ] {
        let rm = common::rtrash(&sb.root)
            .args(mode)
            .current_dir(&dir)
            .output()
            .unwrap();
        assert_eq!(rm.status.code(), Some(2), "{}", stderr_of(&rm));
        assert!(
            stderr_of(&rm).contains("'' is not a path"),
            "{}",
            stderr_of(&rm)
        );
        assert_eq!(
            listed_originals(&sb),
            vec![dir.to_str().unwrap().to_string()]
        );
    }
    let pin = format!("--trash-dir={}", sb.trash().display());
    for mode in [
        vec!["empty", pin.as_str(), ""],
        vec!["empty", pin.as_str(), "--", ""],
    ] {
        let empty = common::rtrash(&sb.root)
            .args(mode)
            .current_dir(&dir)
            .output()
            .unwrap();
        assert_eq!(empty.status.code(), Some(2), "{}", stderr_of(&empty));
        assert!(
            stderr_of(&empty).contains("'' is not a path"),
            "{}",
            stderr_of(&empty)
        );
        assert_eq!(
            listed_originals(&sb),
            vec![dir.to_str().unwrap().to_string()]
        );
    }
}

/// `--` ends empty's options, so `-n` after it is a path.
#[test]
fn empty_takes_a_double_dash_as_the_end_of_its_options() {
    let sb = Sandbox::new("empty-ddash");
    sb.touch("a.txt");
    assert!(sb.run(&["put", "a.txt"]).status.success());
    let out = sb.empty(&["--", "-n"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(!err.contains("unrecognized option"), "{err}");
    assert!(err.contains("no trash items match"), "{err}");
    assert!(!err.contains("Would remove"), "{err}");
    assert!(!err.contains("Removed"), "{err}");
    assert_eq!(trash_names(&sb), vec!["a.txt".to_string()]);
    let path = sb.work().join("a.txt").to_string_lossy().into_owned();
    let gone = sb.empty(&["--", &path]);
    assert!(gone.status.success(), "{}", stderr_of(&gone));
    assert!(trash_names(&sb).is_empty(), "{}", stderr_of(&gone));
}

/// `link/..` is the directory above the link's target, as put resolves a
/// parent, not the directory that holds the link. rm of it removes the item
/// trashed from the physical parent and keeps the one from the lexical one.
#[cfg(unix)]
#[test]
fn rm_resolves_a_trailing_dot_dot_after_a_link_physically() {
    let sb = Sandbox::new("rm-link-dotdot");
    let (real, holder) = (sb.work().join("real"), sb.work().join("holder"));
    for d in [&real, &holder] {
        fs::create_dir_all(d.join("sub")).unwrap();
        let put = sb.run(&["put", "--home-only", "-r", d.to_str().unwrap()]);
        assert!(put.status.success(), "{}", stderr_of(&put));
    }
    fs::create_dir_all(real.join("sub")).unwrap();
    fs::create_dir(&holder).unwrap();
    std::os::unix::fs::symlink(real.join("sub"), holder.join("link")).unwrap();
    let typed = format!("{}/link/..", holder.display());
    let rm = sb.run(&["rm", "--home-only", "--match=exact", &typed]);
    assert!(rm.status.success(), "{}", stderr_of(&rm));
    let left = listed_originals(&sb);
    assert_eq!(left, vec![holder.to_str().unwrap().to_string()], "{left:?}");
}

/// `link/..`, with `link` pointing at `../real`, names the physical parent.
/// put refuses that `..` operand. rm and empty must leave a decoy that was
/// put at the lexical parent.
#[test]
fn rm_and_empty_leave_a_decoy_at_the_lexical_parent_of_link_dotdot() {
    let sb = Sandbox::new("link-dotdot");
    let work = sb.work();
    fs::create_dir(work.join("real")).unwrap();
    fs::write(work.join("real/inside"), b"in").unwrap();
    fs::create_dir(work.join("sub")).unwrap();
    fs::write(work.join("sub/decoy"), b"decoy").unwrap();
    let put = sb.run(&["put", "--home-only", "-r", "sub"]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    assert!(sb.trash().join("files/sub/decoy").is_file());

    fs::create_dir(work.join("sub")).unwrap();
    std::os::unix::fs::symlink("../real", work.join("sub/link")).unwrap();
    let refused = sb.run(&["put", "--home-only", "-r", "sub/link/.."]);
    assert_eq!(refused.status.code(), Some(1), "{}", stderr_of(&refused));
    assert!(
        stderr_of(&refused).contains("'.' or '..'"),
        "{}",
        stderr_of(&refused)
    );
    assert!(sb.trash().join("files/sub/decoy").is_file());

    let dry = sb.run(&["rm", "--home-only", "-n", "sub/link/.."]);
    assert_eq!(dry.status.code(), Some(1), "{}", stderr_of(&dry));
    assert!(stderr_of(&dry).contains("no exact match"));
    let rm = sb.run(&["rm", "--home-only", "-f", "sub/link/.."]);
    assert_eq!(rm.status.code(), Some(1), "{}", stderr_of(&rm));
    assert!(
        sb.trash().join("files/sub/decoy").is_file(),
        "{}",
        stderr_of(&rm)
    );
    let empty = sb.empty(&["sub/link/.."]);
    assert_eq!(empty.status.code(), Some(1), "{}", stderr_of(&empty));
    assert!(stderr_of(&empty).contains("no trash items match"));
    assert!(
        sb.trash().join("files/sub/decoy").is_file(),
        "{}",
        stderr_of(&empty)
    );
}

/// A symlinked parent that is retargeted after the path was resolved does
/// not make put move the new target. The item that was checked moves.
#[cfg(all(unix, debug_assertions))]
#[test]
fn a_retargeted_parent_does_not_change_what_put_moves() {
    let sb = Sandbox::new("put-retarget");
    let real = sb.work().join("real");
    let other = sb.work().join("other");
    fs::create_dir(&real).unwrap();
    fs::create_dir(&other).unwrap();
    fs::write(real.join("item"), b"orig").unwrap();
    fs::write(other.join("item"), b"other").unwrap();
    let link = sb.work().join("via");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let spec = format!("{}|{}", link.display(), other.display());
    let operand = link.join("item");
    let out = common::rtrash(&sb.root)
        .args(["put", "--home-only", operand.to_str().unwrap()])
        .env("RTRASH_TEST_RETARGET", &spec)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!real.join("item").exists(), "the checked item moves");
    assert_eq!(fs::read(other.join("item")).unwrap(), b"other");
    let files = sb.trash().join("files");
    let mut saw = false;
    for ent in fs::read_dir(&files).unwrap() {
        if fs::read(ent.unwrap().path()).unwrap() == b"orig" {
            saw = true;
        }
    }
    assert!(saw, "the trash holds the item that was checked");
}

/// A WSL put into the Windows Recycle Bin probes the moved payload and does
/// not promise `rtrash undo`.
#[cfg(unix)]
#[test]
fn a_wsl_put_of_a_risky_repo_names_wsl_restore() {
    if !common::have_git() {
        return;
    }
    let sb = Sandbox::new("wsl-git");
    let bin_dir = sb.root.join("bin");
    fs::create_dir(&bin_dir).unwrap();
    let write_exe = |name: &str, body: &str| {
        let p = bin_dir.join(name);
        fs::write(&p, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    };
    write_exe("wslpath", "#!/bin/sh\nprintf '%s\\n' \"$2\"\n");
    write_exe(
        "powershell.exe",
        "#!/bin/sh\nprintf x > \"$RTRASH_WSL_TARGET/.rtrash-probe-seam\"\n\
         mv -- \"$RTRASH_WSL_TARGET\" \"$RTRASH_TEST_WSL_PAYLOAD\"\n",
    );
    let repo = sb.root.join("mnt").join("c").join("repo");
    fs::create_dir_all(&repo).unwrap();
    let init = git_env(&mut Command::new("git"), &repo, &sb.root)
        .args(["init", "-q"])
        .output()
        .unwrap();
    if !init.status.success() {
        common::skip("git unavailable; skipping");
        return;
    }
    fs::write(repo.join("f"), b"x").unwrap();
    assert!(git_env(&mut Command::new("git"), &repo, &sb.root)
        .args(["add", "f"])
        .status()
        .unwrap()
        .success());
    assert!(git_env(&mut Command::new("git"), &repo, &sb.root)
        .args(["commit", "-qm", "work"])
        .status()
        .unwrap()
        .success());
    let sid = "S-1-5-21-test";
    let recycle = sb.root.join("mnt/c/$Recycle.Bin").join(sid);
    fs::create_dir_all(&recycle).unwrap();
    let payload = recycle.join("$RPOST");
    let index = recycle.join("$IPOST");
    fs::write(
        index,
        rtrash::win_recycle::encode_recycle_i_v2(
            Path::new(r"C:\repo"),
            0,
            rtrash::win_recycle::FILETIME_UNIX_EPOCH,
        ),
    )
    .unwrap();
    let conf = sb.root.join("wsl.conf");
    fs::write(
        &conf,
        format!("[automount]\nroot = {}\n", sb.root.join("mnt").display()),
    )
    .unwrap();
    let rel = sb.root.join("osrelease");
    fs::write(&rel, "microsoft\n").unwrap();
    let path = format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap());
    let out = common::rtrash(&sb.root)
        .args(["put", "-r", repo.to_str().unwrap()])
        .env("PATH", &path)
        .env("RTRASH_WSL_RECYCLE", "1")
        .env("RTRASH_WSL_OSRELEASE", &rel)
        .env("RTRASH_WSL_CONF", &conf)
        .env("RTRASH_WSL_SID", sid)
        .env("RTRASH_TEST_WSL_PAYLOAD", &payload)
        .output()
        .unwrap();
    let err = stderr_of(&out);
    assert!(out.status.success(), "{err}");
    assert!(!repo.exists(), "{err}");
    assert!(payload.join(".rtrash-probe-seam").is_file(), "{err}");
    assert!(
        err.contains("rtrash wsl restore")
            && err.contains("Explorer")
            && err.contains("dirty and unpushed work")
            && !err.contains("`rtrash undo` brings it back")
            && !err.contains("rtrash rm"),
        "{err}"
    );
}

/// The Finder undo line is printed only after the log is written.
#[cfg(unix)]
#[test]
fn a_finder_undo_log_that_cannot_be_written_does_not_promise_undo() {
    let sb = Sandbox::new("finder-log");
    let state = sb.root.join("state-file");
    fs::write(&state, b"x").unwrap();
    let target = sb.work().join("t");
    fs::write(&target, b"z").unwrap();
    let link = sb.work().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let out = common::rtrash(&sb.root)
        .args(["put", link.to_str().unwrap()])
        .env("RTRASH_MACOS_TRASH", "finder")
        .env("XDG_STATE_HOME", &state)
        .output()
        .unwrap();
    let err = stderr_of(&out);
    assert!(out.status.success(), "{err}");
    assert!(!link.exists(), "{err}");
    assert!(err.contains("could not be written"), "{err}");
    assert!(err.contains("Finder Trash"), "{err}");
    assert!(!err.contains("brings it back"), "{err}");
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

    for args in [
        &["put", "--dry-run", "--json", "a"][..],
        &["put", "--dry-run", "--json", "-v", "a"][..],
    ] {
        let out = sb.run(args);
        assert!(out.status.success(), "{args:?}: {}", stderr_of(&out));
        serde_json::from_slice::<serde_json::Value>(&out.stdout)
            .unwrap_or_else(|e| panic!("{args:?}: {e}: {}", stdout_of(&out)));
        if args.contains(&"-v") {
            assert!(
                stderr_of(&out).contains("would trash"),
                "{args:?}: {}",
                stderr_of(&out)
            );
        }
    }
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
        &["rm", &pin, "-f", "--glob", "*"],
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
    for n in ["mc-a", "mc-b", "mc-c", "other.dat"] {
        sb.touch(n);
        assert!(sb.run(&["put", n]).status.success());
    }
    let all = ["mc-a", "mc-b", "mc-c", "other.dat"];
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
        &["rm", &pin, "--match=glob", "--max-count=2", "mc-*"],
        &[],
        "3 items would be deleted",
    );
    refused(
        &["rm", &pin, "--match=glob", "mc-*"],
        &cap2,
        "more than the cap of 2",
    );
    refused(&["empty", &pin, "--all"], &cap2, "more than the cap of 2");
    refused(
        &["rm", &pin, "mc-a"],
        &[("RTRASH_MAX_PERMANENT", "lots")],
        "RTRASH_MAX_PERMANENT",
    );
    refused(&["rm", &pin, "--max-count=x", "mc-a"], &[], "--max-count");
    // A dry run only looks, so the cap lets it through.
    let out = sb.run_env(&["rm", &pin, "--match=glob", "-n", "mc-*"], &cap2);
    assert!(out.status.success(), "{}", stderr_of(&out));
    // Within the cap the run goes ahead, and --max-count wins over the variable.
    let one = [("RTRASH_MAX_PERMANENT", "1")];
    let out = sb.run_env(&["rm", &pin, "mc-a"], &one);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(names_in_trash(&sb), ["mc-b", "mc-c", "other.dat"]);
    let out = sb.run_env(&["empty", &pin, "--all", "--max-count=3"], &one);
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
    refused(&["rm", &pin, "ar-out/ar-c"], 1);
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
    let out = sb.run_env(&["rm", &pin, "ar-in/ar-b"], &scope);
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

/// Names in `dir`, sorted, for checking what a stopped run left.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// The first Ctrl-C stops a put after the item in hand: that item is in
/// the trash with its whole record, the rest stay where they were, and the
/// run exits 130 as an interrupted command does.
#[test]
fn put_stops_after_the_item_in_hand_at_the_first_ctrl_c() {
    let sb = Sandbox::new("put-ctrl-c");
    for n in ["a.txt", "b.txt", "c.txt"] {
        fs::write(sb.work().join(n), n).unwrap();
    }
    let args = ["put", "a.txt", "b.txt", "c.txt"];
    let out = sb.run_env(&args, &[("RTRASH_TEST_SIGINT", "after-item:1")]);
    let said = stderr_of(&out);
    assert_eq!(out.status.code(), Some(130), "{said}");
    assert!(
        said.contains("stopped by Ctrl-C: 1 trashed, 2 not reached"),
        "{said}"
    );
    assert_eq!(names_in(&sb.work()), ["b.txt", "c.txt"]);
    assert_eq!(names_in(&sb.trash().join("files")), ["a.txt"]);
    let info = fs::read_to_string(sb.trash().join("info/a.txt.trashinfo")).unwrap();
    let path = format!("Path={}\n", sb.work().join("a.txt").display());
    assert!(
        info.starts_with("[Trash Info]\n") && info.contains(&path),
        "{info}"
    );
    assert!(info.contains("DeletionDate="), "{info}");
}

/// A Ctrl-C after put has taken a trash name waits until the record and
/// the payload are both whole. A first press then stops the run; a second
/// one ends the process by SIGINT. Either way the next item stays and no
/// half-written record or lone payload is left.
#[test]
fn ctrl_c_inside_a_record_waits_until_the_pair_is_whole() {
    use std::os::unix::process::ExitStatusExt;
    for (presses, tag) in [(1, "put-ctrl-c-once"), (2, "put-ctrl-c-twice")] {
        let sb = Sandbox::new(tag);
        fs::write(sb.work().join("a.txt"), b"first").unwrap();
        fs::write(sb.work().join("b.txt"), b"second").unwrap();
        let seam = format!("in-record:{presses}");
        let out = sb.run_env(
            &["put", "a.txt", "b.txt"],
            &[("RTRASH_TEST_SIGINT", seam.as_str())],
        );
        let said = stderr_of(&out);
        if presses == 1 {
            assert_eq!(out.status.code(), Some(130), "{said}");
            assert!(said.contains("1 trashed, 1 not reached"), "{said}");
        } else {
            assert_eq!(out.status.signal(), Some(libc::SIGINT), "{said}");
        }
        assert_eq!(names_in(&sb.work()), ["b.txt"], "{presses}");
        assert_eq!(names_in(&sb.trash().join("files")), ["a.txt"], "{presses}");
        assert_eq!(
            names_in(&sb.trash().join("info")),
            ["a.txt.trashinfo"],
            "{presses}"
        );
        let info = fs::read_to_string(sb.trash().join("info/a.txt.trashinfo")).unwrap();
        assert!(
            info.contains("Path=") && info.contains("DeletionDate="),
            "{presses}: {info}"
        );
        assert_eq!(fs::read(sb.trash().join("files/a.txt")).unwrap(), b"first");
    }
}

/// The first Ctrl-C stops a restore after the entry in hand: that entry
/// is back without its record, the others stay in the trash with theirs.
#[test]
fn restore_stops_after_the_entry_in_hand_at_the_first_ctrl_c() {
    let sb = Sandbox::new("restore-ctrl-c");
    for n in ["a.txt", "b.txt", "c.txt"] {
        fs::write(sb.work().join(n), n).unwrap();
    }
    let put = sb.run(&["put", "a.txt", "b.txt", "c.txt"]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let out = sb.run_env(
        &["restore", "--all"],
        &[("RTRASH_TEST_SIGINT", "after-item:1")],
    );
    let said = stderr_of(&out);
    assert_eq!(out.status.code(), Some(130), "{said}");
    assert!(
        said.contains("stopped by Ctrl-C: 1 restored, 2 not reached"),
        "{said}"
    );
    let back = names_in(&sb.work());
    assert_eq!(back.len(), 1, "{back:?}");
    let left = names_in(&sb.trash().join("files"));
    assert_eq!(left.len(), 2, "{left:?}");
    assert!(!left.contains(&back[0]), "{back:?} {left:?}");
    let records: Vec<String> = left.iter().map(|n| format!("{n}.trashinfo")).collect();
    assert_eq!(names_in(&sb.trash().join("info")), records);
}

/// A first Ctrl-C during `rm` stops after the entry in hand: its payload
/// and its record go together, and every entry not reached keeps both.
#[test]
fn rm_stops_after_the_entry_in_hand_at_the_first_ctrl_c() {
    let sb = Sandbox::new("rm-ctrl-c");
    for n in ["a.txt", "b.txt", "c.txt", "notes.dat"] {
        fs::write(sb.work().join(n), n).unwrap();
    }
    let put = sb.run(&["put", "a.txt", "b.txt", "c.txt", "notes.dat"]);
    assert!(put.status.success(), "{}", stderr_of(&put));
    let out = sb.run_env(
        &["rm", "--match=glob", "*.txt"],
        &[("RTRASH_TEST_SIGINT", "after-item:1")],
    );
    let said = stderr_of(&out);
    assert_eq!(out.status.code(), Some(130), "{said}");
    assert!(
        said.contains("stopped by Ctrl-C: 1 removed, 2 not reached"),
        "{said}"
    );
    let left = names_in(&sb.trash().join("files"));
    assert_eq!(left.len(), 3, "{left:?}");
    let records: Vec<String> = left.iter().map(|n| format!("{n}.trashinfo")).collect();
    assert_eq!(names_in(&sb.trash().join("info")), records);
}

/// A first Ctrl-C during `status --check --fix` stops after the record in
/// hand and says how many it did not reach.
#[test]
fn status_fix_stops_after_the_record_in_hand_at_the_first_ctrl_c() {
    let sb = Sandbox::new("status-fix-ctrl-c");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("sf-a");
    assert!(sb.run(&["put", "sf-a"]).status.success());
    let work = sb.work().display().to_string();
    let info = sb.trash().join("info");
    for name in ["sf-x", "sf-y", "sf-z"] {
        fs::write(
            info.join(format!("{name}.trashinfo")),
            format!("[Trash Info]\nPath={work}/{name}\nDeletionDate=2026-01-02T03:04:05\n"),
        )
        .unwrap();
    }
    let out = sb.run_env(
        &["status", pin.as_str(), "--check", "--fix"],
        &[("RTRASH_TEST_SIGINT", "after-item:1")],
    );
    let said = stderr_of(&out);
    assert_eq!(out.status.code(), Some(130), "{said}");
    assert!(
        said.contains("stopped by Ctrl-C: 1 record removed, 2 not reached"),
        "{said}"
    );
    assert_eq!(lines_starting(&out, "removed "), 1, "{}", stdout_of(&out));
    let records = names_in(&info);
    assert_eq!(records.len(), 3, "{records:?}");
    assert!(
        records.contains(&"sf-a.trashinfo".to_string()),
        "{records:?}"
    );
}

/// Trash `names` from the work dir in one put.
fn put_names(sb: &Sandbox, names: &[String]) {
    for n in names {
        fs::write(sb.work().join(n), n).unwrap();
    }
    let mut args = vec!["put"];
    args.extend(names.iter().map(String::as_str));
    let out = sb.run(&args);
    assert!(out.status.success(), "{}", stderr_of(&out));
}

/// Each of `names` has both its payload and its record in the trash, or
/// neither. Returns how many are gone.
fn pairs_whole(sb: &Sandbox, names: &[String]) -> usize {
    let (files, info) = (sb.trash().join("files"), sb.trash().join("info"));
    let mut gone = 0;
    for n in names {
        let payload = files.join(n).exists();
        let record = info.join(format!("{n}.trashinfo")).exists();
        assert_eq!(payload, record, "{n}: payload {payload}, record {record}");
        gone += usize::from(!payload);
    }
    gone
}

/// A first Ctrl-C during a full `empty` stops the wipe before the next
/// payload. The payloads not reached keep their records, and no record
/// is left whose payload is gone.
#[test]
fn empty_stops_at_the_first_ctrl_c_and_leaves_no_half_pair() {
    let sb = Sandbox::new("empty-ctrl-c");
    let names: Vec<String> = (0..40).map(|i| format!("w{i}.txt")).collect();
    put_names(&sb, &names);
    let pin = format!("--trash-dir={}", sb.trash().display());
    let out = sb.run_env(
        &["empty", pin.as_str()],
        &[("RTRASH_TEST_SIGINT", "after-item:1")],
    );
    let said = stderr_of(&out);
    assert_eq!(out.status.code(), Some(130), "{said}");
    assert!(said.contains("stopped by Ctrl-C: "), "{said}");
    assert!(said.contains("the rest stay in the trash"), "{said}");
    assert!(pairs_whole(&sb, &names) >= 1, "{said}");
}

/// The same for `empty DAYS`, which removes item by item in parallel.
#[test]
fn empty_days_stops_at_the_first_ctrl_c_and_leaves_no_half_pair() {
    let sb = Sandbox::new("empty-days-ctrl-c");
    let names: Vec<String> = (0..40).map(|i| format!("d{i}.txt")).collect();
    put_names(&sb, &names);
    for n in &names {
        let at = sb.trash().join(format!("info/{n}.trashinfo"));
        let body = fs::read_to_string(&at).unwrap();
        let old: Vec<String> = body
            .lines()
            .map(|l| {
                if l.starts_with("DeletionDate=") {
                    "DeletionDate=2001-01-01T00:00:00".to_string()
                } else {
                    l.to_string()
                }
            })
            .collect();
        fs::write(&at, old.join("\n")).unwrap();
    }
    let pin = format!("--trash-dir={}", sb.trash().display());
    let out = sb.run_env(
        &["empty", pin.as_str(), "5"],
        &[("RTRASH_TEST_SIGINT", "after-item:1")],
    );
    let said = stderr_of(&out);
    assert_eq!(out.status.code(), Some(130), "{said}");
    assert!(said.contains("stopped by Ctrl-C: "), "{said}");
    assert!(pairs_whole(&sb, &names) >= 1, "{said}");
}

/// The records a put killed between its record and its move leaves
/// (trash-cli #414): an empty one, one cut off before `Path=`, and a
/// whole one whose payload never came. `status --check` names all three
/// and `--fix` removes them, and nothing else.
#[test]
fn status_fix_clears_the_records_a_killed_put_leaves() {
    let sb = Sandbox::new("crash-records");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("cr-good");
    assert!(sb.run(&["put", "cr-good"]).status.success());
    let info = sb.trash().join("info");
    let work = sb.work().display().to_string();
    fs::write(info.join("cr-empty.trashinfo"), b"").unwrap();
    fs::write(info.join("cr-cut.trashinfo"), b"[Trash Info]\n").unwrap();
    fs::write(
        info.join("cr-gone.trashinfo"),
        format!("[Trash Info]\nPath={work}/cr-gone\nDeletionDate=2026-01-02T03:04:05\n"),
    )
    .unwrap();

    let check = sb.run(&["status", pin.as_str(), "--check"]);
    assert_eq!(check.status.code(), Some(1), "{}", stderr_of(&check));
    assert_eq!(
        lines_starting(&check, "dangling: "),
        1,
        "{}",
        stdout_of(&check)
    );
    assert_eq!(
        lines_starting(&check, "bad record: "),
        2,
        "{}",
        stdout_of(&check)
    );

    let fix = sb.run(&["status", pin.as_str(), "--check", "--fix"]);
    assert_eq!(fix.status.code(), Some(0), "{}", stderr_of(&fix));
    assert_eq!(lines_starting(&fix, "removed "), 3, "{}", stdout_of(&fix));
    assert_eq!(names_in(&info), ["cr-good.trashinfo"]);
    assert_eq!(names_in_trash(&sb), ["cr-good"]);
    let again = sb.run(&["status", pin.as_str(), "--check"]);
    assert_eq!(again.status.code(), Some(0), "{}", stderr_of(&again));
}

/// A sandbox with `a.o` in its trash, and the pin to that trash.
fn trashed_a_o(tag: &str) -> (Sandbox, String) {
    let sb = Sandbox::new(tag);
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("a.o");
    assert!(sb.run(&["put", "a.o"]).status.success());
    (sb, pin)
}

#[test]
fn scripts_get_no_rm_question() {
    let (sb, pin) = trashed_a_o("rm-no-tty");
    // A script's stdin is a pipe: it is never asked, even with a no
    // waiting in it.
    let out = sb.run_with_stdin(&["rm", &pin, "a.o"], "n\n");
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!sb.trash().join("files/a.o").exists());
    assert!(!stderr_of(&out).contains("(y/N)"), "{}", stderr_of(&out));
}

#[test]
fn rm_interactive_asks_and_only_a_yes_deletes() {
    let (sb, pin) = trashed_a_o("rm-ask");
    let no = sb.run_with_stdin(&["rm", &pin, "-i", "a.o"], "n\n");
    assert_eq!(no.status.code(), Some(0));
    let err = stderr_of(&no);
    assert!(err.contains("Permanently remove 1 item ("), "{err}");
    assert!(err.contains("nothing removed"), "{err}");
    assert!(sb.trash().join("files/a.o").is_file());
    // End of input is a no as well.
    let eof = sb.run_with_stdin(&["rm", &pin, "--interactive", "a.o"], "");
    assert_eq!(eof.status.code(), Some(0));
    assert!(sb.trash().join("files/a.o").is_file());
    let yes = sb.run_with_stdin(&["rm", &pin, "-i", "a.o"], "yes\n");
    assert!(yes.status.success(), "{}", stderr_of(&yes));
    assert!(!sb.trash().join("files/a.o").exists());
}

#[test]
fn the_last_of_rm_i_y_and_f_wins() {
    for (tag, flag) in [
        ("rm-ask-y", "-y"),
        ("rm-ask-yes", "--yes"),
        ("rm-ask-f", "-f"),
    ] {
        let (sb, pin) = trashed_a_o(tag);
        let out = sb.run_with_stdin(&["rm", &pin, "-i", flag, "a.o"], "n\n");
        assert!(out.status.success(), "{flag}: {}", stderr_of(&out));
        assert!(!sb.trash().join("files/a.o").exists(), "{flag}");
    }
    // -i after -y asks again.
    let (sb, pin) = trashed_a_o("rm-ask-last");
    let out = sb.run_with_stdin(&["rm", &pin, "-y", "-i", "a.o"], "n\n");
    assert!(
        stderr_of(&out).contains("nothing removed"),
        "{}",
        stderr_of(&out)
    );
    assert!(sb.trash().join("files/a.o").is_file());
}

/// On a terminal rm names the match and asks without -i; a no keeps it.
#[cfg(target_os = "linux")]
#[test]
fn a_terminal_gets_the_rm_question_and_a_no_keeps_it() {
    use std::io::Read;
    use std::os::fd::FromRawFd;

    let (sb, pin) = trashed_a_o("rm-tty");
    let (mut m, mut s) = (0, 0);
    // SAFETY: openpty writes the two fds; no name, termios or size.
    let rc = unsafe {
        libc::openpty(
            &mut m,
            &mut s,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
    // SAFETY: both fds are new and owned here alone.
    let (mut master, slave) = unsafe { (fs::File::from_raw_fd(m), fs::File::from_raw_fd(s)) };
    let mut child = common::rtrash(&sb.root)
        .args(["rm", &pin, "a.o"])
        .current_dir(sb.work())
        .stdin(slave.try_clone().unwrap())
        .stderr(slave.try_clone().unwrap())
        .stdout(Stdio::null())
        .spawn()
        .expect("failed to spawn rtrash");
    drop(slave);
    // The line waits in the terminal until rm reads its reply.
    master.write_all(b"n\n").unwrap();
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > std::time::Duration::from_secs(30) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("rtrash rm still waiting after 30 s");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    // With the child gone, the master reads what it wrote, then EIO.
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok(n) = master.read(&mut buf) {
        if n == 0 {
            break;
        }
        seen.extend_from_slice(&buf[..n]);
    }
    let seen = String::from_utf8_lossy(&seen);
    assert!(status.success(), "{seen}");
    assert!(seen.contains("Permanently remove 1 item ("), "{seen}");
    assert!(sb.trash().join("files/a.o").is_file());
}

/// rm asks, then checks each match again under the trash lock: an item
/// that left the trash while it asked, and whose name a new put took,
/// stays. The question on stderr is the barrier between the two runs
/// (round 244).
#[test]
fn rm_keeps_an_item_that_took_the_name_while_it_asked() {
    let (sb, pin) = trashed_a_o("rm-retaken");
    let mut cmd = common::rtrash(&sb.root);
    cmd.args(["rm", &pin, "-i", "a.o"]).current_dir(sb.work());
    let mut rm = common::Asking::spawn(&mut cmd);
    // rm has read the trash once it asks.
    rm.wait_for("(y/N) ");
    // Meanwhile another run takes the item out, and a put gives its name
    // to another item.
    assert!(sb.run(&["rm", &pin, "-y", "a.o"]).status.success());
    fs::write(sb.work().join("a.o"), b"second").unwrap();
    assert!(sb.run(&["put", "a.o"]).status.success());
    assert_eq!(fs::read(sb.trash().join("files/a.o")).unwrap(), b"second");
    let (status, rest) = rm.answer("y\n");
    assert!(!status.success(), "{rest}");
    assert!(rest.contains("changed in the trash"), "{rest}");
    assert_eq!(fs::read(sb.trash().join("files/a.o")).unwrap(), b"second");
    assert!(sb.trash().join("info/a.o.trashinfo").is_file());
}

/// The record can stay as list read it while another file takes the
/// payload's name. rm asks, the payload is swapped, and the answer is yes:
/// the swapped-in file stays, with its record.
#[test]
fn rm_keeps_a_payload_swapped_while_it_asked() {
    let (sb, pin) = trashed_a_o("rm-payload-swapped");
    let payload = sb.trash().join("files/a.o");
    let mut cmd = common::rtrash(&sb.root);
    cmd.args(["rm", &pin, "-i", "a.o"]).current_dir(sb.work());
    let mut rm = common::Asking::spawn(&mut cmd);
    rm.wait_for("(y/N) ");
    fs::remove_file(&payload).unwrap();
    fs::write(&payload, b"replacement").unwrap();
    let (status, rest) = rm.answer("y\n");
    assert!(!status.success(), "{rest}");
    assert!(rest.contains("changed in the trash"), "{rest}");
    assert_eq!(fs::read(&payload).unwrap(), b"replacement");
    assert!(sb.trash().join("info/a.o.trashinfo").is_file());
}

/// A payload swapped after rm's last read, with the record unchanged, stays.
/// The debug-only seam swaps it where the git scan has just ended.
#[test]
fn rm_rechecks_the_payload_after_the_scan() {
    let (sb, pin) = trashed_a_o("rm-payload-after-scan");
    let payload = sb.trash().join("files/a.o");
    let out = sb.run_env(
        &["rm", &pin, "a.o"],
        &[("RTRASH_TEST_REPLACE_PAYLOAD", payload.to_str().unwrap())],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("changed in the trash"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(fs::read(&payload).unwrap(), b"replacement");
    assert!(sb.trash().join("info/a.o.trashinfo").is_file());
}

/// restore moves the file list read. A payload swapped in under the same
/// name, with the record unchanged, is left in the trash and nothing is
/// restored.
#[test]
fn restore_keeps_a_payload_swapped_before_it_moves() {
    let (sb, pin) = trashed_a_o("restore-payload-swapped");
    let payload = sb.trash().join("files/a.o");
    let out = sb.run_env(
        &["restore", &pin, sb.work().join("a.o").to_str().unwrap()],
        &[("RTRASH_TEST_REPLACE_PAYLOAD", payload.to_str().unwrap())],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("changed in the trash"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(fs::read(&payload).unwrap(), b"replacement");
    assert!(sb.trash().join("info/a.o.trashinfo").is_file());
    assert!(!sb.work().join("a.o").exists());
}

/// An id match is tied to the record that list read. Rewriting that record
/// under the same trash name while rm asks makes the locked re-read refuse it.
#[test]
fn rm_by_id_refuses_a_record_rewritten_before_relock() {
    let (sb, pin) = trashed_a_o("rm-id-rewritten");
    let listed = stdout_of(&sb.run(&["list", &pin, "--format={id}\\n"]));
    let id = listed.lines().next().unwrap_or_default().to_string();
    assert!(!id.is_empty(), "{listed}");
    let mut cmd = common::rtrash(&sb.root);
    cmd.args(["rm", &pin, "-i", "--match=id", &id])
        .current_dir(sb.work());
    let mut rm = common::Asking::spawn(&mut cmd);
    rm.wait_for("(y/N) ");

    let record = sb.trash().join("info/a.o.trashinfo");
    let before = fs::read_to_string(&record).unwrap();
    let after = before.replace(
        "DeletionDate=",
        "DeletionDate=2001-01-01T00:00:00\nX-Rewritten-Date=",
    );
    fs::write(&record, after).unwrap();
    let (status, rest) = rm.answer("y\n");
    assert!(!status.success(), "{rest}");
    assert!(rest.contains("changed in the trash"), "{rest}");
    assert!(sb.trash().join("files/a.o").is_file());
    assert!(record.is_file());
}

/// A trash pinned twice, however spelled, is one trash: a prune locks it
/// once and counts its items once. Before, the second lock of the same
/// root waited forever on the first (round 245).
#[test]
fn a_trash_pinned_twice_is_emptied_once() {
    let sb = Sandbox::new("pin-twice");
    sb.touch("a.o");
    assert!(sb.run(&["put", "a.o"]).status.success());
    let one = format!("--trash-dir={}", sb.trash().display());
    let two = format!("--trash-dir={}", sb.trash().join("files/..").display());
    let out = sb.run(&["empty", &one, &two, "--max-size=0", "--json"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let json = stdout_of(&out);
    assert!(json.contains("\"removed\":1,"), "{json}");
    assert!(!sb.trash().join("files/a.o").exists());
}

/// What `empty -i` deletes is what it counted when it asked: an item put
/// while it waits for the yes stays, with its record, and the run says
/// so. The question is the barrier between the two runs (round 245).
#[test]
fn empty_i_leaves_an_item_put_after_the_question() {
    let sb = Sandbox::new("empty-asked");
    sb.touch("a.o");
    assert!(sb.run(&["put", "a.o"]).status.success());
    let pin = format!("--trash-dir={}", sb.trash().display());
    let mut cmd = common::rtrash(&sb.root);
    cmd.args(["empty", "-i", &pin]).current_dir(sb.work());
    let mut empty = common::Asking::spawn(&mut cmd);
    // empty has counted the trash once it asks.
    empty.wait_for("Proceed? (y/N) ");
    sb.touch("b.o");
    assert!(sb.run(&["put", "b.o"]).status.success());
    let (status, err) = empty.answer("y\n");
    assert!(status.success(), "{err}");
    assert!(err.contains(", 1 item)"), "{err}");
    assert!(err.contains("left 1 item that came into"), "{err}");
    assert!(!sb.trash().join("files/a.o").exists());
    assert!(sb.trash().join("files/b.o").is_file());
    assert!(sb.trash().join("info/b.o.trashinfo").is_file());
}

/// Every prompt reads its answer through answer::ask, so a yes is `y` or
/// `yes` in any case, and anything else is a no, from a pipe as from a
/// terminal (trash-cli #333). put -i used to take any answer that starts
/// with y.
#[test]
fn prompts_take_only_y_or_yes_even_from_a_pipe() {
    let sb = Sandbox::new("answers");
    sb.touch("a.o");
    for no in ["yolo\n", "y please\n", "\n", ""] {
        sb.run_with_stdin(&["put", "-i", "a.o"], no);
        assert!(sb.work().join("a.o").exists(), "{no:?}");
    }
    let out = sb.run_with_stdin(&["put", "-i", "a.o"], " YES \n");
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!sb.work().join("a.o").exists());
    let pin = format!("--trash-dir={}", sb.trash().display());
    for no in ["n\n", "yolo\n", ""] {
        let out = sb.run_with_stdin(&["empty", "-i", &pin], no);
        assert!(stderr_of(&out).contains("Proceed? (y/N) "), "{no:?}");
        assert!(sb.trash().join("files/a.o").exists(), "{no:?}");
    }
    let out = sb.run_with_stdin(&["empty", "-i", &pin], "Y\n");
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!sb.trash().join("files/a.o").exists());
}
