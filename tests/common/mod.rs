//! Scaffolding shared by the integration test binaries.
//!
//! Every test that needs a scratch directory takes it from `scratch_dir`.
//! Tests in one binary run in parallel with the same pid, so a path built
//! from the pid and a hand-picked tag lets two tests share, and delete,
//! one tree (round 149 reused the du tag "types"). The counter here makes
//! each call unique whatever tag it gets.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

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
