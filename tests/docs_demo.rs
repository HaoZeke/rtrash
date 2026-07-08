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
    assert!(
        gif_magic.starts_with(b"GIF8"),
        "demo gif must be a GIF"
    );
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
fn readme_embeds_demo_gif() {
    let readme = fs::read_to_string(root().join("README.md")).unwrap();
    assert!(
        readme.contains("docs/demo/rtrash-quickstart.gif"),
        "README must embed the quickstart GIF"
    );
    assert!(
        readme.contains("docs/demo/rtrash-quickstart.cast")
            || readme.contains("docs/demo/"),
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
}
