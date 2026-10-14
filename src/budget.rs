//! The trash budget for the prune after put: a size from `RTRASH_MAX_SIZE`
//! or `policy.max_size`, and which items go first. Shared by the Unix
//! trash and the Windows Recycle Bin.

use crate::filters::parse_size;

/// Which items go first when the trash is over budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Oldest,
    Largest,
}

impl Policy {
    pub fn parse(s: &str) -> Option<Policy> {
        match s {
            "oldest" => Some(Policy::Oldest),
            "largest" => Some(Policy::Largest),
            _ => None,
        }
    }
}

/// The budget from `RTRASH_MAX_SIZE` and the policy from `RTRASH_PRUNE`
/// (`oldest` unless set), for the prune after put. A bad value warns once
/// and turns the prune off.
pub fn from_env(prog: &str) -> Option<(u64, Policy)> {
    let raw = std::env::var("RTRASH_MAX_SIZE").ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    let Some(max) = parse_size(&raw) else {
        eprintln!("{prog}: warning: ignoring RTRASH_MAX_SIZE='{raw}' (want a size such as 10G)");
        return None;
    };
    Some((max, env_policy(prog).unwrap_or(Policy::Oldest)))
}

/// `RTRASH_PRUNE`, when set to something.
fn env_policy(prog: &str) -> Option<Policy> {
    match std::env::var("RTRASH_PRUNE") {
        Ok(v) if !v.is_empty() => match Policy::parse(&v) {
            Some(p) => Some(p),
            None => {
                eprintln!("{prog}: warning: ignoring RTRASH_PRUNE='{v}' (want oldest or largest)");
                None
            }
        },
        _ => None,
    }
}

/// The budget for the prune after put, and where it came from: the
/// environment when `RTRASH_MAX_SIZE` is set (empty turns it off), else
/// `[policy]` in the config file. `RTRASH_PRUNE` beats `policy.prune`.
pub fn budget(prog: &str) -> Option<(u64, Policy, &'static str)> {
    if std::env::var_os("RTRASH_MAX_SIZE").is_some() {
        return from_env(prog).map(|(m, p)| (m, p, "RTRASH_MAX_SIZE"));
    }
    let cfg = crate::config::get();
    let raw = cfg.str("policy", "max_size")?;
    let Some(max) = parse_size(raw) else {
        eprintln!(
            "{prog}: warning: {}",
            cfg.bad_value("policy", "max_size", "a size such as 10G")
        );
        return None;
    };
    let policy = env_policy(prog)
        .or_else(|| {
            let v = cfg.str("policy", "prune")?;
            let p = Policy::parse(v);
            if p.is_none() {
                eprintln!(
                    "{prog}: warning: {}",
                    cfg.bad_value("policy", "prune", "oldest or largest")
                );
            }
            p
        })
        .unwrap_or(Policy::Oldest);
    Some((max, policy, "policy.max_size in the config file"))
}
