//! Structural checks for shell completions and man pages.
//! These parse checked-in static assets (no interactive shell, no cargo build of man).

use std::fs;
use std::path::PathBuf;

#[cfg(unix)]
#[path = "common/catalog_guard.rs"]
mod catalog_guard;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    let p = root().join(rel);
    let raw = fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
    normalize_newlines(&raw)
}

// Git for Windows checks files out with CRLF. `read` and the Performance
// scan both turn CRLF into LF, so a hand-built CRLF string scans like the file.
fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n")
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

// Retired rows are named by these labels. `0.21 s` is a real du --inodes
// cell on both benchmark pages, so it is not a token. The `2.5x` / `12.5x`
// pair is only a boundary example in the test; the pages use ~13.6× and ~12×.
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
    assert!(
        !performance.contains("older README table"),
        "the Performance section must not refer to a table that is not on the page"
    );
    assert!(
        performance.contains("how full empty deletes"),
        "the Performance section must point at the empty-walk notes on the page"
    );
    let benches = read("docs/benchmarks.md");
    let org = read("docs/orgmode/benchmarks.org");
    for token in RETIRED_ROW_TOKENS {
        assert!(
            !has_token(&performance, token),
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
        assert!(
            !text.contains("current dated numbers"),
            "{label} must not say current dated numbers"
        );
        assert!(
            !text.contains("last harness run"),
            "{label} must not say last harness run"
        );
        assert!(
            !text.contains("The crate version is now"),
            "{label} must not name the live crate version"
        );
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
    let org_note = org
        .lines()
        .find(|line| line.contains("The table below is still"))
        .expect("org table note");
    let org_cell = org
        .lines()
        .find(|line| line.starts_with('|') && line.contains("release"))
        .expect("org release cell");
    let note_ver = rtrash_version_token(org_note).expect("org note names rtrash");
    let cell_ver = rtrash_version_token(org_cell).expect("org cell names rtrash");
    assert_eq!(note_ver, "0.1.2", "the org sentence stays the token 0.1.2");
    assert_eq!(
        cell_ver, note_ver,
        "the org release cell must use the sentence's rtrash version"
    );
    assert_ne!(rtrash_version_token("rtrash 0.1.20 release"), Some("0.1.2"));
    assert!(
        has_token(&benches, "0.21 s"),
        "sourced du --inodes cell stays"
    );
    assert!(
        has_token(&org, "0.21 s"),
        "sourced org du --inodes cell stays"
    );
    let markdown_dates = dates_missing_from_headings(&benches).expect("markdown anchor");
    assert!(
        markdown_dates.is_empty(),
        "markdown names a later-section date that is not a heading: {markdown_dates:?}"
    );
    let org_dates = dates_missing_from_headings(&org).expect("org anchor");
    assert!(
        org_dates.is_empty(),
        "org names a later-section date that is not a heading: {org_dates:?}"
    );
    for (label, text) in [
        ("docs/benchmarks.md", &benches),
        ("docs/orgmode/benchmarks.org", &org),
    ] {
        assert!(
            has_token(text, "n_small=400"),
            "{label} must contain the token n_small=400"
        );
        assert!(
            text.contains("ASV params 50/200"),
            "{label} must contain ASV params 50/200"
        );
    }
    let after_note = benches
        .lines()
        .find(|line| line.contains("\"Before\" means"))
        .expect("2026-10-10 before/after note");
    assert!(
        after_note.contains("`242a601`"),
        "the before column must pin 242a601, got {after_note}"
    );
    assert!(
        after_note.contains("`c9db258`"),
        "the after column must pin c9db258, got {after_note}"
    );
    assert!(
        !after_note.contains("current tree"),
        "the after column must name its commit, got {after_note}"
    );
    assert!(
        !after_note.contains("recorded against rtrash 0.1.4"),
        "the after column must not read as tag v0.1.4, got {after_note}"
    );
    let news = read("docs/newsfragments/+retire-unsourced-bench-table.changed.md");
    assert!(
        news.contains("`c9db258`"),
        "the newsfragment must pin the after column at c9db258, got {news}"
    );
    assert!(
        !news.contains("recorded against rtrash 0.1.4"),
        "the newsfragment must not read as tag v0.1.4, got {news}"
    );
    assert!(
        !org.contains("2026-10-10"),
        "the org page has no 2026-10-10 section, so it does not mirror that label"
    );
}

// Bound: the guard starts at `## Performance` and runs to the next unfenced
// level-1 heading (`# `, not `##` or `###`) or the end of the file. A new
// `##` section after the Performance paragraphs stays inside the scan. A
// fenced `# ` line does not end it. `## Example` would stay inside even
// with the fence toggle broken, so the sample uses a fenced `# comment`.
#[test]
fn retired_row_tokens_ignore_longer_cells_and_inner_headings() {
    assert!(!has_token("12.5x", "2.5x"));
    assert!(!has_token("152x", "52x"));
    assert!(!has_token("~1.52x", "52x"));
    assert!(!has_token("~13.52x", "52x"));
    assert!(!has_token("1.52x", "52x"));
    assert!(!has_token("~1.52×", "52×"));
    assert!(!has_token("~13.52×", "52×"));
    assert!(!has_token("1.52×", "52×"));
    assert!(!has_token("1 100 000 entries", "100 000 entries"));
    assert!(has_token("52x", "52x"));
    assert!(has_token("| 52x |", "52x"));
    assert!(has_token("100 000 entries", "100 000 entries"));
    assert!(has_token("0.21 s", "0.21 s"));
    let sample = [
        "## Performance",
        "",
        "intro",
        "",
        "### Nested",
        "",
        "| `empty`, 100 000 entries | 0.92 s | 0.36 s | 2.5x |",
        "",
        "## Notes",
        "",
        "still 52x",
        "",
        "```",
        "# comment",
        "```",
        "",
        "after fence 52×",
        "",
        "# Outside",
        "",
        "20 000 entries",
    ]
    .join("\n");
    let body = performance_section(&sample);
    assert!(has_token(&body, "100 000 entries"));
    assert!(has_token(&body, "52x"));
    assert!(has_token(&body, "52×"));
    assert!(body.contains("### Nested"));
    assert!(body.contains("## Notes"));
    assert!(
        body.contains("# comment"),
        "a fenced level-1 comment must stay inside the scan, got {body}"
    );
    assert!(!has_token(&body, "20 000 entries"));
    let fenced_date = "\
Later sections of this page name 2026-10-10.

```
# 2026-10-10 shell comment
```

* measured (2026-10-13)
";
    assert_eq!(
        dates_missing_from_headings(fenced_date).expect("anchor"),
        vec!["2026-10-10"]
    );
    assert!(
        dates_missing_from_headings("# 2026-10-10\n").is_err(),
        "a page with no later-sections sentence must fail"
    );
    let r2_org = "\
* Measured results (Linux x86_64, 2026-07-08)

Later sections of this page are other measurements, including du runs
from 2026-10-10, 2026-10-13, and 2026-10-14.

* du against dua, gdu and du (Linux x86_64, 2026-10-13)

** One wide directory (2026-10-14)
";
    assert_eq!(
        dates_missing_from_headings(r2_org).expect("anchor"),
        vec!["2026-10-10"]
    );
    let org_src = "\
Later sections of this page name 2026-10-10.

#+begin_src shell
# 2026-10-10 inside the source block
#+end_src

* measured (2026-10-13)
";
    assert_eq!(
        dates_missing_from_headings(org_src).expect("anchor"),
        vec!["2026-10-10"]
    );
    let org_after = "\
Later sections of this page name 2026-10-10.

#+BEGIN_SRC shell
# comment
#+END_SRC

* run (2026-10-10)
";
    assert!(
        dates_missing_from_headings(org_after)
            .expect("anchor")
            .is_empty(),
        "a heading after #+end_src still counts"
    );
    assert!(
        !has_token("n_small=4000", "n_small=400"),
        "n_small=400 must not match inside n_small=4000"
    );
    assert!(has_token("(n_small=400; ASV params 50/200)", "n_small=400"));
    assert!(has_token(
        "(=n_small=400=; ASV params 50/200)",
        "n_small=400"
    ));
}

#[test]
fn a_crlf_readme_scans_like_lf() {
    let lf = read("README.md");
    let crlf = lf.replace('\n', "\r\n");
    let from_lf = performance_section(&lf);
    let from_crlf = performance_section(&crlf);
    assert_eq!(from_crlf, from_lf);
    assert!(
        from_lf.contains("](docs/benchmarks.md)"),
        "the CRLF scan must keep the benchmark link, got {from_lf}"
    );
    assert!(from_lf.contains("how full empty deletes"));
    assert!(!from_lf.contains("further optimized"));
    for token in RETIRED_ROW_TOKENS {
        assert_eq!(
            has_token(&from_lf, token),
            has_token(&from_crlf, token),
            "{token}"
        );
        assert!(!has_token(&from_lf, token), "{token}");
    }
}

// See `retired_row_tokens_ignore_longer_cells_and_inner_headings` for the bound.
fn performance_section(readme: &str) -> String {
    let readme = normalize_newlines(readme);
    let marker = "## Performance\n";
    let start = readme
        .find(marker)
        .expect("README has a Performance section");
    let rest = &readme[start + marker.len()..];
    let mut end = rest.len();
    let mut offset = 0;
    let mut in_fence = false;
    while offset < rest.len() {
        let next = rest[offset..]
            .find('\n')
            .map(|n| offset + n + 1)
            .unwrap_or(rest.len());
        let line = rest[offset..next].trim_end_matches(['\n', '\r']);
        if is_fence_line(line) {
            in_fence = !in_fence;
            offset = next;
            continue;
        }
        if !in_fence && line.starts_with("# ") {
            end = offset;
            break;
        }
        offset = next;
    }
    rest[..end].to_string()
}

fn is_fence_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
        return true;
    }
    let bytes = trimmed.as_bytes();
    starts_with_ignore_ascii_case(bytes, b"#+begin_")
        || starts_with_ignore_ascii_case(bytes, b"#+end_")
}

fn starts_with_ignore_ascii_case(text: &[u8], prefix: &[u8]) -> bool {
    text.len() >= prefix.len() && text[..prefix.len()].eq_ignore_ascii_case(prefix)
}

fn has_token(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let digit_lead = needle.chars().next().is_some_and(|c| c.is_ascii_digit());
    hay.match_indices(needle).any(|(i, _)| {
        let after = hay[i + needle.len()..].chars().next();
        if !after.is_none_or(|ch| !ch.is_ascii_alphanumeric()) {
            return false;
        }
        if digit_lead && grouped_number_continues(hay, i) {
            return false;
        }
        hay[..i]
            .chars()
            .next_back()
            .is_none_or(|ch| !ch.is_ascii_alphanumeric())
    })
}

// A digit-leading token is still the same number when the character before
// it is a digit, a decimal point, or a space that a digit groups.
fn grouped_number_continues(hay: &str, at: usize) -> bool {
    let mut prev = hay[..at].chars().rev();
    let Some(first) = prev.next() else {
        return false;
    };
    if first.is_ascii_digit() || first == '.' {
        return true;
    }
    if first != ' ' && first != '\t' {
        return false;
    }
    prev.find(|c| *c != ' ' && *c != '\t')
        .is_some_and(|c| c.is_ascii_digit())
}

fn rtrash_version_token(text: &str) -> Option<&str> {
    let idx = text.find("rtrash ")?;
    let rest = &text[idx + "rtrash ".len()..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    let ver = rest[..end].trim_end_matches('.');
    if ver.contains('.') && ver.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        Some(ver)
    } else {
        None
    }
}

fn is_heading(line: &str) -> bool {
    let Some(mark) = line.chars().next() else {
        return false;
    };
    if mark != '#' && mark != '*' {
        return false;
    }
    let marks = line.chars().take_while(|c| *c == mark).count();
    line[marks..].starts_with(' ')
}

fn later_sections_sentence(text: &str) -> Option<&str> {
    let start = text.find("Later sections of this page")?;
    let rest = &text[start..];
    let end = rest.find('.')?;
    Some(&rest[..end])
}

fn iso_dates(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut dates = Vec::new();
    let mut i = 0;
    while i + 10 <= bytes.len() {
        if is_iso_date(bytes, i)
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
            && (i + 10 == bytes.len() || !bytes[i + 10].is_ascii_alphanumeric())
        {
            dates.push(&text[i..i + 10]);
            i += 10;
        } else {
            i += 1;
        }
    }
    dates
}

fn is_iso_date(bytes: &[u8], i: usize) -> bool {
    let s = &bytes[i..i + 10];
    s[4] == b'-'
        && s[7] == b'-'
        && s[..4].iter().all(u8::is_ascii_digit)
        && s[5..7].iter().all(u8::is_ascii_digit)
        && s[8..10].iter().all(u8::is_ascii_digit)
}

fn unfenced_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        if is_fence_line(line) {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence {
            lines.push(line);
        }
    }
    lines
}

fn dates_missing_from_headings(text: &str) -> Result<Vec<&str>, &'static str> {
    let Some(sentence) = later_sections_sentence(text) else {
        return Err("missing Later sections of this page");
    };
    let headings = unfenced_lines(text);
    Ok(iso_dates(sentence)
        .into_iter()
        .filter(|date| {
            !headings
                .iter()
                .any(|line| is_heading(line) && has_token(line, date))
        })
        .collect())
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
    catalog_guard::install();
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
