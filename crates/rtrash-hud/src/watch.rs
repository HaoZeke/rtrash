//! Debounced watch of trash `files/` and `info/` so the HUD reloads.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Handle that keeps the watcher thread alive.
pub struct Watch {
    flag: Arc<AtomicBool>,
    #[allow(dead_code)]
    join: Option<thread::JoinHandle<()>>,
}

impl Watch {
    /// True if `files/` or `info/` changed since the last take.
    pub fn take(&self) -> bool {
        self.flag.swap(false, Ordering::SeqCst)
    }
}

/// Watch each trash root's `files/` and `info/`. Polls mtimes (no extra crate).
pub fn install(roots: &[PathBuf]) -> Watch {
    let flag = Arc::new(AtomicBool::new(false));
    let watched = roots.to_vec();
    let flag_t = Arc::clone(&flag);
    let join = thread::Builder::new()
        .name("rtrash-hud-watch".into())
        .spawn(move || watch_loop(watched, flag_t))
        .ok();
    Watch { flag, join }
}

fn watch_loop(roots: Vec<PathBuf>, flag: Arc<AtomicBool>) {
    let mut last = snapshot(&roots);
    loop {
        thread::sleep(Duration::from_millis(400));
        let now = snapshot(&roots);
        if now != last {
            last = now;
            flag.store(true, Ordering::SeqCst);
        }
    }
}

fn snapshot(roots: &[PathBuf]) -> Vec<(u64, i64, i64)> {
    roots
        .iter()
        .map(|root| {
            let info = stamp(&root.join("info"));
            let files = stamp(&root.join("files"));
            (hash_path(root), info, files)
        })
        .collect()
}

fn stamp(path: &std::path::Path) -> i64 {
    std::fs::metadata(path)
        .ok()
        .map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.mtime().saturating_mul(1_000_000_000) + m.mtime_nsec()
        })
        .unwrap_or(0)
}

fn hash_path(p: &std::path::Path) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    p.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn take_sees_info_dir_change() {
        let root = std::env::temp_dir().join(format!(
            "rtrash-hud-watch-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("info")).unwrap();
        fs::create_dir_all(root.join("files")).unwrap();
        let w = install(&[root.clone()]);
        thread::sleep(Duration::from_millis(50));
        let _ = w.take();
        fs::write(root.join("info/x.trashinfo"), b"[Trash Info]\n").unwrap();
        let start = std::time::Instant::now();
        let mut saw = false;
        while start.elapsed() < Duration::from_secs(2) {
            if w.take() {
                saw = true;
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = fs::remove_dir_all(&root);
        assert!(saw, "watch must notice a new trashinfo");
    }
}
