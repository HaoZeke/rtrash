//! Listing filters shared by the FreeDesktop and Recycle Bin backends:
//! `--older-than`/`--newer-than` on when an item was trashed, and
//! `--larger-than`/`--smaller-than` on how much it takes on disk.

/// The age and size flags restore reads in `--flag=VALUE` form.
const SPACED: [&str; 4] = [
    "--older-than",
    "--newer-than",
    "--larger-than",
    "--smaller-than",
];

/// Join `--older-than 30` and the other [`SPACED`] flags given as two
/// words into `--older-than=30`, as list takes them, so a parser that
/// reads only the `=` form sees both. Words after `--` stay as they are.
pub fn join_spaced(args: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(args.len());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            out.push(a.clone());
            out.extend(it.cloned());
            break;
        }
        if SPACED.contains(&a.as_str()) {
            let v = it.next().ok_or_else(|| format!("{a} requires a value"))?;
            out.push(format!("{a}={v}"));
        } else {
            out.push(a.clone());
        }
    }
    Ok(out)
}

/// Parse a size such as `500M`, `10G`, `1.5GiB`, `64k` or `123` (bytes).
/// Units are powers of 1024; a trailing `B` or `iB` is optional.
pub fn parse_size(s: &str) -> Option<u64> {
    let t = s.trim();
    let t = t
        .strip_suffix("iB")
        .or_else(|| t.strip_suffix('B'))
        .unwrap_or(t);
    let (num, mult) = match t.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => {
            let m: u64 = match c.to_ascii_uppercase() {
                'K' => 1 << 10,
                'M' => 1 << 20,
                'G' => 1 << 30,
                'T' => 1 << 40,
                _ => return None,
            };
            (&t[..i], m)
        }
        _ => (t, 1),
    };
    let v: f64 = num.trim().parse().ok()?;
    if !v.is_finite() || v < 0.0 {
        return None;
    }
    let bytes = v * mult as f64;
    (bytes <= u64::MAX as f64).then_some(bytes as u64)
}

/// `--larger-than` and `--smaller-than`: bounds, both exclusive, on an
/// item's disk usage (payload plus `.trashinfo`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SizeFilter {
    pub larger_than: Option<u64>,
    pub smaller_than: Option<u64>,
}

impl SizeFilter {
    pub fn is_set(&self) -> bool {
        self.larger_than.is_some() || self.smaller_than.is_some()
    }

    /// Whether an item of `bytes` passes both bounds.
    pub fn keeps(&self, bytes: u64) -> bool {
        self.larger_than.is_none_or(|n| bytes > n) && self.smaller_than.is_none_or(|n| bytes < n)
    }

    /// Take `--larger-than` or `--smaller-than` at `args[*i]`, as
    /// `--flag=SIZE` or `--flag SIZE` (then `*i` moves past the value).
    /// `None` when `args[*i]` is neither flag.
    pub fn take(&mut self, args: &[String], i: &mut usize) -> Option<Result<(), String>> {
        let arg = args[*i].as_str();
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (arg, None),
        };
        let slot = match name {
            "--larger-than" => &mut self.larger_than,
            "--smaller-than" => &mut self.smaller_than,
            _ => return None,
        };
        let value = match inline {
            Some(v) => v,
            None => {
                *i += 1;
                match args.get(*i) {
                    Some(v) => v.as_str(),
                    None => return Some(Err(format!("{name} requires SIZE"))),
                }
            }
        };
        Some(match parse_size(value) {
            Some(n) => {
                *slot = Some(n);
                Ok(())
            }
            None => Err(format!(
                "invalid SIZE '{value}' for {name} (e.g. 500M, 1.5G)"
            )),
        })
    }

    /// Bounds that no item can pass are a usage error.
    pub fn check(&self) -> Result<(), String> {
        match (self.larger_than, self.smaller_than) {
            (Some(l), Some(s)) if l.saturating_add(1) >= s => Err(format!(
                "--larger-than={l} and --smaller-than={s} leave no size between them"
            )),
            _ => Ok(()),
        }
    }
}

/// An age for `--older-than` and `--newer-than` in seconds: a bare
/// number is days (`7`), otherwise one or more NUMBER UNIT pairs with
/// units s, m (or min), h, d and w (`2h`, `30m`, `1d12h`).
pub fn parse_age_secs(s: &str) -> Result<i64, String> {
    let t = s.trim();
    let bad = || format!("invalid age '{s}' (want DAYS or a duration such as 2h, 30m or 1d12h)");
    if t.is_empty() {
        return Err(bad());
    }
    if t.bytes().all(|b| b.is_ascii_digit()) {
        return t
            .parse::<i64>()
            .ok()
            .and_then(|d| d.checked_mul(86_400))
            .ok_or_else(bad);
    }
    let mut total: i64 = 0;
    let mut rest = t;
    while !rest.is_empty() {
        let n_end = rest.find(|c: char| !c.is_ascii_digit()).ok_or_else(bad)?;
        if n_end == 0 {
            return Err(bad());
        }
        let n: i64 = rest[..n_end].parse().map_err(|_| bad())?;
        let after = &rest[n_end..];
        let u_end = after
            .find(|c: char| c.is_ascii_digit())
            .unwrap_or(after.len());
        let mul = match &after[..u_end] {
            "s" => 1,
            "m" | "min" => 60,
            "h" => 3600,
            "d" => 86_400,
            "w" => 7 * 86_400,
            _ => return Err(bad()),
        };
        total = n
            .checked_mul(mul)
            .and_then(|v| total.checked_add(v))
            .ok_or_else(bad)?;
        rest = &after[u_end..];
    }
    Ok(total)
}

/// `--older-than` and `--newer-than`, in seconds, on when an item was
/// trashed. The cutoff is `empty DAYS`'s: older keeps items trashed at or
/// before it, newer keeps items trashed after it, and both leave out an
/// item with no readable date, whose age is unknown (trash-cli's
/// `trash-empty DAYS` keeps such items too).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AgeFilter {
    pub older_than: Option<i64>,
    pub newer_than: Option<i64>,
}

impl AgeFilter {
    pub fn is_set(&self) -> bool {
        self.older_than.is_some() || self.newer_than.is_some()
    }

    /// Whether an item trashed at `epoch` (seconds, `None` when unknown)
    /// passes both bounds, as of `now`.
    pub fn keeps_at(&self, epoch: Option<i64>, now: i64) -> bool {
        let cutoff = |secs: i64| now.saturating_sub(secs);
        let old = self
            .older_than
            .is_none_or(|d| epoch.is_some_and(|t| t <= cutoff(d)));
        let new = self
            .newer_than
            .is_none_or(|d| epoch.is_some_and(|t| t > cutoff(d)));
        old && new
    }

    /// [`AgeFilter::keeps_at`] as of now.
    pub fn keeps(&self, epoch: Option<i64>) -> bool {
        self.keeps_at(epoch, crate::util::now_epoch())
    }

    /// Take `--older-than` or `--newer-than` at `args[*i]`, as
    /// `--flag=AGE` or `--flag AGE` (then `*i` moves past the value), AGE
    /// as [`parse_age_secs`] reads it.
    /// `None` when `args[*i]` is neither flag.
    pub fn take(&mut self, args: &[String], i: &mut usize) -> Option<Result<(), String>> {
        let arg = args[*i].as_str();
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (arg, None),
        };
        let slot = match name {
            "--older-than" => &mut self.older_than,
            "--newer-than" => &mut self.newer_than,
            _ => return None,
        };
        let value = match inline {
            Some(v) => v,
            None => {
                *i += 1;
                match args.get(*i) {
                    Some(v) => v.as_str(),
                    None => return Some(Err(format!("{name} requires an AGE"))),
                }
            }
        };
        Some(parse_age_secs(value).map(|d| *slot = Some(d)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spaced_flags_join_their_value() {
        let words = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let got = join_spaced(&words(&["--older-than", "30", "-f", "--larger-than", "1K"]));
        assert_eq!(
            got.unwrap(),
            words(&["--older-than=30", "-f", "--larger-than=1K"])
        );
        let kept = join_spaced(&words(&["--", "--newer-than", "2"])).unwrap();
        assert_eq!(kept, words(&["--", "--newer-than", "2"]));
        assert!(join_spaced(&words(&["--smaller-than"])).is_err());
    }

    #[test]
    fn ages_take_days_and_compound_durations() {
        assert_eq!(parse_age_secs("7"), Ok(7 * 86_400));
        assert_eq!(parse_age_secs("0"), Ok(0));
        assert_eq!(parse_age_secs("2h"), Ok(7200));
        assert_eq!(parse_age_secs("30m"), Ok(1800));
        assert_eq!(parse_age_secs("90min"), Ok(5400));
        assert_eq!(parse_age_secs("1d12h"), Ok(129_600));
        assert_eq!(parse_age_secs("1w"), Ok(604_800));
        for bad in ["", "h", "-1", "2x", "1.5h", "2h-", "99999999999999999999"] {
            assert!(parse_age_secs(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn size_filters_parse_both_forms_and_bound_strictly() {
        let args: Vec<String> = [
            "--larger-than=1K",
            "--smaller-than",
            "2k",
            "--size",
            "--larger-than",
        ]
        .map(String::from)
        .to_vec();
        let mut f = SizeFilter::default();
        let mut i = 0;
        assert_eq!(f.take(&args, &mut i), Some(Ok(())));
        i += 1;
        assert_eq!(f.take(&args, &mut i), Some(Ok(())));
        assert_eq!(i, 2);
        i += 1;
        assert_eq!(f.take(&args, &mut i), None);
        i += 1;
        assert!(f.take(&args, &mut i).unwrap().is_err());
        assert_eq!(
            f,
            SizeFilter {
                larger_than: Some(1024),
                smaller_than: Some(2048)
            }
        );
        assert!(!f.keeps(1024) && f.keeps(1025) && f.keeps(2047) && !f.keeps(2048));
        assert!(f.check().is_ok());
        let none = SizeFilter {
            larger_than: Some(5),
            smaller_than: Some(6),
        };
        assert!(none.check().is_err());
        assert!(SizeFilter::default().keeps(0) && !SizeFilter::default().is_set());
    }

    #[test]
    fn age_bounds_follow_the_empty_cutoff() {
        let now = 100 * 86_400;
        let old = AgeFilter {
            older_than: Some(30 * 86_400),
            newer_than: None,
        };
        assert!(old.keeps_at(Some(now - 30 * 86_400), now));
        assert!(!old.keeps_at(Some(now - 30 * 86_400 + 1), now));
        assert!(!old.keeps_at(None, now));
        let new = AgeFilter {
            older_than: None,
            newer_than: Some(86_400),
        };
        assert!(new.keeps_at(Some(now - 100), now));
        assert!(!new.keeps_at(Some(now - 86_400), now));
        assert!(!new.keeps_at(None, now));
        let both = AgeFilter {
            older_than: Some(86_400),
            newer_than: Some(7 * 86_400),
        };
        assert!(both.keeps_at(Some(now - 3 * 86_400), now));
        assert!(!both.keeps_at(Some(now - 10 * 86_400), now));
        assert!(AgeFilter::default().keeps_at(None, now) && !AgeFilter::default().is_set());
    }

    #[test]
    fn age_flags_parse_both_forms() {
        let args: Vec<String> = [
            "--older-than=3",
            "--newer-than",
            "9",
            "--newer-than=-1",
            "--older-than",
        ]
        .map(String::from)
        .to_vec();
        let mut f = AgeFilter::default();
        let mut i = 0;
        assert_eq!(f.take(&args, &mut i), Some(Ok(())));
        i += 1;
        assert_eq!(f.take(&args, &mut i), Some(Ok(())));
        assert_eq!(
            (i, f.older_than, f.newer_than),
            (2, Some(3 * 86_400), Some(9 * 86_400))
        );
        i += 1;
        assert!(f
            .take(&args, &mut i)
            .unwrap()
            .unwrap_err()
            .contains("invalid age"));
        i += 1;
        assert!(f
            .take(&args, &mut i)
            .unwrap()
            .unwrap_err()
            .contains("requires"));
        assert_eq!(f.take(&["--size".to_string()], &mut 0), None);
    }
}

/// How `rm` reads its PATTERNs: trashy's `--match` kinds. Globs are the
/// default here (trashy's default is regex).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Match {
    /// Shell glob on the original path, its basename or the trash name.
    #[default]
    Glob,
    /// Regular expression searched anywhere in the original path.
    Regex,
    /// Text found anywhere in the original path.
    Substring,
    /// The whole original path, character for character.
    Exact,
}

impl Match {
    pub fn parse(s: &str) -> Result<Match, String> {
        match s {
            "glob" => Ok(Match::Glob),
            "regex" => Ok(Match::Regex),
            "substring" => Ok(Match::Substring),
            "exact" => Ok(Match::Exact),
            _ => Err(format!(
                "invalid --match '{s}' (use glob, regex, substring or exact)"
            )),
        }
    }
}

/// The PATTERN operands of `rm`, read as `kind` says. `texts` keeps them
/// as given; `regex` holds them compiled when `kind` is [`Match::Regex`].
#[derive(Debug, Clone, Default)]
pub struct Patterns {
    pub kind: Match,
    pub texts: Vec<String>,
    pub regex: Vec<regex_lite::Regex>,
}

impl Patterns {
    pub fn new(texts: Vec<String>, kind: Match) -> Result<Patterns, String> {
        let regex = if kind == Match::Regex {
            texts
                .iter()
                .map(|p| regex_lite::Regex::new(p).map_err(|e| format!("invalid regex '{p}': {e}")))
                .collect::<Result<_, _>>()?
        } else {
            Vec::new()
        };
        Ok(Patterns { kind, texts, regex })
    }

    /// A pattern that takes everything: `*` and friends, a regex that
    /// matches the empty string (such as `.*` or `^`), or a substring
    /// every path contains (empty, or a lone separator).
    pub fn is_mass(&self) -> bool {
        match self.kind {
            Match::Glob => {
                let all = ["*", "*.*", "**", "**/*"];
                self.texts.iter().any(|p| all.contains(&p.as_str()))
            }
            Match::Regex => self.regex.iter().any(|r| r.is_match("")),
            Match::Substring => self
                .texts
                .iter()
                .any(|p| ["", "/", "\\"].contains(&p.as_str())),
            Match::Exact => false,
        }
    }

    pub fn hit(&self, full: &str, base: &str, name: &str) -> bool {
        match self.kind {
            Match::Glob => self.texts.iter().any(|p| {
                crate::util::fnmatch(p, full)
                    || crate::util::fnmatch(p, base)
                    || crate::util::fnmatch(p, name)
            }),
            Match::Regex => self.regex.iter().any(|r| r.is_match(full)),
            Match::Substring => self.texts.iter().any(|p| full.contains(p.as_str())),
            Match::Exact => self.texts.iter().any(|p| p == full),
        }
    }
}

/// What an `rm` run did, for `--json`.
pub struct RmSummary {
    pub dry_run: bool,
    pub removed: u64,
    pub bytes: u64,
}

impl RmSummary {
    /// The `rm --json` object: the counts and the matched originals.
    pub fn json(&self, matches: &[String]) -> String {
        let mut out = format!(
            "{{\n  \"dry_run\": {},\n  \"removed\": {},\n  \"bytes\": {},\n  \"matches\": [\n",
            self.dry_run, self.removed, self.bytes
        );
        for (i, path) in matches.iter().enumerate() {
            if i > 0 {
                out.push_str(",\n");
            }
            out.push_str(&format!("    \"{}\"", crate::util::json_escape(path)));
        }
        if !matches.is_empty() {
            out.push('\n');
        }
        out.push_str("  ]\n}\n");
        out
    }
}

#[cfg(test)]
mod pattern_tests {
    use super::{Match, Patterns};

    #[test]
    fn regex_patterns_search_the_whole_path() {
        let p = Patterns::new(vec![r"/old-[0-9]+/".into()], Match::Regex).unwrap();
        assert!(p.hit("/home/u/old-42/a.txt", "a.txt", "a.txt"));
        assert!(!p.hit("/home/u/old-x/a.txt", "a.txt", "a.txt"));
        assert!(!p.is_mass());
        assert!(Patterns::new(vec![".*".into()], Match::Regex)
            .unwrap()
            .is_mass());
        assert!(Patterns::new(vec!["(".into()], Match::Regex).is_err());
        let g = Patterns::new(vec!["*.o".into()], Match::Glob).unwrap();
        assert!(g.hit("/x/a.o", "a.o", "a.o"));
        assert!(!g.is_mass());
    }

    #[test]
    fn substring_and_exact_match_the_original_path() {
        let sub = Patterns::new(vec!["old-pro".into()], Match::Substring).unwrap();
        assert!(sub.hit("/home/u/old-project/a", "a", "a"));
        assert!(!sub.hit("/home/u/new/a", "a", "a"));
        assert!(Patterns::new(vec!["/".into()], Match::Substring)
            .unwrap()
            .is_mass());
        let ex = Patterns::new(vec!["/home/u/a".into()], Match::Exact).unwrap();
        assert!(ex.hit("/home/u/a", "a", "a"));
        assert!(!ex.hit("/home/u/a2", "a2", "a2"));
        assert!(!ex.hit("/x/a", "a", "a"));
        assert_eq!(Match::parse("exact"), Ok(Match::Exact));
        assert!(Match::parse("fuzzy").is_err());
    }

    #[test]
    fn rm_summary_json_lists_the_matches() {
        let sum = super::RmSummary {
            dry_run: true,
            removed: 1,
            bytes: 4096,
        };
        let j = sum.json(&["C:\\a \"b\".txt".into()]);
        assert!(j.contains("\"dry_run\": true"), "{j}");
        assert!(j.contains("\"bytes\": 4096"), "{j}");
        assert!(j.contains(r#""C:\\a \"b\".txt""#), "{j}");
        let none = super::RmSummary {
            dry_run: false,
            removed: 0,
            bytes: 0,
        };
        assert!(none.json(&[]).ends_with("\"matches\": [\n  ]\n}\n"));
    }
}
