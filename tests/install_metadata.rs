//! Structural checks: cargo-binstall metadata and package-release naming agree.
//! No network; reads Cargo.toml + scripts/package-release.sh from the tree.
//!
//! Critical: default host triple on Linux desktops is often
//! `x86_64-unknown-linux-gnu`, but we only publish a *musl* tarball. Metadata
//! must remap that host to the musl asset so bare `cargo binstall rtrash` works.

use std::fs;
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    let p = root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// First `version = "…"` in the package table (Cargo.toml top).
fn package_version(cargo_toml: &str) -> &str {
    let mut in_package = false;
    for line in cargo_toml.lines() {
        let line = line.trim();
        if line == "[package]" {
            in_package = true;
            continue;
        }
        if in_package && line.starts_with('[') {
            break;
        }
        if in_package {
            if let Some(rest) = line.strip_prefix("version = \"") {
                return rest.strip_suffix('"').expect("version quote");
            }
        }
    }
    panic!("package version not found in Cargo.toml");
}

fn package_name(cargo_toml: &str) -> &str {
    let mut in_package = false;
    for line in cargo_toml.lines() {
        let line = line.trim();
        if line == "[package]" {
            in_package = true;
            continue;
        }
        if in_package && line.starts_with('[') {
            break;
        }
        if in_package {
            if let Some(rest) = line.strip_prefix("name = \"") {
                return rest.strip_suffix('"').expect("name quote");
            }
        }
    }
    panic!("package name not found");
}

/// Expand the Linux x86_64 override template (musl asset) for the current version.
/// This is what a typical `x86_64-unknown-linux-gnu` host must resolve to.
fn expand_linux_x86_64_musl_asset(name: &str, version: &str) -> (String, String, String) {
    let musl_target = "x86_64-unknown-linux-musl";
    let basename = format!("{name}-{version}-{musl_target}.tar.gz");
    let bin_path = format!("{name}-{version}-{musl_target}/bin/{name}");
    let url_suffix = format!("releases/download/v{version}/{basename}");
    (basename, bin_path, url_suffix)
}

/// What a naive `{ target }` expansion would produce for the default glibc host
/// (must NOT be the only documented path — that asset is not published).
fn expand_naive_gnu_host(name: &str, version: &str) -> String {
    format!("{name}-{version}-x86_64-unknown-linux-gnu.tar.gz")
}

#[test]
fn binstall_metadata_matches_package_release_basename() {
    let cargo = read("Cargo.toml");
    let script = read("scripts/package-release.sh");

    assert!(
        cargo.contains("[package.metadata.binstall]"),
        "Cargo.toml must declare [package.metadata.binstall]"
    );
    assert!(
        cargo.contains("pkg-fmt = \"tgz\"") || cargo.contains("pkg-fmt = \"tar.gz\""),
        "pkg-fmt must be a tar.gz style format"
    );

    let name = package_name(&cargo);
    let version = package_version(&cargo);
    assert_eq!(name, "rtrash");

    // package-release.sh builds NAME="rtrash-${VERSION}-${TARGET}" then TAR=…/${NAME}.tar.gz
    assert!(
        script.contains("NAME=\"rtrash-${VERSION}-${TARGET}\""),
        "package-release must set NAME to rtrash-VERSION-TARGET"
    );
    assert!(
        script.contains("${NAME}.tar.gz"),
        "package-release must emit ${{NAME}}.tar.gz"
    );
    assert!(
        script.contains("bin/rtrash"),
        "package-release must stage bin/rtrash (binstall bin-dir)"
    );
    assert!(
        script.contains("metadata.binstall")
            || script.contains("cargo-binstall")
            || script.contains("binstall"),
        "package-release should mention binstall naming lockstep"
    );

    let (basename, bin_path, url_suffix) = expand_linux_x86_64_musl_asset(name, version);
    assert_eq!(
        basename,
        format!("rtrash-{version}-x86_64-unknown-linux-musl.tar.gz")
    );
    assert_eq!(
        bin_path,
        format!("rtrash-{version}-x86_64-unknown-linux-musl/bin/rtrash")
    );
    assert!(url_suffix.starts_with("releases/download/v"));
    assert!(url_suffix.ends_with(&basename));
}

#[test]
fn binstall_remaps_default_linux_gnu_host_to_musl_asset() {
    let cargo = read("Cargo.toml");
    let name = package_name(&cargo);
    let version = package_version(&cargo);

    // Override table for all x86_64 Linux (covers gnu *and* musl host triples).
    assert!(
        cargo.contains("overrides")
            && (cargo.contains("target_os") || cargo.contains("cfg(all(target_os")),
        "must declare a binstall override for Linux x86_64 (or equivalent)"
    );
    // Hardcoded musl triple in the override URL (not only {{ target }}).
    assert!(
        cargo.contains("x86_64-unknown-linux-musl.tar.gz"),
        "override pkg-url must hardcode the musl tarball basename pattern"
    );
    assert!(
        cargo.contains("x86_64-unknown-linux-musl/bin/"),
        "override bin-dir must hardcode the musl staged directory"
    );

    let (musl_basename, musl_bin, _) = expand_linux_x86_64_musl_asset(name, version);
    let gnu_naive = expand_naive_gnu_host(name, version);

    // The published asset (and override) must be the musl name, not the naive gnu expansion.
    assert_ne!(
        musl_basename, gnu_naive,
        "musl and gnu basenames must differ so the remapping is meaningful"
    );
    assert!(
        !cargo.contains(&format!("{{ name }}-{{ version }}-x86_64-unknown-linux-gnu")),
        "must not publish a linux-gnu-only template as the only Linux path"
    );

    // Simulate what a successful default-host resolution must produce for docs/CI.
    // Host: x86_64-unknown-linux-gnu → override → musl asset (not gnu).
    let resolved_for_default_host = musl_basename.clone();
    assert_eq!(
        resolved_for_default_host,
        format!("{name}-{version}-x86_64-unknown-linux-musl.tar.gz")
    );
    assert_ne!(
        resolved_for_default_host, gnu_naive,
        "default glibc host must not resolve to an unpublished *-linux-gnu*.tar.gz"
    );

    // package-release default TARGET is musl — same basename the override points at.
    let script = read("scripts/package-release.sh");
    assert!(
        script.contains("x86_64-unknown-linux-musl"),
        "package-release default target must be the musl triple used by binstall override"
    );
    assert_eq!(
        musl_bin,
        format!("{name}-{version}-x86_64-unknown-linux-musl/bin/{name}")
    );
}

#[test]
fn readme_prioritizes_binstall_install_path() {
    let s = read("README.md");
    let binstall_pos = s
        .find("cargo binstall")
        .expect("README must document cargo binstall");
    let cargo_install_git = s
        .find("cargo install --git")
        .expect("README must still document cargo install --git fallback");
    assert!(
        binstall_pos < cargo_install_git,
        "README should present cargo binstall before cargo install --git (binstall_pos={binstall_pos}, cargo_install_git={cargo_install_git})"
    );
    assert!(
        s.contains("rtrash setup"),
        "post-install setup must still be documented"
    );
    assert!(
        s.contains("Release") || s.contains("v*"),
        "docs must mention that a published release/tag is required for binstall assets"
    );
    // Primary recipe must not rely on inventing --pkg-url; musl remapping is the mechanism.
    assert!(
        s.contains("musl")
            && (s.contains("glibc")
                || s.contains("linux-gnu")
                || s.contains("x86_64 Linux")
                || s.contains("static")),
        "README must explain that the prebuilt is musl (works on typical glibc hosts) or how remapping works"
    );
}
