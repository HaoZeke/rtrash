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
            "fish",
            "cargo binstall",
        ],
        "README.md",
    );
}

#[test]
fn readme_documents_release_or_binary_path() {
    let s = read("README.md");
    assert!(
        s.contains("cargo binstall"),
        "README must document cargo binstall"
    );
    assert!(
        s.contains("scripts/package-release") || s.contains("package-release.sh"),
        "README must mention package-release for tarball builds"
    );
    assert!(
        s.contains("metadata.binstall") || s.contains("package.metadata.binstall"),
        "README should point at binstall metadata alignment"
    );
}

#[test]
fn package_release_script_stages_fish_multicall() {
    let s = read("scripts/package-release.sh");
    assert!(
        s.contains("rtrash.fish"),
        "package-release must stage fish main completion"
    );
    // Must link multi-call names so fish autoloads them (not only rtrash.fish).
    for name in [
        "trash-put.fish",
        "trash-empty.fish",
        "trash-list.fish",
        "trash-restore.fish",
        "trash-rm.fish",
    ] {
        assert!(
            s.contains(name) || (s.contains("trash-put") && s.contains(".fish") && s.contains("ln -sf rtrash.fish")),
            "package-release must stage fish multi-call file for {name}"
        );
    }
    assert!(
        s.contains("ln -sf rtrash.fish"),
        "package-release must symlink multi-call fish completions to rtrash.fish"
    );
}
