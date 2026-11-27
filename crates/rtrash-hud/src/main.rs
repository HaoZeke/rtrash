//! Process entry for the `rtrash-hud` status display.
//!
//! `rtrash desktop` is not a subcommand of the CLI. The desktop launcher
//! execs this binary.

fn main() {
    std::process::exit(rtrash_hud::run());
}
