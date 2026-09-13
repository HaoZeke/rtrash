//! icedtea desktop HUD over the rtrash library APIs.
//!
//! The musl CLI never depends on this crate. `rtrash desktop` execs the
//! `rtrash-hud` binary. `--offline` paints the catalog without a TTY.

pub mod app;
pub mod catalog;
pub mod cli;
pub mod keys;
pub mod theme;
pub mod view;

pub use cli::{HudCli, run_cli};
