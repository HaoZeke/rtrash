//! Snapshot the real catalog directories at process start and fail the
//! test binary at exit if they changed. Integration tests link the library
//! without `cfg(test)`, so the cache is on; a `list` in the test process
//! otherwise writes `~/.cache/rtrash`. The key directory stays untouched.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};

struct Shot {
    paths: Vec<PathBuf>,
    text: String,
}

static SHOT: Mutex<Option<Shot>> = Mutex::new(None);
static START: Once = Once::new();

/// The real cache and key directories this process would use before a
/// test points `HOME` or the XDG variables at a temp directory.
pub fn real_catalog_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        if !home.is_empty() {
            let home = PathBuf::from(home);
            dirs.push(home.join(".cache/rtrash"));
            dirs.push(home.join(".local/share/rtrash"));
        }
    }
    for var in ["XDG_CACHE_HOME", "XDG_DATA_HOME"] {
        if let Some(v) = std::env::var_os(var) {
            if !v.is_empty() {
                dirs.push(PathBuf::from(v).join("rtrash"));
            }
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

/// One directory's files, so a new, removed or rewritten catalog shows up.
pub fn catalog_dir_listing(path: &Path) -> String {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return String::from("missing\n"),
        Err(e) => return format!("unreadable {e}\n"),
    };
    let mut rows = Vec::new();
    let rd = match fs::read_dir(path) {
        Ok(rd) => rd,
        Err(e) => return format!("unreadable {e}\n"),
    };
    let dir_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        rows.push(match fs::symlink_metadata(ent.path()) {
            Ok(m) => {
                let ns = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                let kind = if m.file_type().is_symlink() {
                    "link"
                } else if m.is_dir() {
                    "dir"
                } else {
                    "file"
                };
                format!(
                    "{name} {kind} {} {ns} {:016x}",
                    m.len(),
                    file_hash(&ent.path())
                )
            }
            Err(e) => format!("{name} err {e}"),
        });
    }
    rows.sort();
    let mut out = format!(
        "dir {} {dir_ns} {:o}\ncount={}\n",
        meta.len(),
        meta.permissions().mode(),
        rows.len()
    );
    for row in rows {
        out.push_str(&row);
        out.push('\n');
    }
    out
}

fn file_hash(path: &Path) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let Ok(bytes) = fs::read(path) else {
        return 0;
    };
    if bytes.len() > (1 << 20) {
        return bytes.len() as u64;
    }
    for b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn listing(paths: &[PathBuf]) -> String {
    let mut out = String::new();
    for path in paths {
        out.push_str(&path.to_string_lossy());
        out.push('\n');
        out.push_str(&catalog_dir_listing(path));
    }
    out
}

/// Remember the real directories. A later call is a no-op, so a test that
/// has already moved `HOME` does not move the snapshot with it.
pub fn install() {
    START.call_once(|| {
        let paths = real_catalog_dirs();
        let text = listing(&paths);
        *SHOT.lock().unwrap_or_else(|e| e.into_inner()) = Some(Shot { paths, text });
        // SAFETY: `guard_check` is a plain C function. It runs while the
        // runtime is still up, because it is registered after startup.
        unsafe {
            libc::atexit(guard_check);
        }
    });
}

extern "C" fn guard_init() {
    install();
}

extern "C" fn guard_check() {
    let (before, paths) = {
        let g = SHOT.lock().unwrap_or_else(|e| e.into_inner());
        match g.as_ref() {
            Some(s) => (s.text.clone(), s.paths.clone()),
            None => return,
        }
    };
    let now = listing(&paths);
    if now != before {
        eprintln!("the tests changed the real catalog directories:\n{before}---\n{now}");
        // SAFETY: the suite wrote the real cache or key. Exit non-zero
        // even when every test already reported success.
        unsafe { libc::_exit(1) }
    }
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "netbsd"
))]
#[used]
#[link_section = ".init_array"]
static GUARD_INIT: extern "C" fn() = guard_init;

#[cfg(target_os = "macos")]
#[used]
#[link_section = "__DATA,__mod_init_func"]
static GUARD_INIT: extern "C" fn() = guard_init;
