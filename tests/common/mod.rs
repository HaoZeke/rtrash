//! Scaffolding shared by the integration test binaries.
//!
//! Every test that needs a scratch directory takes it from `scratch_dir`.
//! Tests in one binary run in parallel with the same pid, so a path built
//! from the pid and a hand-picked tag lets two tests share, and delete,
//! one tree (round 149 reused the du tag "types"). The counter here makes
//! each call unique whatever tag it gets.
//!
//! Every test that needs two trash puts to get different deletion times
//! calls `next_trash_second` between them, in place of a fixed sleep.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A fresh, empty directory under the system temp dir, named
/// `rtrash-{prefix}-{pid}-{n}-{tag}`, unique to this call.
pub fn scratch_dir(prefix: &str, tag: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let name = format!("rtrash-{prefix}-{}-{n}-{tag}", std::process::id());
    let root = std::env::temp_dir().join(name);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// Whole seconds since the Unix epoch on the system clock.
pub fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Wait until the system clock reaches a later whole second, so a trash
/// put after this call gets a later deletion time than any put before it.
///
/// The Recycle Bin, the Finder Trash and a trashinfo `DeletionDate` all
/// keep whole seconds. Two puts in one second tie, and the order then
/// falls back to the name, which a test that sorts by date does not
/// expect. Linux test runs rarely hit the tie; slower Windows and macOS
/// runners do (commit 9708b71).
pub fn next_trash_second() {
    let start = epoch_secs();
    while epoch_secs() <= start {
        std::thread::sleep(Duration::from_millis(20));
    }
}
