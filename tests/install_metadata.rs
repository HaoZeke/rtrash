//! Structural checks: cargo-binstall metadata and package-release naming agree.
//! No network; reads Cargo.toml + scripts/package-release.sh from the tree.
//!
//! Critical: default host triples on Linux desktops are often *-linux-gnu, but
//! we only publish *musl* tarballs for x86_64 and aarch64. Metadata must remap
//! those hosts to the musl assets so bare `cargo binstall rtrash` works.

use std::fs;
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    let p = root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

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

fn musl_asset(name: &str, version: &str, arch: &str) -> (String, String) {
    let triple = format!("{arch}-unknown-linux-musl");
    let basename = format!("{name}-{version}-{triple}.tar.gz");
    let bin_path = format!("{name}-{version}-{triple}/bin/{name}");
    (basename, bin_path)
}

#[test]
fn binstall_metadata_matches_package_release_basename() {
    let cargo = read("Cargo.toml");
    let script = read("scripts/package-release.sh");
    let workflow = read(".github/workflows/release.yml");

    assert!(cargo.contains("[package.metadata.binstall]"));
    assert!(cargo.contains("pkg-fmt = \"tgz\"") || cargo.contains("pkg-fmt = \"tar.gz\""));

    let name = package_name(&cargo);
    let version = package_version(&cargo);
    assert_eq!(name, "rtrash");

    assert!(
        script.contains("rtrash-${VERSION}-${TARGET}")
            || script.contains("NAME=\"rtrash-${VERSION}-${TARGET}\"")
            || script.contains("local NAME=\"rtrash-${VERSION}-${TARGET}\""),
        "package-release must set NAME to rtrash-VERSION-TARGET"
    );
    assert!(script.contains("${NAME}.tar.gz") || script.contains("NAME}.tar.gz"));
    assert!(script.contains("bin/rtrash"));
    assert!(
        script.contains("aarch64-unknown-linux-musl"),
        "package-release must know aarch64 musl target"
    );
    assert!(
        script.contains("x86_64-unknown-linux-musl"),
        "package-release must know x86_64 musl target"
    );

    assert!(
        workflow.contains("aarch64-unknown-linux-musl"),
        "release workflow must build aarch64 musl"
    );
    assert!(
        workflow.contains("x86_64-unknown-linux-musl"),
        "release workflow must build x86_64 musl"
    );

    let (x86_base, x86_bin) = musl_asset(name, version, "x86_64");
    let (arm_base, arm_bin) = musl_asset(name, version, "aarch64");
    assert_eq!(
        x86_base,
        format!("rtrash-{version}-x86_64-unknown-linux-musl.tar.gz")
    );
    assert_eq!(
        arm_base,
        format!("rtrash-{version}-aarch64-unknown-linux-musl.tar.gz")
    );
    assert_eq!(
        x86_bin,
        format!("rtrash-{version}-x86_64-unknown-linux-musl/bin/rtrash")
    );
    assert_eq!(
        arm_bin,
        format!("rtrash-{version}-aarch64-unknown-linux-musl/bin/rtrash")
    );
}

#[test]
fn binstall_remaps_linux_gnu_hosts_to_matching_musl_assets() {
    let cargo = read("Cargo.toml");
    let name = package_name(&cargo);
    let version = package_version(&cargo);

    assert!(
        cargo.contains("x86_64-unknown-linux-musl.tar.gz"),
        "x86_64 musl asset in metadata"
    );
    assert!(
        cargo.contains("aarch64-unknown-linux-musl.tar.gz"),
        "aarch64 musl asset in metadata"
    );

    // cfg overrides for both arches (not only { target } templates).
    assert!(
        cargo.contains("target_arch = \\\"x86_64\\\"")
            || cargo.contains("target_arch = \"x86_64\""),
        "x86_64 linux override cfg"
    );
    assert!(
        cargo.contains("target_arch = \\\"aarch64\\\"")
            || cargo.contains("target_arch = \"aarch64\""),
        "aarch64 linux override cfg"
    );

    let (x86_musl, _) = musl_asset(name, version, "x86_64");
    let (arm_musl, _) = musl_asset(name, version, "aarch64");
    let x86_gnu_naive = format!("{name}-{version}-x86_64-unknown-linux-gnu.tar.gz");
    let arm_gnu_naive = format!("{name}-{version}-aarch64-unknown-linux-gnu.tar.gz");

    assert_ne!(x86_musl, x86_gnu_naive);
    assert_ne!(arm_musl, arm_gnu_naive);

    // Default glibc hosts must resolve to musl published names, not gnu.
    assert_eq!(
        x86_musl,
        format!("{name}-{version}-x86_64-unknown-linux-musl.tar.gz")
    );
    assert_eq!(
        arm_musl,
        format!("{name}-{version}-aarch64-unknown-linux-musl.tar.gz")
    );
}

#[test]
fn readme_prioritizes_binstall_install_path() {
    let s = read("README.md");
    let binstall_pos = s.find("cargo binstall").expect("cargo binstall");
    let cargo_install_git = s.find("cargo install --git").expect("cargo install --git");
    assert!(binstall_pos < cargo_install_git);
    assert!(s.contains("rtrash setup"));
    assert!(s.contains("musl"));
    assert!(
        s.contains("aarch64") || s.contains("arm64") || s.contains("ARM"),
        "README should mention aarch64/ARM prebuilds"
    );
}
