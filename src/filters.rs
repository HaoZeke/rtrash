//! Listing filters shared by the FreeDesktop and Recycle Bin backends:
//! `--older-than`/`--newer-than` on when an item was trashed, and
//! `--larger-than`/`--smaller-than` on how much it takes on disk.

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
        self.larger_than.map_or(true, |n| bytes > n)
            && self.smaller_than.map_or(true, |n| bytes < n)
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

/// `--older-than` and `--newer-than`, in days, on when an item was
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
        let cutoff = |days: i64| now - days.saturating_mul(86_400);
        let old = self
            .older_than
            .map_or(true, |d| epoch.is_some_and(|t| t <= cutoff(d)));
        let new = self
            .newer_than
            .map_or(true, |d| epoch.is_some_and(|t| t > cutoff(d)));
        old && new
    }

    /// [`AgeFilter::keeps_at`] as of now.
    pub fn keeps(&self, epoch: Option<i64>) -> bool {
        self.keeps_at(epoch, crate::util::now_epoch())
    }

    /// Take `--older-than` or `--newer-than` at `args[*i]`, as
    /// `--flag=DAYS` or `--flag DAYS` (then `*i` moves past the value).
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
                    None => return Some(Err(format!("{name} requires DAYS"))),
                }
            }
        };
        Some(parse_days(value).map(|d| *slot = Some(d)))
    }
}

/// A day count for an age filter: a whole number, zero or more.
pub fn parse_days(s: &str) -> Result<i64, String> {
    match s.parse::<i64>() {
        Ok(d) if d >= 0 => Ok(d),
        Ok(d) => Err(format!("DAYS must be >= 0 (got {d})")),
        Err(_) => Err(format!("invalid DAYS '{s}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            older_than: Some(30),
            newer_than: None,
        };
        assert!(old.keeps_at(Some(now - 30 * 86_400), now));
        assert!(!old.keeps_at(Some(now - 30 * 86_400 + 1), now));
        assert!(!old.keeps_at(None, now));
        let new = AgeFilter {
            older_than: None,
            newer_than: Some(1),
        };
        assert!(new.keeps_at(Some(now - 100), now));
        assert!(!new.keeps_at(Some(now - 86_400), now));
        assert!(!new.keeps_at(None, now));
        let both = AgeFilter {
            older_than: Some(1),
            newer_than: Some(7),
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
        assert_eq!((i, f.older_than, f.newer_than), (2, Some(3), Some(9)));
        i += 1;
        assert!(f.take(&args, &mut i).unwrap().unwrap_err().contains(">= 0"));
        i += 1;
        assert!(f
            .take(&args, &mut i)
            .unwrap()
            .unwrap_err()
            .contains("requires"));
        assert_eq!(f.take(&["--size".to_string()], &mut 0), None);
    }
}
