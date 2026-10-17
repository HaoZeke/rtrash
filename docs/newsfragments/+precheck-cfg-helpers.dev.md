Precheck flags a test helper with no `#[cfg(...)]` when every caller shares one cfg. Such a helper turns into dead code on the other targets and fails cross-clippy there.
