//! Deterministic fuzz loops over every parser that reads files other tools
//! write: `.trashinfo`, `directorysizes`, `.DS_Store`, Recycle Bin `$I`,
//! du snapshots, ncdu exports (JSON, zstd JSON and binary), the config file and
//! exclude patterns. Each loop mutates a
//! valid seed (bit flips, byte overwrites, truncation, splices) and also
//! feeds pure noise. A parser may reject input; it must never panic. The
//! generator is a fixed-seed xorshift, so a failure replays exactly.

use std::path::Path;

/// Rounds per parser. `RTRASH_FUZZ_ROUNDS` raises it for a longer run.
fn rounds() -> usize {
    std::env::var("RTRASH_FUZZ_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4000)
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }

    fn bytes(&mut self, max: usize) -> Vec<u8> {
        let n = self.below(max + 1);
        (0..n).map(|_| self.next() as u8).collect()
    }

    /// A mutated copy of `seed`.
    fn mutate(&mut self, seed: &[u8]) -> Vec<u8> {
        let mut v = seed.to_vec();
        for _ in 0..1 + self.below(8) {
            match self.below(6) {
                0 if !v.is_empty() => {
                    let i = self.below(v.len());
                    v[i] ^= 1 << self.below(8);
                }
                1 if !v.is_empty() => {
                    let i = self.below(v.len());
                    v[i] = self.next() as u8;
                }
                2 => v.truncate(self.below(v.len() + 1)),
                3 => {
                    let i = self.below(v.len() + 1);
                    let extra = self.bytes(16);
                    v.splice(i..i, extra);
                }
                4 if !v.is_empty() => {
                    // Interesting integers: 0, all ones, sign bits.
                    let i = self.below(v.len());
                    let b = [0x00, 0xff, 0x7f, 0x80][self.below(4)];
                    for x in v.iter_mut().skip(i).take(4) {
                        *x = b;
                    }
                }
                _ => {
                    let i = self.below(v.len() + 1);
                    let j = self.below(v.len() + 1);
                    let (a, b) = (i.min(j), i.max(j));
                    let chunk = v[a..b].to_vec();
                    let at = self.below(v.len() + 1);
                    v.splice(at..at, chunk);
                }
            }
        }
        v
    }

    /// Mutated seed half the time, noise the other half.
    fn input(&mut self, seed: &[u8]) -> Vec<u8> {
        if self.next().is_multiple_of(2) {
            self.mutate(seed)
        } else {
            self.bytes(seed.len() * 2 + 8)
        }
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[cfg(unix)]
#[test]
fn trashinfo_parser_survives_noise_and_keeps_raw_bytes() {
    use rtrash::info;
    use std::os::unix::ffi::OsStrExt;
    let seed = info::render_batch(
        Path::new("/home/u/a b%c.txt"),
        "2026-07-03T12:00:00",
        Some("1-2"),
    );
    let mut rng = Rng(0x7a5f_19c3_0b1e_44d1);
    for _ in 0..rounds() {
        let input = rng.input(seed.as_bytes());
        let parsed = info::parse_bytes(&input);
        if let Some(d) = parsed.deletion_date.as_deref() {
            let _ = info::parse_local_to_epoch(d);
        }
        // Text input parses the same either way.
        if let Ok(s) = std::str::from_utf8(&input) {
            let p = info::parse(s);
            assert_eq!(p.path, parsed.path);
            assert_eq!(p.deletion_date, parsed.deletion_date);
        }
    }
    // Any byte path survives render then parse.
    for _ in 0..rounds() {
        let mut raw = rng.bytes(40);
        raw.retain(|&b| b != 0);
        raw.insert(0, b'/');
        let p = std::ffi::OsStr::from_bytes(&raw);
        let body = info::render(Path::new(p), "2026-07-03T12:00:00");
        assert_eq!(info::parse(&body).path.as_os_str().as_bytes(), &raw[..]);
    }
}

#[cfg(unix)]
#[test]
fn trashinfo_with_raw_path_bytes_keeps_them() {
    use std::os::unix::ffi::OsStrExt;
    let body = b"[Trash Info]\r\nPath=/w/name\xe9 x\r\nDeletionDate= 2026-07-03T12:00:00 \r\n";
    let p = rtrash::info::parse_bytes(body);
    assert_eq!(p.path.as_os_str().as_bytes(), b"/w/name\xe9 x");
    assert_eq!(p.deletion_date.as_deref(), Some("2026-07-03T12:00:00"));
}

#[cfg(unix)]
#[test]
fn date_parser_survives_noise() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let seed = b"2024-02-29T23:59:60";
    for _ in 0..rounds() {
        let _ = rtrash::info::parse_local_to_epoch(&text(&rng.input(seed)));
    }
}

#[cfg(unix)]
#[test]
fn directorysizes_lines_survive_noise() {
    let mut rng = Rng(0x0dd5_eed5_1234_9876);
    let seed = b"8192 1791660998 d%FF%20x";
    for _ in 0..rounds() {
        let line = text(&rng.input(seed));
        if let Some((_, _, enc)) = rtrash::trashdir::directorysizes_parse_line(&line) {
            assert!(!enc.is_empty());
            let _ = rtrash::util::url_decode(&enc);
        }
    }
}

#[test]
fn ds_store_parser_survives_noise() {
    use rtrash::ds_store::{self, Record, Value};
    let records = vec![
        Record {
            name: "a.txt".into(),
            code: *b"ptbL",
            value: Value::Ustr("Users/ann/Desktop/".into()),
        },
        Record {
            name: "a.txt".into(),
            code: *b"ptbN",
            value: Value::Ustr("a.txt".into()),
        },
        Record {
            name: "b".into(),
            code: *b"moDD",
            value: Value::Dutc(7),
        },
        Record {
            name: "c".into(),
            code: *b"icvp",
            value: Value::Blob(vec![1, 2, 3]),
        },
    ];
    let seed = ds_store::write(&records);
    assert_eq!(ds_store::parse(&seed).unwrap(), records);
    let mut rng = Rng(0xd5d5_0000_beef_0001);
    for _ in 0..rounds() {
        let input = rng.mutate(&seed);
        if let Ok(recs) = ds_store::parse(&input) {
            let _ = ds_store::put_back(&recs);
        }
        let _ = ds_store::parse(&rng.bytes(64));
    }
}

#[test]
fn recycle_bin_index_parser_survives_noise() {
    use rtrash::win_recycle::{encode_recycle_i_v2, filetime_to_unix_epoch, parse_recycle_i};
    let seed = encode_recycle_i_v2(Path::new(r"C:\Users\me\a.txt"), 42, 133_000_000_000_000_000);
    assert!(parse_recycle_i(&seed).is_some());
    let mut rng = Rng(0x5eed_1111_2222_3333);
    for _ in 0..rounds() {
        if let Some(m) = parse_recycle_i(&rng.input(&seed)) {
            let _ = filetime_to_unix_epoch(m.delete_filetime);
        }
    }
}

#[test]
fn du_snapshot_parser_survives_noise() {
    let seed = b"# rtrash du snapshot 1\ncreated 1700000000\nmax_depth -\none_file_system 0\n\
root 4096 10 1 1 /w\ndir 4096 10 1700000000 - /w\ndir 8 8 -5 cargo /w/t%20x\n";
    let mut rng = Rng(0x0005_7a95_407d_0001);
    for _ in 0..rounds() {
        let input = rng.mutate(seed);
        let _ = rtrash::du_snap::parse(&input[..]);
    }
}

#[test]
fn ncdu_json_export_parser_survives_noise() {
    let seed = include_bytes!("data/ncdu-2.9.1-export.json");
    let mut rng = Rng(0x0005_7a95_407d_0002);
    for _ in 0..rounds() {
        let input = rng.input(seed);
        let _ = rtrash::du_ncdu::parse(&input[..]);
    }
}

/// The zstd reader under `ncdu -c` exports: noise in the frames, and
/// in the JSON behind them.
#[test]
fn ncdu_zstd_export_parser_survives_noise() {
    let json = include_bytes!("data/ncdu-2.9.1-export.json");
    let seed = include_bytes!("data/ncdu-2.9.1-export.json.zst");
    let mut rng = Rng(0x0005_7a95_407d_0004);
    for _ in 0..rounds() {
        let input = if rng.below(2) == 0 {
            rng.input(seed)
        } else {
            ruzstd::encoding::compress_to_vec(
                &rng.input(json)[..],
                ruzstd::encoding::CompressionLevel::Fastest,
            )
        };
        let _ = rtrash::du_ncdu::parse_zstd(&input[..]);
    }
}

#[test]
fn ncdu_binary_export_parser_survives_noise() {
    let seed = include_bytes!("data/ncdu-2.9.1-export.ncdu");
    let mut rng = Rng(0x0005_7a95_407d_0003);
    for _ in 0..rounds() {
        let mut input = rng.input(seed);
        // Keep the signature on most rounds, so the noise reaches the
        // blocks behind it.
        if rng.below(4) != 0 && input.len() >= 8 {
            input[..8].copy_from_slice(rtrash::du_ncdu_bin::SIGNATURE);
        }
        let _ = rtrash::du_ncdu_bin::parse(std::io::Cursor::new(input), 0);
    }
}

#[test]
fn config_parser_survives_noise() {
    let seed = rtrash::config::template();
    let mut rng = Rng(0x00c0_4f19_0000_0007);
    for _ in 0..rounds() / 4 {
        let input = text(&rng.input(seed.as_bytes()));
        let c = rtrash::config::Config::parse(&input, None);
        let _ = c.str("du", "sort");
        let _ = c.int("du", "top");
        let _ = c.bool("list", "reverse");
        let _ = c.list("protect", "paths");
    }
}

#[test]
fn exclude_patterns_survive_noise() {
    use rtrash::du_filter::{seg_match, Pattern};
    let mut rng = Rng(0x9a77_e4f5_0000_0003);
    let alphabet = b"ab*?[]!^-\\/.";
    let pick = |rng: &mut Rng, n: usize| -> Vec<u8> {
        (0..rng.below(n + 1))
            .map(|_| alphabet[rng.below(alphabet.len())])
            .collect()
    };
    for _ in 0..rounds() {
        let pat = pick(&mut rng, 12);
        let name = pick(&mut rng, 12);
        let _ = seg_match(&pat, &name);
        let _ = Pattern::parse(&text(&pat));
        let _ = rtrash::util::fnmatch(&text(&pat), &text(&name));
    }
    // Star runs must not go exponential.
    let t = std::time::Instant::now();
    let pat = "*a".repeat(20) + "b";
    assert!(!seg_match(pat.as_bytes(), "a".repeat(60).as_bytes()));
    assert!(!rtrash::util::fnmatch(&pat, &"a".repeat(60)));
    assert!(t.elapsed().as_secs() < 5, "{:?}", t.elapsed());
}

#[cfg(unix)]
#[test]
fn byte_names_round_trip_through_strings() {
    use rtrash::util::{os_from_str, show_name, str_from_os, str_from_os_ref};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let mut rng = Rng(0xb17e_5000_0000_0009);
    for _ in 0..rounds() {
        let raw = rng.bytes(24);
        let os = std::ffi::OsString::from_vec(raw.clone());
        let s = str_from_os(os.clone());
        assert_eq!(s, str_from_os_ref(&os));
        assert_eq!(os_from_str(&s).as_bytes(), &raw[..]);
        assert_eq!(show_name(&s), os.to_string_lossy());
        if let Ok(valid) = std::str::from_utf8(&raw) {
            assert_eq!(s, valid);
        }
    }
}

/// Property: a record's file name never maps to an item whose payload is
/// `files/` itself or the trash root, and a relative `Path=` in a volume
/// trash, once decoded, never resolves outside the volume.
#[cfg(unix)]
#[test]
fn record_names_and_paths_stay_inside_their_trash() {
    use rtrash::trashdir::TrashDir;
    use std::path::Component;
    let mut rng = Rng(0x0dd5_7a1e_2026_1011);
    let bits = ["", ".", "..", "...", "a", "/", ".trashinfo", "%2E", " "];
    for _ in 0..rounds() {
        let mut name: String = (0..rng.below(4))
            .map(|_| bits[rng.below(bits.len())])
            .collect();
        name.push_str(".trashinfo");
        if let Some(item) = rtrash::util::trash_item_name(&name) {
            assert!(!matches!(item, "" | "." | ".."), "{name:?} -> {item:?}");
            assert!(!item.contains('/'), "{name:?} -> {item:?}");
        }
    }
    let volume = TrashDir {
        root: "/mnt/v/.Trash-1000".into(),
        topdir: Some("/mnt/v".into()),
    };
    let parts: [&[u8]; 10] = [
        b"..", b".", b"", b"a", b"%2E%2E", b"%2F", b"%2F..", b"%00", b"%zz", b"b c",
    ];
    for _ in 0..rounds() {
        let mut raw = Vec::new();
        if rng.below(3) == 0 {
            raw.push(b'/');
        }
        for i in 0..rng.below(5) {
            if i > 0 {
                raw.push(b'/');
            }
            raw.extend_from_slice(parts[rng.below(parts.len())]);
        }
        let body = [
            b"[Trash Info]\nPath=".as_slice(),
            &raw,
            b"\nDeletionDate=2026-01-01T00:00:00\n",
        ]
        .concat();
        let recorded = rtrash::info::parse_bytes(&body).path;
        let Ok(p) = volume.resolve_original_checked(&recorded) else {
            continue;
        };
        // Absolute or not, what a volume trash restores stays inside it.
        assert!(p.starts_with("/mnt/v"), "{recorded:?} -> {p:?}");
        if !recorded.is_absolute() {
            assert!(
                !p.components().any(|c| c == Component::ParentDir),
                "{recorded:?} -> {p:?}"
            );
        }
    }
}
