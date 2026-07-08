//! Shared TUI key semantics and help copy for restore / empty / put browsers.

/// Core roles present in every browser (browse mode).
pub const CORE_BROWSE_HINT: &str =
    "↑↓/jk  PgUp/Dn  Space mark  a/A all/none  / fuzzy  ? help  q quit";

/// Filter mode footer (live refilter on each keystroke).
pub const FILTER_HINT: &str = "live fuzzy…  Enter commit  Esc cancel (restore prior filter)";

/// Confirm mode footer.
pub const CONFIRM_HINT: &str = "y confirm  n/Esc cancel";

/// Help mode footer.
pub const HELP_HINT: &str = "? or Esc close help";

/// Multi-line help body for the `?` overlay (shared core; browsers append extras).
pub fn core_help_lines() -> &'static [&'static str] {
    &[
        "Navigation",
        "  ↑↓ / j k     move selection",
        "  PgUp / PgDn  page by viewport",
        "  g / Home     first item",
        "  G / End      last item",
        "Multi-select",
        "  Space        toggle mark on cursor",
        "  a            mark all visible",
        "  A            clear all marks",
        "Filter",
        "  /            open live fuzzy filter",
        "  (type)       refilter on each keystroke",
        "  Enter        commit filter · Esc cancel",
        "Actions",
        "  Enter        primary action (restore / purge / put)",
        "  y / n        confirm / cancel destructive bulk",
        "  ?            toggle this help",
        "  q / Esc      quit (in browse mode)",
        "  Ctrl-c       quit",
    ]
}

/// Footer line for browse mode with browser-specific action label and extras.
pub fn browse_footer(action: &str, extras: &str) -> String {
    if extras.is_empty() {
        format!("{CORE_BROWSE_HINT}  Enter {action}")
    } else {
        format!("{CORE_BROWSE_HINT}  Enter {action}  {extras}")
    }
}

/// Status line after marking.
pub fn status_marked(n: usize) -> String {
    format!("marked {n}")
}

pub fn status_marked_all(n: usize) -> String {
    format!("marked all visible ({n})")
}

pub fn status_cleared() -> &'static str {
    "cleared marks"
}

pub fn status_filter_live(draft: &str, n: usize) -> String {
    if draft.is_empty() {
        format!("live filter (all) · {n} shown · Enter commit · Esc cancel")
    } else {
        format!("live filter {draft:?} · {n} match(es) · Enter commit · Esc cancel")
    }
}

pub fn status_filter_committed(applied: &str, n: usize) -> String {
    if applied.is_empty() {
        "filter cleared".into()
    } else {
        format!("fuzzy {applied:?} · {n} match(es)")
    }
}

pub fn status_filter_cancelled() -> &'static str {
    "filter cancelled (prior filter restored)"
}

/// Strings every TUI source must bind (structural tests).
pub fn required_key_tokens() -> &'static [&'static str] {
    &[
        "KeyCode::Char(' ')",
        "KeyCode::Char('a')",
        "KeyCode::Char('A')",
        "KeyCode::Char('/')",
        "KeyCode::Char('?')",
        "KeyCode::Char('q')",
        "KeyCode::PageDown",
        "KeyCode::PageUp",
        "live",
    ]
}
