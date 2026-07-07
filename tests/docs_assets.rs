//! Structural checks for shell completions and man pages.
//! These parse checked-in static assets (no interactive shell, no cargo build of man).

use std::fs;
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    let p = root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn assert_contains(hay: &str, needles: &[&str], label: &str) {
    let mut missing = Vec::new();
    for n in needles {
        if !hay.contains(n) {
            missing.push(*n);
        }
    }
    assert!(
        missing.is_empty(),
        "{label}: missing tokens: {missing:?}"
    );
}

#[test]
fn fish_completion_covers_surface() {
    let s = read("completions/rtrash.fish");
    assert_contains(
        &s,
        &[
            "put",
            "empty",
            "list",
            "status",
            "restore",
            "rm",
            "setup",
            "completions",
            "home-only",
            "trash-dir",
            "dry-run",
            "trash-put",
            "trash-empty",
            "complete -c rtrash",
        ],
        "completions/rtrash.fish",
    );
    // Must not register system `rm` by default (would shadow GNU rm completion).
    assert!(
        !s.contains("complete -c rm "),
        "fish must not complete multi-call rm by default"
    );
}

#[test]
fn bash_completion_covers_surface() {
    let s = read("completions/rtrash.bash");
    assert_contains(
        &s,
        &[
            "put",
            "empty",
            "list",
            "status",
            "restore",
            "rm",
            "--home-only",
            "--trash-dir=",
            "--dry-run",
            "trash-put",
            "trash-empty",
            "trash-list",
            "trash-restore",
            "trash-rm",
            "complete -F _rtrash_main rtrash",
            "--recursive",
            "--force",
            "--verbose",
            "setup",
            "completions",
        ],
        "completions/rtrash.bash",
    );
}

#[test]
fn zsh_completion_covers_surface() {
    let s = read("completions/_rtrash");
    assert_contains(
        &s,
        &[
            "put",
            "empty",
            "list",
            "status",
            "restore",
            "rm",
            "--home-only",
            "--trash-dir=",
            "--dry-run",
            "trash-put",
            "trash-empty",
            "trash-list",
            "trash-restore",
            "trash-rm",
            "--recursive",
            "--force",
            "--verbose",
            "#compdef",
        ],
        "completions/_rtrash",
    );
}

#[test]
fn man_page_covers_surface() {
    let s = read("man/rtrash.1");
    assert_contains(
        &s,
        &[
            ".TH RTRASH 1",
            "put",
            "empty",
            "list",
            "status",
            "restore",
            "trash-put",
            "trash-empty",
            "trash-list",
            "trash-restore",
            "trash-rm",
            "\\-\\-home\\-only",
            "\\-\\-trash\\-dir",
            "\\-\\-dry\\-run",
            "\\-\\-force",
            "\\-\\-recursive",
            "\\-\\-verbose",
            "XDG_DATA_HOME",
        ],
        "man/rtrash.1",
    );
}

#[test]
fn readme_documents_setup_story() {
    let s = read("README.md");
    assert_contains(
        &s,
        &[
            "rtrash setup",
            "completions bash",
            "bash-completion",
            "site-functions",
            "man/rtrash.1",
            "embedded",
            "fish",
        ],
        "README.md",
    );
}

#[test]
fn readme_documents_release_or_binary_path() {
    let s = read("README.md");
    // At least one concrete path beyond bare ad-hoc lore: binstall and/or release script.
    let has_binstall = s.contains("cargo binstall") || s.contains("binstall");
    let has_release = s.contains("release")
        && (s.contains("musl") || s.contains("scripts/package-release") || s.contains("GitHub Release"));
    assert!(
        has_binstall || has_release,
        "README must document a binary/release install path (binstall and/or release recipe)"
    );
}
