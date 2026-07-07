//! Structural checks: cargo-binstall metadata and package-release naming agree.
//! No network; reads Cargo.toml + scripts/package-release.sh from the tree.

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
    for line in cargo_toml.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("version = \"") {
            return rest.strip_suffix('"').expect("version quote");
        }
        // Stop at next table so we do not pick dependency versions.
        if line.starts_with('[') && line != "[package]" {
            break;
        }
    }
    panic!("package version not found in Cargo.toml");
}

fn package_name(cargo_toml: &str) -> &str {
    for line in cargo_toml.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("name = \"") {
            return rest.strip_suffix('"').expect("name quote");
        }
        if line.starts_with('[') && line != "[package]" {
            break;
        }
    }
    panic!("package name not found");
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
        cargo.contains("pkg-url = \"{ repo }/releases/download/v{ version }/{ name }-{ version }-{ target }.tar.gz\""),
        "pkg-url must use name-version-target.tar.gz under v{{version}} tags"
    );
    assert!(
        cargo.contains("bin-dir = \"{ name }-{ version }-{ target }/bin/{ bin }{ binary-ext }\""),
        "bin-dir must point at staged bin/ inside the versioned directory"
    );
    assert!(
        cargo.contains("pkg-fmt = \"tgz\"") || cargo.contains("pkg-fmt = \"tar.gz\""),
        "pkg-fmt must be a tar.gz style format"
    );

    let name = package_name(&cargo);
    let version = package_version(&cargo);
    let target = "x86_64-unknown-linux-musl";
    let expected_basename = format!("{name}-{version}-{target}.tar.gz");
    let expected_dirname = format!("{name}-{version}-{target}");

    // package-release.sh builds NAME="rtrash-${VERSION}-${TARGET}" then TAR=…/${NAME}.tar.gz
    assert!(
        script.contains("NAME=\"rtrash-${VERSION}-${TARGET}\"")
            || script.contains("NAME=\"${NAME:-")
            || script.contains("NAME=\"rtrash-"),
        "package-release must set NAME to rtrash-VERSION-TARGET"
    );
    assert!(
        script.contains("TAR=\"$OUT_DIR/${NAME}.tar.gz\"")
            || script.contains("${NAME}.tar.gz"),
        "package-release must emit ${{NAME}}.tar.gz"
    );
    assert!(
        script.contains("STAGE/bin/rtrash") || script.contains("bin/rtrash"),
        "package-release must stage bin/rtrash (binstall bin-dir)"
    );

    // Expanded basename for current Cargo.toml version (template without hard-coding only the version in assertions about metadata keys).
    assert_eq!(
        expected_basename,
        format!("rtrash-{version}-x86_64-unknown-linux-musl.tar.gz")
    );
    assert_eq!(
        expected_dirname,
        format!("rtrash-{version}-x86_64-unknown-linux-musl")
    );

    // Script derives VERSION from Cargo.toml the same way consumers would.
    assert!(
        script.contains("Cargo.toml"),
        "package-release must read version from Cargo.toml"
    );

    // Document the alignment comment in the script (maintainers).
    assert!(
        script.contains("metadata.binstall") || script.contains("cargo-binstall") || script.contains("binstall"),
        "package-release should mention binstall naming lockstep"
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
    // Recommended / primary path should appear before the from-source fallback.
    assert!(
        binstall_pos < cargo_install_git,
        "README should present cargo binstall before cargo install --git (binstall_pos={binstall_pos}, cargo_install_git={cargo_install_git})"
    );
    assert!(
        s.contains("rtrash setup"),
        "post-install setup must still be documented"
    );
    assert!(
        s.contains("v{") || s.contains("v*") || s.contains("tag") || s.contains("Release"),
        "docs must mention that a published release/tag is required for binstall assets"
    );
}
