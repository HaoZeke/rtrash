//! Structural checks: CI and lockfile policy match the documented quality bar.
//! These read shipped files in the workspace (not a reimplementation of cargo).

use std::fs;
use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn cargo_lock_is_tracked_for_binary_crate() {
    let lock = workspace_root().join("Cargo.lock");
    assert!(
        lock.is_file(),
        "Cargo.lock must be committed for the rtrash binary (idiomatic app lockfile)"
    );
    let body = fs::read_to_string(&lock).unwrap();
    assert!(
        body.contains("name = \"rtrash\""),
        "Cargo.lock must resolve the rtrash package"
    );
}

#[test]
fn ci_workflow_runs_fmt_clippy_locked_tests() {
    let path = workspace_root().join(".github/workflows/ci.yml");
    let yml = fs::read_to_string(&path).expect("ci.yml must exist");
    for needle in [
        "cargo fmt --check",
        "cargo clippy",
        "-D warnings",
        "nextest",
        "pull_request",
    ] {
        assert!(
            yml.contains(needle),
            "ci.yml must include {needle:?}\n---\n{yml}"
        );
    }
    // push to main is the other trigger
    assert!(
        yml.contains("branches: [main]") || yml.contains("branches: [\"main\"]"),
        "ci.yml must target main"
    );
}

#[test]
fn deny_toml_and_ci_deny_job_exist() {
    let deny = workspace_root().join("deny.toml");
    assert!(deny.is_file(), "deny.toml supply-chain policy required");
    let deny_body = fs::read_to_string(&deny).unwrap();
    assert!(
        deny_body.contains("[licenses]") && deny_body.contains("[advisories]"),
        "deny.toml must configure licenses and advisories"
    );
    let yml = fs::read_to_string(workspace_root().join(".github/workflows/ci.yml")).unwrap();
    assert!(
        yml.contains("cargo-deny") || yml.contains("cargo deny"),
        "ci.yml must invoke cargo-deny"
    );
}

#[test]
fn contributing_documents_quality_bar() {
    let body = fs::read_to_string(workspace_root().join("CONTRIBUTING.md")).unwrap();
    for needle in [
        "cargo fmt --check",
        "cargo clippy",
        "nextest",
        "Cargo.lock",
        "deny",
    ] {
        assert!(
            body.contains(needle),
            "CONTRIBUTING.md must document {needle:?}"
        );
    }
}

#[test]
fn cargo_dist_and_asv_layout_exist() {
    let root = workspace_root();
    assert!(
        root.join("dist-workspace.toml").is_file(),
        "dist-workspace.toml (cargo-dist) required"
    );
    let dist = fs::read_to_string(root.join("dist-workspace.toml")).unwrap();
    assert!(
        dist.contains("unknown-linux-musl"),
        "cargo-dist targets should include Linux musl for FreeDesktop binaries"
    );
    assert!(root.join("asv.conf.json").is_file());
    assert!(root.join("benchmarks/trash_ops.py").is_file());
    let bench_ci = fs::read_to_string(root.join(".github/workflows/ci_benchmark.yml")).unwrap();
    assert!(bench_ci.contains("asv run") || bench_ci.contains("asv-spyglass"));
    let comment = fs::read_to_string(root.join(".github/workflows/ci_bench_commenter.yml")).unwrap();
    assert!(comment.contains("asv-perch"));
    assert!(root.join(".config/nextest.toml").is_file());
}
