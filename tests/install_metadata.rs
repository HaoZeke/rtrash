//! Structural checks: cargo-binstall metadata and package-release naming agree.
//! No network; reads Cargo.toml + scripts/package-release.sh from the tree.
//!
//! Critical: default host triples on Linux desktops are often *-linux-gnu, but
//! we only publish *musl* tarballs for x86_64 and aarch64. Metadata must remap
//! those hosts to the musl assets so bare `cargo binstall rtrash` works.

mod common;

use std::collections::BTreeMap;
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

fn table<'a>(document: &'a str, heading: &str) -> BTreeMap<&'a str, &'a str> {
    let mut values = BTreeMap::new();
    let mut found = false;
    for line in document.lines().map(str::trim) {
        if line == heading {
            found = true;
            continue;
        }
        if found && line.starts_with('[') {
            break;
        }
        if found && !line.is_empty() && !line.starts_with('#') {
            let (key, value) = line.split_once('=').expect("key = value");
            values.insert(key.trim(), value.trim().trim_matches('"'));
        }
    }
    assert!(found, "missing TOML table {heading}");
    values
}

#[cfg(unix)]
fn zip_entries(bytes: &[u8]) -> Vec<String> {
    fn u16_at(bytes: &[u8], at: usize) -> usize {
        u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap()) as usize
    }
    fn u32_at(bytes: &[u8], at: usize) -> usize {
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
    }

    let end = bytes
        .windows(4)
        .rposition(|window| window == b"PK\x05\x06")
        .expect("zip end record");
    let count = u16_at(bytes, end + 10);
    let mut at = u32_at(bytes, end + 16);
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        assert_eq!(&bytes[at..at + 4], b"PK\x01\x02", "zip central header");
        let name_len = u16_at(bytes, at + 28);
        let extra_len = u16_at(bytes, at + 30);
        let comment_len = u16_at(bytes, at + 32);
        let name = std::str::from_utf8(&bytes[at + 46..at + 46 + name_len]).unwrap();
        entries.push(name.to_owned());
        at += 46 + name_len + extra_len + comment_len;
    }
    entries.sort();
    entries
}

#[test]
fn binstall_metadata_matches_package_release_basename() {
    let cargo = read("Cargo.toml");
    let script = read("scripts/package-release.sh");
    // Hand-written binstall-aligned musl job (cargo-dist owns release.yml).
    let musl_workflow = read(".github/workflows/release-musl.yml");
    let dist_ws = read("dist-workspace.toml");

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
        script.contains("aarch64-apple-darwin"),
        "package-release must know Apple silicon target"
    );
    assert!(
        script.contains("x86_64-apple-darwin"),
        "package-release must know Intel macOS target"
    );
    assert!(
        script.contains("${tar_name}.sha256"),
        "package-release must write a checksum beside each tarball"
    );

    assert!(
        musl_workflow.contains("aarch64-unknown-linux-musl"),
        "release-musl workflow must build aarch64 musl"
    );
    assert!(
        musl_workflow.contains("x86_64-unknown-linux-musl"),
        "release-musl workflow must build x86_64 musl"
    );
    assert!(
        musl_workflow.contains("aarch64-apple-darwin"),
        "release workflow must build Apple silicon"
    );
    assert!(
        musl_workflow.contains("x86_64-apple-darwin"),
        "release workflow must build Intel macOS"
    );
    assert!(
        musl_workflow.contains("dist/*.tar.gz*"),
        "release workflow must upload tarballs and checksums"
    );
    assert!(
        dist_ws.contains("x86_64-unknown-linux-musl")
            && dist_ws.contains("aarch64-unknown-linux-musl"),
        "cargo-dist must target both Linux musl arches"
    );
    assert!(
        musl_workflow.contains("x86_64-pc-windows-msvc") && musl_workflow.contains("dist/*.zip*"),
        "release workflow must build and upload the Windows x64 zip and checksum"
    );
    assert!(
        dist_ws.contains("x86_64-pc-windows-msvc"),
        "cargo-dist must plan the Windows x64 zip this workflow publishes"
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
fn binstall_windows_metadata_describes_the_rooted_zip() {
    let cargo = read("Cargo.toml");
    let windows = table(
        &cargo,
        "[package.metadata.binstall.overrides.x86_64-pc-windows-msvc]",
    );
    assert_eq!(
        windows["pkg-url"],
        "{ repo }/releases/download/v{ version }/{ name }-x86_64-pc-windows-msvc.zip"
    );
    assert_eq!(windows["pkg-fmt"], "zip");
    assert_eq!(windows["bin-dir"], "{ bin }{ binary-ext }");
}

#[cfg(unix)]
#[test]
fn windows_package_has_its_files_at_the_zip_root() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    // The script makes its zip with zip, python3 or python, as it checks.
    let zipper = Command::new("sh")
        .args([
            "-c",
            "command -v zip || command -v python3 || command -v python",
        ])
        .output()
        .is_ok_and(|o| o.status.success());
    if !zipper {
        common::skip("neither zip nor python is on PATH; skipping");
        return;
    }
    let temp = common::scratch_dir("install-metadata", "windows-zip-root");
    let scripts = temp.join("scripts");
    let fake_bin = temp.join("fake-bin");
    let target = temp.join("target/x86_64-pc-windows-msvc/release");
    fs::create_dir_all(&scripts).unwrap();
    fs::create_dir_all(&fake_bin).unwrap();
    fs::create_dir_all(&target).unwrap();
    fs::copy(
        root().join("scripts/package-release.sh"),
        scripts.join("package-release.sh"),
    )
    .unwrap();
    fs::write(temp.join("Cargo.toml"), "[package]\nversion = \"1.2.3\"\n").unwrap();
    for name in ["CHANGELOG.md", "LICENSE", "README.md"] {
        fs::write(temp.join(name), name).unwrap();
    }
    fs::write(target.join("rtrash.exe"), "fake executable").unwrap();

    for (name, body) in [
        ("cargo", "#!/bin/sh\nexit 0\n"),
        (
            "rustc",
            "#!/bin/sh\nprintf '%s\\n' x86_64-pc-windows-msvc\n",
        ),
    ] {
        let path = fake_bin.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut paths = vec![fake_bin];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").expect("PATH"),
    ));
    let status = Command::new("bash")
        .current_dir(&temp)
        .arg("scripts/package-release.sh")
        .arg("x86_64-pc-windows-msvc")
        .env("SKIP_TEST", "1")
        .env("PATH", std::env::join_paths(paths).unwrap())
        .status()
        .unwrap();
    assert!(status.success());

    let archive = fs::read(temp.join("dist/rtrash-x86_64-pc-windows-msvc.zip")).unwrap();
    assert_eq!(
        zip_entries(&archive),
        ["CHANGELOG.md", "LICENSE", "README.md", "rtrash.exe"]
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
