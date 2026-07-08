//! Structural checks for README terminal demos (asciinema cast + GIF).

use std::fs;
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn demo_assets_exist_and_are_nonempty() {
    let cast = root().join("docs/demo/rtrash-quickstart.cast");
    let gif = root().join("docs/demo/rtrash-quickstart.gif");
    let seq = root().join("docs/demo/sequence.sh");
    let rec = root().join("docs/demo/record.sh");
    for p in [&cast, &gif, &seq, &rec] {
        assert!(p.is_file(), "missing {}", p.display());
        let meta = fs::metadata(p).unwrap();
        assert!(meta.len() > 100, "{} too small ({})", p.display(), meta.len());
    }
    let cast_head = fs::read_to_string(&cast).unwrap();
    assert!(
        cast_head.starts_with('{') && cast_head.contains("\"version\":2"),
        "cast must be asciicast v2 JSON header"
    );
    let gif_magic = fs::read(&gif).unwrap();
    assert!(gif_magic.starts_with(b"GIF8"), "demo gif must be a GIF");
}

#[test]
fn sequence_covers_core_freedesktop_loop() {
    let seq = fs::read_to_string(root().join("docs/demo/sequence.sh")).unwrap();
    for token in [
        "rtrash put",
        "rtrash list",
        "rtrash status",
        "rtrash restore",
        "rtrash empty",
    ] {
        assert!(
            seq.contains(token),
            "sequence.sh must include {token:?} for the attract demo story"
        );
    }
}

#[test]
fn sequence_and_record_pin_trash_dir_for_isolation() {
    let seq = fs::read_to_string(root().join("docs/demo/sequence.sh")).unwrap();
    let rec = fs::read_to_string(root().join("docs/demo/record.sh")).unwrap();
    assert!(
        seq.contains("RTRASH_DEMO_PIN"),
        "sequence must require RTRASH_DEMO_PIN (refuse unpinned multi-volume discovery)"
    );
    assert!(
        seq.contains("$PIN") || seq.contains("${PIN}"),
        "sequence must apply PIN on list/status/restore/empty"
    );
    assert!(
        rec.contains("--trash-dir=") && rec.contains("RTRASH_DEMO_PIN"),
        "record.sh must export RTRASH_DEMO_PIN=--trash-dir=$XDG_DATA_HOME/Trash"
    );
    let cast = fs::read_to_string(root().join("docs/demo/rtrash-quickstart.cast")).unwrap();
    assert!(
        !cast.contains("pCloudDrive"),
        "cast must not list foreign volume paths (pCloudDrive)"
    );
    assert!(
        !cast.contains("/.Trash-"),
        "cast must not list volume /.Trash-$uid paths"
    );
    assert!(
        cast.contains("--trash-dir=") || cast.contains("trash-dir"),
        "cast must show pinned --trash-dir in the recorded commands"
    );
    assert!(
        cast.contains("Removed 1 item"),
        "cast empty should remove only the one leftover demo item"
    );
    // Hard fail if double-digit bulk empty appeared (old broken multi-volume demo).
    for line in cast.lines() {
        if let Some(rest) = line.split("Removed ").nth(1) {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            assert!(
                digits.len() < 2,
                "cast empty must not remove double-digit item counts (isolation): {line}"
            );
        }
    }
}

#[test]
fn readme_embeds_demo_gif() {
    let readme = fs::read_to_string(root().join("README.md")).unwrap();
    assert!(
        readme.contains("docs/demo/rtrash-quickstart.gif"),
        "README must embed the quickstart GIF"
    );
    assert!(
        readme.contains("docs/demo/rtrash-quickstart.cast") || readme.contains("docs/demo/"),
        "README should point at demo sources"
    );
    let gs = fs::read_to_string(root().join("docs/getting-started.md")).unwrap();
    assert!(
        gs.contains("rtrash-quickstart.gif") || gs.contains("demo/"),
        "getting-started should reference the demo"
    );
}

#[test]
fn record_script_supports_dry_run() {
    let rec = fs::read_to_string(root().join("docs/demo/record.sh")).unwrap();
    assert!(rec.contains("--dry-run"));
    assert!(rec.contains("asciinema"));
    assert!(rec.contains("agg"));
    assert!(rec.contains("XDG_DATA_HOME"));
    assert!(rec.contains("RTRASH_DEMO_PIN"));
}
