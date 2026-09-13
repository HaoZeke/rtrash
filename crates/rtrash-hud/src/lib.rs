//! icedtea desktop HUD over the rtrash library APIs.
//!
//! The musl CLI never depends on this crate. `rtrash desktop` execs the
//! `rtrash-hud` binary. `--offline` paints the catalog without a TTY.

pub mod app;
pub mod catalog;
pub mod cli;
pub mod install_desktop;
pub mod keys;
pub mod prefs;
pub mod summon;
pub mod theme;
pub mod tray;
pub mod view;
pub mod watch;

pub use cli::{HudCli, run_cli};
