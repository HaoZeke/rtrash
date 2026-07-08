//! Structural checks for README terminal demos (asciinema cast + GIF).

use std::fs;
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    fs::read_to_string(root().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

#[test]
fn demo_assets_exist_and_are_nonempty() {
    for rel in [
        "docs/demo/rtrash-quickstart.cast",
        "docs/demo/rtrash-quickstart.gif",
        "docs/demo/rtrash-suite.cast",
        "docs/demo/rtrash-suite.gif",
        "docs/demo/sequence.sh",
        "docs/demo/sequence-suite.sh",
        "docs/demo/record.sh",
    ] {
        let p = root().join(rel);
        assert!(p.is_file(), "missing {rel}");
        assert!(fs::metadata(&p).unwrap().len() > 100, "{rel} too small");
    }
    for cast in [
        "docs/demo/rtrash-quickstart.cast",
        "docs/demo/rtrash-suite.cast",
    ] {
        let h = read(cast);
        assert!(h.contains("\"version\":2"), "{cast} must be asciicast v2");
    }
    for gif in [
        "docs/demo/rtrash-quickstart.gif",
        "docs/demo/rtrash-suite.gif",
    ] {
        let b = fs::read(root().join(gif)).unwrap();
        assert!(b.starts_with(b"GIF8"), "{gif} must be GIF");
    }
}

#[test]
fn quickstart_sequence_covers_core_loop() {
    let seq = read("docs/demo/sequence.sh");
    for token in [
        "rtrash put",
        "rtrash list",
        "rtrash status",
        "rtrash restore",
        "rtrash empty",
        "RTRASH_DEMO_PIN",
        "$PIN",
    ] {
        assert!(seq.contains(token), "quickstart missing {token:?}");
    }
}

#[test]
fn suite_sequence_covers_broader_surface() {
    let seq = read("docs/demo/sequence-suite.sh");
    for token in [
        "rtrash -rf",
        "trash-put",
        "trash-list",
        "rtrash rm",
        "empty --plain -n",
        "rtrash keys",
        "RTRASH_DEMO_PIN",
        "$PIN",
    ] {
        assert!(seq.contains(token), "suite missing {token:?}");
    }
    let cast = read("docs/demo/rtrash-suite.cast");
    for token in ["-rf", "trash-put", "keys --list", "--trash-dir="] {
        assert!(
            cast.contains(token),
            "suite cast should show {token:?} from real recording"
        );
    }
}

#[test]
fn casts_are_volume_isolated() {
    for cast_rel in [
        "docs/demo/rtrash-quickstart.cast",
        "docs/demo/rtrash-suite.cast",
    ] {
        let cast = read(cast_rel);
        assert!(!cast.contains("pCloudDrive"), "{cast_rel} leaked pCloudDrive");
        assert!(!cast.contains("/.Trash-"), "{cast_rel} leaked volume /.Trash-");
        assert!(
            cast.contains("--trash-dir="),
            "{cast_rel} must show pinned --trash-dir"
        );
        for line in cast.lines() {
            if let Some(rest) = line.split("Removed ").nth(1) {
                let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                assert!(
                    digits.len() < 2,
                    "{cast_rel} bulk empty? {line}"
                );
            }
        }
    }
}

#[test]
fn readme_embeds_both_demos_and_feature_map() {
    let readme = read("README.md");
    // Absolute raw.githubusercontent.com URLs: render on GitHub *and* crates.io.
    assert!(
        readme.contains(
            "raw.githubusercontent.com/HaoZeke/rtrash/main/docs/demo/rtrash-quickstart.gif"
        ),
        "README must embed quickstart GIF via absolute raw.githubusercontent.com URL"
    );
    assert!(
        readme.contains(
            "raw.githubusercontent.com/HaoZeke/rtrash/main/docs/demo/rtrash-suite.gif"
        ),
        "README must embed suite GIF via absolute raw URL"
    );
    assert!(
        readme.contains("width=\"720\""),
        "hero GIF should set width for readable embed"
    );
    assert!(
        readme.contains("align=\"center\""),
        "hero should be centered"
    );
    assert!(readme.contains("multi-call") || readme.contains("Multi-call"));
    assert!(readme.contains("TUI") || readme.contains("TTY"));
    // Docs site static copies for Sphinx embed
    assert!(
        root().join("docs/source/_static/demo/rtrash-quickstart.gif").is_file(),
        "Sphinx _static/demo must hold quickstart GIF for the docs site"
    );
}

#[test]
fn record_script_supports_dry_run_and_pin() {
    let rec = read("docs/demo/record.sh");
    assert!(rec.contains("--dry-run"));
    assert!(rec.contains("RTRASH_DEMO_PIN"));
    assert!(rec.contains("--trash-dir="));
    assert!(rec.contains("/bin/rm")); // multi-call safe cleanup
    assert!(rec.contains("sequence-suite.sh"));
}
