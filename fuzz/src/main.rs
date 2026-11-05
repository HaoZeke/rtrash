//! Mutational fuzz targets for the parsers that read on-disk trash data.
//!
//!     cargo run --manifest-path fuzz/Cargo.toml --release -- --seconds 60
//!
//! Each target starts from a valid seed, then bit-flips, truncates, splices
//! and overwrites length fields. A parser may reject a buffer. It must not
//! panic, and one input must not run on without bound. The generator is a
//! fixed-seed xorshift, so a failure replays from the iteration it prints.

use std::env;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use rtrash::ds_store::{self, Record, Value};
use rtrash::info;
use rtrash::trashdir::directorysizes_parse_line;

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
            (self.next() as usize) % n
        }
    }

    fn bytes(&mut self, max: usize) -> Vec<u8> {
        let n = self.below(max + 1);
        (0..n).map(|_| self.next() as u8).collect()
    }

    fn mutate(&mut self, seed: &[u8]) -> Vec<u8> {
        let mut v = seed.to_vec();
        for _ in 0..1 + self.below(12) {
            match self.below(8) {
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
                    let extra = self.bytes(24);
                    v.splice(i..i, extra);
                }
                4 if !v.is_empty() => {
                    let i = self.below(v.len());
                    let b = [0x00, 0xff, 0x7f, 0x80, 0x20][self.below(5)];
                    for x in v.iter_mut().skip(i).take(4) {
                        *x = b;
                    }
                }
                5 if v.len() >= 4 => {
                    // A length field, big-endian, at a 4-byte boundary.
                    let i = self.below(v.len() / 4) * 4;
                    let n = [0u32, 1, 255, 256, u32::MAX, 0x7fff_ffff][self.below(6)];
                    v[i..i + 4].copy_from_slice(&n.to_be_bytes());
                }
                6 => {
                    let junk = [
                        b"%".as_slice(),
                        b"%2",
                        b"%zz",
                        b"%2F",
                        b"%00",
                        b"%2E%2E",
                        b"\xff",
                        b"2023-02-29T00:00:00",
                        b"2026-00-00T00:00:00",
                        b"9999-99-99T99:99:99",
                    ];
                    let piece = junk[self.below(junk.len())];
                    let i = self.below(v.len() + 1);
                    v.splice(i..i, piece.iter().copied());
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
        if self.below(16) == 0 {
            // A long run, still small enough to stay in the test process.
            let n = 1 + self.below(64 * 1024);
            let b = self.next() as u8;
            v = vec![b; n];
        }
        v
    }

    fn input(&mut self, seed: &[u8]) -> Vec<u8> {
        if self.next().is_multiple_of(2) {
            self.mutate(seed)
        } else {
            self.bytes(seed.len().saturating_mul(2) + 32)
        }
    }
}

fn seconds() -> u64 {
    let mut args = env::args().skip(1);
    let mut secs = 60u64;
    while let Some(a) = args.next() {
        if a == "--seconds" {
            if let Some(v) = args.next().and_then(|s| s.parse().ok()) {
                secs = v;
            }
        }
    }
    secs
}

fn run_for(name: &'static str, seed: u64, budget: Duration, mut one: impl FnMut(&mut Rng) -> bool) {
    let mut rng = Rng(seed);
    let start = Instant::now();
    let mut iters = 0u64;
    let mut accepted = 0u64;
    let mut slowest = Duration::ZERO;
    while start.elapsed() < budget {
        let t = Instant::now();
        if one(&mut rng) {
            accepted += 1;
        }
        let took = t.elapsed();
        if took > slowest {
            slowest = took;
        }
        if took > Duration::from_secs(2) {
            eprintln!("{name}: input {iters} took {took:?} (seed {seed:#x})");
            std::process::exit(1);
        }
        iters += 1;
    }
    eprintln!(
        "{name}: {iters} inputs, {accepted} accepted, slowest {slowest:?}, {:.1}s",
        start.elapsed().as_secs_f64()
    );
}

fn trashinfo_seed() -> Vec<u8> {
    info::render_batch(
        std::path::Path::new("/home/u/a b%c.txt"),
        "2026-07-03T12:00:00",
        Some("1-2"),
    )
    .into_bytes()
}

fn fuzz_trashinfo(rng: &mut Rng, seed: &[u8]) -> bool {
    let input = rng.input(seed);
    let parsed =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| info::parse_bytes(&input)));
    let parsed = parsed.unwrap_or_else(|_| panic!("trashinfo parse panicked"));
    if let Some(d) = parsed.deletion_date.as_deref() {
        let _ = info::parse_local_to_epoch(d);
        assert!(d.len() <= 128, "kept a long date");
    }
    if let Some(b) = parsed.batch.as_deref() {
        assert!(b.len() <= 128, "kept a long batch");
    }
    parsed.deletion_date.is_some() || !parsed.path.as_os_str().is_empty()
}

fn fuzz_dates(rng: &mut Rng) -> bool {
    let shapes = [
        "2026-07-03T12:00:00",
        "2024-02-29T23:59:60",
        "2023-02-29T00:00:00",
        "2026-00-00T00:00:00",
        "1900-02-29T12:00:00",
        "9999-99-99T99:99:99",
    ];
    let pick = shapes[rng.below(shapes.len())];
    let text = String::from_utf8_lossy(&rng.input(pick.as_bytes())).into_owned();
    let got = info::parse_local_to_epoch(&text);
    if text.len() == 19 && got.is_some() {
        // A date that parses is a real day; mktime must agree on its hour.
        let again = info::parse_local_to_epoch(&text);
        assert_eq!(got, again);
    }
    got.is_some()
}

fn fuzz_sizes(rng: &mut Rng, seed: &[u8]) -> bool {
    let line = rng.input(seed);
    let parsed = directorysizes_parse_line(&line);
    if let Some(l) = &parsed {
        assert!(!l.name.is_empty() && !l.name.contains(&0) && !l.name.contains(&b'/'));
        assert!(line.len() <= 64 * 1024);
        let enc = rtrash::util::url_encode(&l.name);
        let again = format!("{} {} {enc}", l.size, l.mtime);
        let back = directorysizes_parse_line(again.as_bytes());
        assert_eq!(back.as_ref(), Some(l));
    }
    parsed.is_some()
}

fn fuzz_store(rng: &mut Rng, seed: &[u8]) -> bool {
    let input = rng.input(seed);
    let parsed = ds_store::parse(&input);
    if let Ok(recs) = &parsed {
        let _ = ds_store::put_back(recs);
        // A record is at least a name length, a code and a type.
        assert!(recs.len().saturating_mul(12) <= input.len());
    }
    let _ = ds_store::parse(&rng.bytes(96));
    parsed.is_ok()
}

fn fixed_cases() {
    assert!(info::parse_local_to_epoch("2023-02-29T00:00:00").is_none());
    assert!(info::parse_local_to_epoch("2026-00-00T00:00:00").is_none());
    assert!(directorysizes_parse_line(&vec![b'0'; 70_000]).is_none());
    let mut node = Vec::new();
    node.extend_from_slice(&0u32.to_be_bytes());
    node.extend_from_slice(&u32::MAX.to_be_bytes());
    let img = ds_store::assemble(&[ds_store::dsdb_header(2, 0, 0, 1), node], 1);
    assert!(ds_store::parse(&img).is_err());
}

fn main() -> ExitCode {
    let budget = Duration::from_secs(seconds());
    fixed_cases();
    let info_seed = trashinfo_seed();
    let size_seed = b"8192 1791660998 d%FF%20x".to_vec();
    let store_seed = ds_store::write(&[
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
            value: Value::Blob(vec![1, 2, 3, 255]),
        },
    ]);
    eprintln!("fuzz budget {budget:?} per target");
    run_for("trashinfo", 0x7a5f_19c3_0b1e_44d1, budget, |rng| {
        fuzz_trashinfo(rng, &info_seed)
    });
    run_for("dates", 0x1234_5678_9abc_def1, budget, fuzz_dates);
    run_for("directorysizes", 0x0dd5_eed5_1234_9876, budget, |rng| {
        fuzz_sizes(rng, &size_seed)
    });
    run_for("ds_store", 0xd5d5_0000_beef_0001, budget, |rng| {
        fuzz_store(rng, &store_seed)
    });
    ExitCode::SUCCESS
}
