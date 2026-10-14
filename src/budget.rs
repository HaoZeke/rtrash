//! The trash budget for the prune after put: a size from `RTRASH_MAX_SIZE`
//! or `policy.max_size`, and which items go first. Shared by the Unix
//! trash and the Windows Recycle Bin.

use std::path::{Path, PathBuf};

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

/// Indices of `items` (deletion time, bytes) to purge, in order, so the
/// rest totals at most `max` bytes: oldest first, or largest first when
/// `largest` is set (ties go to the older item).
pub fn pick_budget(items: &[(i64, u64)], max: u64, largest: bool) -> Vec<usize> {
    let mut total: u64 = items.iter().map(|i| i.1).sum();
    let mut order: Vec<usize> = (0..items.len()).collect();
    if largest {
        order.sort_by(|&a, &b| {
            items[b]
                .1
                .cmp(&items[a].1)
                .then(items[a].0.cmp(&items[b].0))
        });
    } else {
        order.sort_by(|&a, &b| {
            items[a]
                .0
                .cmp(&items[b].0)
                .then(items[b].1.cmp(&items[a].1))
        });
    }
    let mut out = Vec::new();
    for i in order {
        if total <= max {
            break;
        }
        total -= items[i].1;
        out.push(i);
    }
    out
}

/// Indices of `rows` (original path, deletion time, bytes) a put's trim
/// removes to bring them under `max`. Rows whose original is in `moved`
/// (what this put trashed) count toward the total but never go.
pub fn trim_after_put(
    rows: &[(PathBuf, i64, u64)],
    moved: &[PathBuf],
    max: u64,
    policy: Policy,
) -> Vec<usize> {
    let mine = |p: &Path| moved.iter().any(|m| m == p);
    let kept: u64 = rows.iter().filter(|r| mine(&r.0)).map(|r| r.2).sum();
    let rest: Vec<usize> = (0..rows.len()).filter(|&i| !mine(&rows[i].0)).collect();
    let sized: Vec<(i64, u64)> = rest.iter().map(|&i| (rows[i].1, rows[i].2)).collect();
    pick_budget(&sized, max.saturating_sub(kept), policy == Policy::Largest)
        .into_iter()
        .map(|i| rest[i])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_put_trim_spares_what_the_put_moved_but_counts_it() {
        let p = |s: &str| PathBuf::from(s);
        let rows = vec![
            (p("/a/old"), 10, 50),
            (p("/a/new"), 30, 50),
            (p("/a/mine"), 5, 60),
            (p("/a/mid"), 20, 10),
        ];
        let moved = vec![p("/a/mine")];
        // 170 in all, budget 100: mine (60, oldest) stays, so the other
        // three must come down to 40: old goes, then mid, then new.
        assert_eq!(
            trim_after_put(&rows, &moved, 100, Policy::Oldest),
            vec![0, 3, 1]
        );
        // Budget 120 leaves room for 60 besides mine: old alone is enough.
        assert_eq!(trim_after_put(&rows, &moved, 120, Policy::Oldest), vec![0]);
        // Largest first takes one 50 (the older), never the 60 of mine.
        assert_eq!(trim_after_put(&rows, &moved, 120, Policy::Largest), vec![0]);
        // Mine alone over budget: everything else goes, mine stays.
        assert_eq!(
            trim_after_put(&rows, &moved, 10, Policy::Oldest),
            vec![0, 3, 1]
        );
        assert!(trim_after_put(&rows, &moved, 1000, Policy::Oldest).is_empty());
        assert!(trim_after_put(&rows, &[], 1000, Policy::Oldest).is_empty());
    }

    #[test]
    fn pick_budget_trims_to_the_size_oldest_or_largest_first() {
        let items = [(30, 5), (10, 4), (20, 100), (40, 1)];
        assert_eq!(pick_budget(&items, 200, false), Vec::<usize>::new());
        assert_eq!(pick_budget(&items, 10, false), vec![1, 2]);
        assert_eq!(pick_budget(&items, 10, true), vec![2]);
        assert_eq!(pick_budget(&items, 0, true), vec![2, 0, 1, 3]);
    }
}
