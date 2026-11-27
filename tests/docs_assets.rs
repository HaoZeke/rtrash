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
    assert!(missing.is_empty(), "{label}: missing tokens: {missing:?}");
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
            "--json",
            "older-than",
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

// Retired rows are named by these labels. Times such as `0.21 s` and ratios
// such as `2.5x` also occur in sourced cells (`12.5x` contains `2.5x`), and
// "not continuously re-verified" is the old caveat, not a row.
const RETIRED_ROW_TOKENS: [&str; 4] = ["100 000 entries", "20 000 entries", "52x", "52×"];

#[test]
fn readme_performance_points_at_the_sourced_benchmarks() {
    let readme = read("README.md");
    let performance = performance_section(&readme);
    assert!(
        performance.contains("](docs/benchmarks.md)"),
        "the Performance section must point at the sourced benchmark page"
    );
    assert!(
        !performance.contains("mainly wins on process startup"),
        "the Performance section must not keep the unsourced startup claim"
    );
    let benches = read("docs/benchmarks.md");
    let org = read("docs/orgmode/benchmarks.org");
    for token in RETIRED_ROW_TOKENS {
        assert!(
            !has_token(performance, token),
            "retired row `{token}` is back in the Performance section"
        );
        assert!(
            !has_token(&benches, token),
            "retired row `{token}` is in docs/benchmarks.md"
        );
        assert!(
            !has_token(&org, token),
            "retired row `{token}` is in docs/orgmode/benchmarks.org"
        );
    }
    assert!(
        benches.contains("benches/compare_trash_cli.py"),
        "the benchmark page must name the harness that produced the dated table"
    );
    let compare = "This is the last 400-file put/empty compare shipped in-tree.";
    let later = "Of the two 400-file put/empty compares, the";
    for (label, text) in [
        ("docs/benchmarks.md", &benches),
        ("docs/orgmode/benchmarks.org", &org),
    ] {
        assert!(
            text.contains(compare),
            "{label} must scope the July compare"
        );
        assert!(
            text.contains("before quoting it as current"),
            "{label} must say to re-run before quoting the compare as current"
        );
        assert!(
            text.contains("release build of that tree"),
            "{label} must call 0.1.2 a release build of that tree"
        );
        assert!(
            text.contains(later),
            "{label} must keep the July compare the later of the two"
        );
        assert!(!text.contains("current dated numbers"), "{label}");
        assert!(!text.contains("last harness run"), "{label}");
        assert!(!text.contains("The crate version is now"), "{label}");
    }
    let table_line = benches
        .lines()
        .find(|line| line.contains("The table below is still"))
        .expect("table note");
    let peers_line = benches
        .lines()
        .find(|line| line.starts_with("Peers:"))
        .expect("peers line");
    let table_ver = bold_rtrash_version(table_line).expect("table note names rtrash");
    let peers_ver = bold_rtrash_version(peers_line).expect("peers line names rtrash");
    assert_eq!(table_ver, "0.1.2", "the 400-file table stays labeled 0.1.2");
    assert_eq!(
        peers_ver, table_ver,
        "docs/benchmarks.md peers line must use the table note's version"
    );
    assert!(peers_line.contains("(release build of that tree)"));
    assert!(benches.contains("the host of that run"));
    assert!(!benches.contains("build host for this tree"));
    // cog.toml bumps Cargo.toml, pyproject.toml, and conf.py. It leaves this page.
    assert_ne!(crate_version(), table_ver);
    assert!(org.contains("The table below is still rtrash 0.1.2"));
    assert!(
        has_token(&benches, "0.21 s"),
        "sourced du --inodes cell stays"
    );
    assert!(
        has_token(&org, "0.21 s"),
        "sourced org du --inodes cell stays"
    );
}

#[test]
fn retired_row_tokens_ignore_longer_cells_and_inner_headings() {
    assert!(!has_token("12.5x", "2.5x"));
    assert!(!has_token("152x", "52x"));
    assert!(has_token("| 52x |", "52x"));
    assert!(has_token("0.21 s", "0.21 s"));
    let sample = "\
## Performance

intro

### Nested

| `empty`, 100 000 entries | 0.92 s | 0.36 s | 2.5x |

## Development

20 000 entries
";
    let body = performance_section(sample);
    assert!(has_token(body, "100 000 entries"));
    assert!(!has_token(body, "20 000 entries"));
    assert!(body.contains("### Nested"));
}

// A `###` line contains the bytes `## `, but it is not the next section.
fn performance_section(readme: &str) -> &str {
    let marker = "## Performance\n";
    let start = readme
        .find(marker)
        .expect("README has a Performance section");
    let rest = &readme[start + marker.len()..];
    let mut end = rest.len();
    let mut offset = 0;
    while offset < rest.len() {
        let next = rest[offset..]
            .find('\n')
            .map(|n| offset + n + 1)
            .unwrap_or(rest.len());
        let line = rest[offset..next].trim_end_matches(['\n', '\r']);
        if line.starts_with("## ") {
            end = offset;
            break;
        }
        offset = next;
    }
    &rest[..end]
}

fn has_token(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    hay.match_indices(needle).any(|(i, _)| {
        let before = hay[..i].chars().next_back();
        let after = hay[i + needle.len()..].chars().next();
        let open = |c: Option<char>| c.is_none_or(|ch| !ch.is_ascii_alphanumeric());
        open(before) && open(after)
    })
}

fn bold_rtrash_version(line: &str) -> Option<&str> {
    let rest = line.split("**rtrash ").nth(1)?;
    let ver = rest.split("**").next()?.trim();
    if ver.contains('.') && ver.chars().all(|c| c.is_ascii_digit() || c == '.') {
        Some(ver)
    } else {
        None
    }
}

fn crate_version() -> String {
    read("Cargo.toml")
        .lines()
        .find_map(|line| {
            line.strip_prefix("version = \"")
                .and_then(|rest| rest.strip_suffix('"'))
                .map(str::to_string)
        })
        .expect("Cargo.toml package version")
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
            s.contains(name)
                || (s.contains("trash-put")
                    && s.contains(".fish")
                    && s.contains("ln -sf rtrash.fish")),
            "package-release must stage fish multi-call file for {name}"
        );
    }
    assert!(
        s.contains("ln -sf rtrash.fish"),
        "package-release must symlink multi-call fish completions to rtrash.fish"
    );
}

#[cfg(unix)]
#[test]
fn interop_smoke_script_passes_with_the_tools_present() {
    // CI installs trash-cli, gtrash and gio and runs them. This test sets
    // RTRASH_INTEROP_NO_GIO=1 to leave gio out. The run pairs rtrash with
    // itself and with each of trash-cli and gtrash on PATH.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    // Through bash by name: a shebang of /usr/bin/env fails where that
    // path does not exist, as in the Nix build sandbox.
    let out = std::process::Command::new("bash")
        .arg(root.join("scripts/interop-smoke.sh"))
        .arg(env!("CARGO_BIN_EXE_rtrash"))
        .env("RTRASH_INTEROP_NO_GIO", "1")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("ok   rtrash -> rtrash"), "{text}");
    assert!(
        text.contains("note: RTRASH_INTEROP_NO_GIO=1 leaves gio out"),
        "{text}"
    );
    assert!(!text.contains("gio or gvfsd missing"), "{text}");
    let ci = read(".github/workflows/ci.yml");
    assert!(ci.contains("scripts/interop-smoke.sh target/release/rtrash"));
}
