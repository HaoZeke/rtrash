//! `rtrash-hud` process entry. `rtrash desktop` execs this binary.

fn main() {
    let code = match rtrash_hud::run_cli() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("rtrash-hud: {err}");
            1
        }
    };
    std::process::exit(code);
}
