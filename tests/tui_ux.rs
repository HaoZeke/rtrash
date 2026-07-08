//! Structural + pure UX checks for restore/empty/put TUI consistency.
//! Does not open an alternate-screen session.

use std::fs;
use std::path::PathBuf;

#[test]
fn core_key_tokens_present_in_all_three_browsers() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tokens = rtrash::tui_keys::required_key_tokens();
    for rel in ["src/restore_tui.rs", "src/empty_tui.rs", "src/put_tui.rs"] {
        let s = fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
        for t in tokens {
            assert!(
                s.contains(t),
                "{rel} missing core key token {t:?} (SOTA shared keymap)"
            );
        }
        // Live filter: refilter on draft mutation
        assert!(
            s.contains("refilter_query") && s.contains("status_filter_live"),
            "{rel} must live-refilter draft"
        );
    }
}

#[test]
fn shared_help_and_scroll_helpers_exist() {
    assert!(!rtrash::tui_keys::core_help_lines().is_empty());
    assert!(rtrash::tui_keys::CORE_BROWSE_HINT.contains("Space"));
    assert!(rtrash::tui_keys::CORE_BROWSE_HINT.contains('?'));
    assert!(rtrash::tui_keys::FILTER_HINT.contains("live"));
    // scroll math
    let off = rtrash::tui_list::scroll_offset(Some(12), 50, 8, 0);
    assert_eq!(off, 5); // 12 visible in rows 5..12
    let p = rtrash::tui_list::page_selected(Some(0), 100, 8, true);
    assert_eq!(p, Some(8));
}

#[test]
fn docs_document_shared_tui_keys() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let readme = fs::read_to_string(root.join("README.md")).unwrap();
    let gs = fs::read_to_string(root.join("docs/getting-started.md")).unwrap();
    for doc in [&readme, &gs] {
        assert!(doc.contains('?') || doc.contains("help"), "docs mention help");
        assert!(doc.contains("Space") || doc.contains("`Space`"));
        assert!(doc.contains("fuzzy") || doc.contains("Fuzzy"));
        assert!(
            doc.contains("PgUp")
                || doc.contains("Page")
                || doc.contains("page")
                || doc.contains("PgDn"),
            "docs mention paging"
        );
    }
    // Single shared key table or explicit shared model
    assert!(
        readme.contains("TUI keys") || readme.contains("shared") || gs.contains("| Key |"),
        "docs expose key table"
    );
}

#[test]
fn live_fuzzy_rank_shrinks_via_shipped_fuzzy() {
    // Drive real tui_fuzzy through progressive queries (same as live filter).
    let hays = [
        "/home/u/notes.md",
        "/home/u/notebook/x",
        "/var/log/noise",
        "/tmp/n1o2t3e4s",
    ];
    let refs: Vec<&str> = hays.to_vec();
    let mut prev = rtrash::tui_fuzzy::rank_indices(&refs, "");
    assert_eq!(prev.len(), 4);
    for q in ["n", "no", "not", "note"] {
        let cur = rtrash::tui_fuzzy::rank_indices(&refs, q);
        assert!(
            cur.len() <= prev.len(),
            "query {q:?} grew set {cur:?} vs {prev:?}"
        );
        for &i in &cur {
            assert!(prev.contains(&i) || q.len() == 1, "index lost order at {q}");
        }
        // Every result must still fuzzy-match
        for &i in &cur {
            assert!(
                rtrash::tui_fuzzy::fuzzy_score(hays[i], q).is_some(),
                "ranked non-match"
            );
        }
        prev = cur;
    }
}


#[test]
fn keybinds_config_partial_override_via_shipped_api() {
    use crossterm::event::{KeyCode, KeyModifiers};
    use rtrash::tui_binds::{Action, Keymap};
    let mut m = Keymap::builtin();
    m.apply_config_text("[keys]\ntoggle_mark = \"m\"\nhelp = \"h\"\n")
        .unwrap();
    assert_eq!(
        m.resolve_browse(KeyCode::Char('m'), KeyModifiers::NONE),
        Some(Action::ToggleMark)
    );
    assert_eq!(
        m.resolve_browse(KeyCode::Char(' '), KeyModifiers::NONE),
        None
    );
    assert_eq!(
        m.resolve_browse(KeyCode::Char('h'), KeyModifiers::NONE),
        Some(Action::Help)
    );
}

#[test]
fn keybinds_sample_is_valid_config() {
    let sample = rtrash::tui_binds::Keymap::sample_config();
    let mut m = rtrash::tui_binds::Keymap::builtin();
    m.apply_config_text(&sample).unwrap();
    assert!(!m.format_table().is_empty());
}
