//! Path lists for `put --files-from` and `du --files-from`.
//!
//! One reader splits a file or stdin into paths. Newline lists skip blank
//! lines and a trailing CR. NUL lists keep every byte between separators.
//! On Unix a path keeps bytes that are not UTF-8.

use std::io;
use std::path::PathBuf;

/// Paths from FILE, or from stdin when `src` is `-`.
///
/// `nul` selects NUL separators (`du --files0-from`, `put --zero`).
/// Otherwise each line is one path: a blank line is skipped and a trailing
/// CR is dropped, so a CRLF list works. put and du both call this.
pub(crate) fn read_path_list(src: &str, nul: bool) -> io::Result<Vec<PathBuf>> {
    let bytes = if src == "-" {
        let mut bytes = Vec::new();
        io::Read::read_to_end(&mut io::stdin(), &mut bytes)?;
        bytes
    } else {
        fs_read(src)?
    };
    Ok(split_paths(&bytes, nul))
}

fn fs_read(src: &str) -> io::Result<Vec<u8>> {
    std::fs::read(crate::util::arg_path(src))
}

fn split_paths(bytes: &[u8], nul: bool) -> Vec<PathBuf> {
    let sep = if nul { b'\0' } else { b'\n' };
    let mut out = Vec::new();
    for mut part in bytes.split(|&b| b == sep) {
        if !nul {
            part = part.strip_suffix(b"\r").unwrap_or(part);
        }
        if part.is_empty() {
            continue;
        }
        out.push(path_bytes(part));
    }
    out
}

#[cfg(unix)]
fn path_bytes(part: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(part))
}

#[cfg(not(unix))]
fn path_bytes(part: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(part).into_owned())
}

/// put's `--files-from` list. `--zero` / `-0` selects NUL records.
/// du spells that form `--files0-from` and does not use this type.
#[derive(Default)]
pub(crate) struct FilesFrom {
    src: Option<String>,
    nul: bool,
}

/// Why a put path list could not be read.
pub(crate) enum FilesFromError {
    /// The flags themselves are wrong. Exit 2.
    Usage(&'static str),
    /// FILE could not be read. Exit 1.
    Read { src: String, err: io::Error },
}

impl FilesFromError {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Usage(msg) => (*msg).to_string(),
            Self::Read { src, err } => format!(
                "cannot read --files-from '{}': {err}",
                crate::util::err_path(src)
            ),
        }
    }
}

impl FilesFrom {
    /// `true` when `args[*i]` is a list flag: `--files-from`, `--files0-from`
    /// (either `=PATH` or the next word), `-0` or `--zero`. A bare
    /// `--files-from` or `--files0-from` leaves `i` on the path.
    /// A short cluster such as `-rf0` is not this flag; the caller handles `0`.
    pub(crate) fn take_flag(
        &mut self,
        args: &[String],
        i: &mut usize,
    ) -> Result<bool, &'static str> {
        let Some(arg) = args.get(*i).cloned() else {
            return Ok(false);
        };
        // A bare `-0` is this flag. `-0f` and `-f0` are clusters, so the
        // equality has to stay: the caller's short-option loop never sees
        // an argument this already consumed.
        if arg == "-0" || arg == "--zero" {
            self.nul = true;
            return Ok(true);
        }
        if let Some(rest) = arg.strip_prefix("--files0-from=") {
            return self.store_list(rest, true);
        }
        if let Some(rest) = arg.strip_prefix("--files-from=") {
            return self.store_list(rest, false);
        }
        if arg == "--files-from" || arg == "--files0-from" {
            if self.src.is_some() {
                return Err("only one --files-from is allowed");
            }
            let nul = arg == "--files0-from";
            *i += 1;
            let Some(path) = args.get(*i) else {
                return Err(if nul {
                    "--files0-from requires a value"
                } else {
                    "--files-from requires a value"
                });
            };
            self.src = Some(path.clone());
            if nul {
                self.nul = true;
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn store_list(&mut self, path: &str, nul: bool) -> Result<bool, &'static str> {
        if self.src.is_some() {
            return Err("only one --files-from is allowed");
        }
        if nul {
            self.nul = true;
        }
        self.src = Some(path.to_string());
        Ok(true)
    }

    /// `0` in a short cluster (`-rf0`, `-0f`, `-f0`). A bare `-0` is
    /// `take_flag`, not this.
    pub(crate) fn set_nul(&mut self) {
        self.nul = true;
    }

    /// `--files-from=-` and a prompt both read stdin. `None` when the
    /// list is a file, or when this run does not prompt.
    pub(crate) fn refuse_stdin_prompt(&self, prompts: bool) -> Option<&'static str> {
        if prompts && self.src.as_deref() == Some("-") {
            Some("--files-from=- and a prompt both read standard input")
        } else {
            None
        }
    }

    /// The listed paths. Empty when no `--files-from` was given.
    /// `--zero` without `--files-from` is an error.
    pub(crate) fn load(&self) -> Result<Vec<PathBuf>, FilesFromError> {
        if self.nul && self.src.is_none() {
            return Err(FilesFromError::Usage("--zero requires --files-from"));
        }
        let Some(src) = &self.src else {
            return Ok(Vec::new());
        };
        match read_path_list(src, self.nul) {
            Ok(paths) => Ok(paths),
            Err(err) => Err(FilesFromError::Read {
                src: src.clone(),
                err,
            }),
        }
    }

    /// A `--files-from` was given, even when the file listed nothing.
    /// Only the Unix put browser asks: an empty list must not open it.
    /// Windows put has no browser. The cfg keeps the method for that
    /// caller and for the tests.
    #[cfg(any(unix, test))]
    pub(crate) fn is_set(&self) -> bool {
        self.src.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_skip_blanks_and_a_trailing_cr() {
        assert_eq!(
            split_paths(b"a\r\n\nb\n", false),
            vec![PathBuf::from("a"), PathBuf::from("b")]
        );
    }

    #[test]
    fn nul_records_keep_an_interior_cr_and_skip_empty() {
        assert_eq!(
            split_paths(b"a\r\0\0b\0", true),
            vec![PathBuf::from("a\r"), PathBuf::from("b")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn bytes_that_are_not_utf8_stay_on_unix() {
        use std::os::unix::ffi::OsStrExt;
        let got = split_paths(b"x\xff\n", false);
        assert_eq!(got[0].as_os_str().as_bytes(), b"x\xff");
    }

    #[cfg(not(unix))]
    #[test]
    fn bytes_that_are_not_utf8_are_replaced_off_unix() {
        let got = split_paths(b"x\xff\n", false);
        assert_eq!(got, vec![PathBuf::from("x\u{FFFD}")]);
    }

    #[test]
    fn the_separate_form_reads_the_next_word() {
        let mut list = FilesFrom::default();
        let args = vec!["--files-from".into(), "names".into(), "extra".into()];
        let mut i = 0;
        assert_eq!(list.take_flag(&args, &mut i), Ok(true));
        assert_eq!(i, 1);
        assert!(list.is_set());
    }

    #[test]
    fn a_second_list_is_refused_before_its_value_is_taken() {
        let mut list = FilesFrom::default();
        let args = vec!["--files-from=a".into(), "--files-from".into(), "b".into()];
        let mut i = 0;
        assert_eq!(list.take_flag(&args, &mut i), Ok(true));
        i = 1;
        assert_eq!(
            list.take_flag(&args, &mut i),
            Err("only one --files-from is allowed")
        );
        assert_eq!(i, 1);
    }

    #[test]
    fn files_from_needs_a_value() {
        let mut list = FilesFrom::default();
        let args = vec!["--files-from".into()];
        let mut i = 0;
        assert_eq!(
            list.take_flag(&args, &mut i),
            Err("--files-from requires a value")
        );
    }

    #[test]
    fn zero_alone_is_refused_when_the_list_is_read() {
        let mut list = FilesFrom::default();
        let args = vec!["--zero".into()];
        let mut i = 0;
        assert_eq!(list.take_flag(&args, &mut i), Ok(true));
        assert!(matches!(
            list.load(),
            Err(FilesFromError::Usage("--zero requires --files-from"))
        ));
    }

    #[test]
    fn a_missing_list_is_a_read_error() {
        let mut list = FilesFrom::default();
        let args = vec!["--files-from=/no/such/rtrash-files-from-list".into()];
        let mut i = 0;
        assert_eq!(list.take_flag(&args, &mut i), Ok(true));
        assert!(matches!(list.load(), Err(FilesFromError::Read { .. })));
    }

    /// Deleting `arg == "-0"` in `take_flag` makes this fail. The short-option
    /// cluster also accepts `0`, so a put of bare `-0` would still work.
    #[test]
    fn dash_zero_is_taken_as_its_own_argument() {
        let mut list = FilesFrom::default();
        let args = vec!["-0".into()];
        let mut i = 0;
        assert_eq!(list.take_flag(&args, &mut i), Ok(true));
        assert_eq!(i, 0);
        assert!(matches!(
            list.load(),
            Err(FilesFromError::Usage("--zero requires --files-from"))
        ));
    }

    #[test]
    fn a_short_cluster_is_not_a_files_from_flag() {
        let mut list = FilesFrom::default();
        for arg in ["-0f", "-f0", "-rf0"] {
            let args = vec![arg.into()];
            let mut i = 0;
            assert_eq!(list.take_flag(&args, &mut i), Ok(false), "{arg}");
        }
    }

    #[test]
    fn files0_from_is_one_nul_list() {
        let mut list = FilesFrom::default();
        let args = vec!["--files0-from".into(), "names".into()];
        let mut i = 0;
        assert_eq!(list.take_flag(&args, &mut i), Ok(true));
        assert_eq!(i, 1);
        assert!(list.is_set());
        let again = vec!["--files0-from=other".into()];
        let mut j = 0;
        assert_eq!(
            list.take_flag(&again, &mut j),
            Err("only one --files-from is allowed")
        );
        let mut bare = FilesFrom::default();
        let args = vec!["--files0-from".into()];
        let mut i = 0;
        assert_eq!(
            bare.take_flag(&args, &mut i),
            Err("--files0-from requires a value")
        );
    }

    #[test]
    fn a_prompt_cannot_share_stdin_with_the_list() {
        let mut list = FilesFrom::default();
        let args = vec!["--files-from=-".into()];
        let mut i = 0;
        assert_eq!(list.take_flag(&args, &mut i), Ok(true));
        assert_eq!(
            list.refuse_stdin_prompt(true),
            Some("--files-from=- and a prompt both read standard input")
        );
        assert_eq!(list.refuse_stdin_prompt(false), None);

        let mut file = FilesFrom::default();
        let args = vec!["--files-from=names".into()];
        let mut i = 0;
        assert_eq!(file.take_flag(&args, &mut i), Ok(true));
        assert_eq!(file.refuse_stdin_prompt(true), None);
    }
}
