//! Argument forms the trash-cli commands take that the rest of rtrash
//! spells differently, so `trash-put`, `trash-list`, `trash-empty`,
//! `trash-restore` and `trash-rm` accept a trash-cli command line as is.

/// Options that take a value, which trash-cli also accepts as a separate
/// word (`--trash-dir DIR`); rtrash reads the `--opt=VALUE` form.
const VALUED: &[&str] = &["--trash-dir"];

/// `args` with each `--opt VALUE` pair of [`VALUED`] joined into
/// `--opt=VALUE`. Words after `--` stay as they are.
pub fn join_values(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            out.extend(args[i..].iter().cloned());
            break;
        }
        if VALUED.contains(&a.as_str()) {
            if let Some(v) = args.get(i + 1) {
                out.push(format!("{a}={v}"));
                i += 2;
                continue;
            }
        }
        out.push(a.clone());
        i += 1;
    }
    out
}

/// trash-cli's `--print-completion SHELL` (also `=SHELL`): print the
/// completion script for bash or zsh, which covers every trash-* name.
/// `None` when `args` does not ask for it.
pub fn print_completion(prog: &str, args: &[String]) -> Option<i32> {
    let i = args
        .iter()
        .position(|a| a == "--print-completion" || a.starts_with("--print-completion="))?;
    let shell = match args[i].split_once('=') {
        Some((_, s)) => Some(s),
        None => args.get(i + 1).map(String::as_str),
    };
    Some(match shell {
        Some("bash") => {
            print!("{}", crate::shell_assets::BASH_COMPLETION);
            0
        }
        Some("zsh") => {
            print!("{}", crate::shell_assets::ZSH_COMPLETION);
            0
        }
        Some(s) => {
            eprintln!(
                "{prog}: no completion script for '{s}' (bash or zsh; see rtrash completions)"
            );
            2
        }
        None => {
            eprintln!("{prog}: --print-completion needs a shell (bash or zsh)");
            2
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn join_values_joins_a_separate_value_but_not_after_dashes() {
        assert_eq!(
            join_values(&s(&["--trash-dir", "/t", "-v", "--", "--trash-dir", "x"])),
            s(&["--trash-dir=/t", "-v", "--", "--trash-dir", "x"])
        );
        assert_eq!(join_values(&s(&["--trash-dir"])), s(&["--trash-dir"]));
        assert_eq!(join_values(&s(&["--trash-dir=/t"])), s(&["--trash-dir=/t"]));
    }
}
