//! Matching rules shared by the FreeDesktop, Finder and Recycle Bin `rm`.

use std::path::PathBuf;

struct MatchArg {
    shown: String,
    texts: Vec<String>,
    regex: Vec<regex_lite::Regex>,
    bare: bool,
    absolute_literal: bool,
}

pub(crate) struct Matcher {
    kind: crate::filters::Match,
    args: Vec<MatchArg>,
    basename: bool,
    mass: bool,
}

impl Matcher {
    pub(crate) fn new(
        patterns: &[String],
        aliases: Vec<Vec<String>>,
        kind: crate::filters::Match,
        basename: bool,
    ) -> Result<Self, String> {
        let all = crate::filters::Patterns::new(patterns.to_vec(), kind)?;
        let mut args = Vec::with_capacity(patterns.len());
        for (shown, texts) in patterns.iter().cloned().zip(aliases) {
            let regex = if kind == crate::filters::Match::Regex {
                texts
                    .iter()
                    .map(|p| {
                        regex_lite::Regex::new(p).map_err(|e| format!("invalid regex '{p}': {e}"))
                    })
                    .collect::<Result<_, _>>()?
            } else {
                Vec::new()
            };
            let bare = !shown.contains(['/', '\\']);
            let absolute_literal = kind == crate::filters::Match::Glob
                && crate::util::arg_path(&shown).is_absolute()
                && !shown.contains(['*', '?', '[']);
            args.push(MatchArg {
                shown,
                texts,
                regex,
                bare,
                absolute_literal,
            });
        }
        Ok(Self {
            kind,
            args,
            basename,
            mass: all.is_mass(),
        })
    }

    fn automatic_basename(&self, arg: &MatchArg) -> bool {
        arg.bare
            && matches!(
                self.kind,
                crate::filters::Match::Glob | crate::filters::Match::Exact
            )
    }

    fn target(&self, arg: &MatchArg, text: &str) -> bool {
        match self.kind {
            crate::filters::Match::Glob => arg.texts.iter().any(|p| crate::util::fnmatch(p, text)),
            crate::filters::Match::Regex => arg.regex.iter().any(|r| r.is_match(text)),
            crate::filters::Match::Substring => arg.texts.iter().any(|p| text.contains(p)),
            crate::filters::Match::Exact => arg.texts.iter().any(|p| p == text),
        }
    }

    fn base_target(&self, arg: &MatchArg, base: &str, name: &str) -> bool {
        (self.basename || self.automatic_basename(arg))
            && (self.target(arg, base) || self.target(arg, name))
    }

    pub(crate) fn hit(&self, full: &str, base: &str, name: &str) -> bool {
        self.args.iter().any(|arg| {
            let full_hit = if arg.absolute_literal {
                arg.texts.iter().any(|p| p == full)
            } else {
                self.target(arg, full)
            };
            full_hit || self.base_target(arg, base, name)
        })
    }

    #[cfg(unix)]
    fn target_nfc(&self, arg: &MatchArg, text: &str) -> bool {
        match self.kind {
            crate::filters::Match::Glob => arg
                .texts
                .iter()
                .any(|pattern| crate::util::fnmatch(&crate::finder_shared::nfc(pattern), text)),
            crate::filters::Match::Regex => arg.texts.iter().any(|pattern| {
                regex_lite::Regex::new(&crate::finder_shared::nfc(pattern))
                    .is_ok_and(|regex| regex.is_match(text))
            }),
            crate::filters::Match::Substring => arg
                .texts
                .iter()
                .any(|pattern| text.contains(&crate::finder_shared::nfc(pattern))),
            crate::filters::Match::Exact => arg
                .texts
                .iter()
                .any(|pattern| crate::finder_shared::nfc(pattern) == text),
        }
    }

    #[cfg(unix)]
    fn base_target_nfc(&self, arg: &MatchArg, base: &str, name: &str) -> bool {
        (self.basename || self.automatic_basename(arg))
            && (self.target_nfc(arg, base) || self.target_nfc(arg, name))
    }

    #[cfg(unix)]
    pub(crate) fn hit_nfc(&self, full: &str, base: &str, name: &str) -> bool {
        self.args.iter().any(|arg| {
            let full_hit = if arg.absolute_literal {
                arg.texts
                    .iter()
                    .any(|pattern| crate::finder_shared::nfc(pattern) == full)
            } else {
                self.target_nfc(arg, full)
            };
            full_hit || self.base_target_nfc(arg, base, name)
        })
    }

    #[cfg(unix)]
    pub(crate) fn ambiguous_nfc(
        &self,
        entries: &[(String, String, String, PathBuf)],
    ) -> Option<(&str, Vec<PathBuf>)> {
        for arg in &self.args {
            if !(self.basename || self.automatic_basename(arg)) {
                continue;
            }
            let paths: Vec<PathBuf> = entries
                .iter()
                .filter(|(_, base, name, _)| {
                    self.base_target_nfc(arg, base.as_str(), name.as_str())
                })
                .map(|(_, _, _, path)| path.clone())
                .collect();
            if paths.len() > 1 {
                return Some((&arg.shown, paths));
            }
        }
        None
    }

    pub(crate) fn is_mass(&self) -> bool {
        self.mass
    }

    pub(crate) fn ambiguous(
        &self,
        entries: &[(String, String, String, PathBuf)],
    ) -> Option<(&str, Vec<PathBuf>)> {
        for arg in &self.args {
            if !(self.basename || self.automatic_basename(arg)) {
                continue;
            }
            let paths: Vec<PathBuf> = entries
                .iter()
                .filter(|(_, base, name, _)| self.base_target(arg, base.as_str(), name.as_str()))
                .map(|(_, _, _, path)| path.clone())
                .collect();
            if paths.len() > 1 {
                return Some((&arg.shown, paths));
            }
        }
        None
    }
}
