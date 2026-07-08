# Benchmarks and comparison vs Python trash-cli

## Verdict

| Question | Verdict |
|----------|---------|
| **Safer** than permanent `os.remove` / bare `rm`? | **Yes** — FreeDesktop trash and rm-shaped fail-safes; recoverable until empty |
| **Safer** than trash-cli? | **Roughly equivalent** FreeDesktop correctness; rtrash has stronger GNU-style put fail-safes (`-f`/`-i` last-wins, preserve-root, and related flags) |
| **Better** for this niche? | **Yes** if you want one native multi-call binary and in-process Python (`import rtrash`) without spawning trash-cli |
| **Faster**? | **Yes on the measured fixtures** below — not a claim for every machine or trash size |

Prefer trash-cli when you only want distro Python packaging and no Rust toolchain.

## Reproduce

```shell
cargo build --release
export RTRASH_BIN=$PWD/target/release/rtrash
python3 benches/compare_trash_cli.py | tee compare-trash-cli.log
```

Needs system `trash-put` / `trash-empty` / `trash-list` (version from `trash-put --version`).

**Fixture:** 400 small files plus one multi-file directory tree (80 nested files); isolated `XDG_DATA_HOME`; two trials per tool; timed put of the full set, then empty with a `--trash-dir` pin.

Optional peers (trashy, gtrash) run only if present on `PATH`.
On the 2026-07-07 run they were not installed.

## Measured results (Linux x86_64 host, 2026-07-07)

Host class: Linux x86_64.
Peers: **rtrash 0.1.0** (release build of this tree), **trash-cli 0.24.5.26**.

From `benches/compare_trash_cli.py` (two warm trials each):

| Tool | put avg (s) | empty avg (s) |
|------|-------------|----------------|
| **rtrash** (release) | **0.00507** | **0.00267** |
| trash-cli 0.24.5.26 | 0.0674 | 0.0334 |
| **speedup (trash-cli / rtrash)** | **~13×** | **~12×** |

Both tools: `ec=0`, multi-entry trash after put (`entries=401`), empty leaves `files_left=0` and `info_left=0`, `LIST_OK` for a single-file put.
Harness ended with `COMPARE_OK`.

### Earlier snapshot (same host class, 2026-07-05)

An earlier run reported about ~19× put and ~13× empty versus the same trash-cli version.
Absolute times vary with load and filesystem state; the order of magnitude for native put/empty is the useful takeaway.
Prefer the 2026-07-07 table above as the current dated numbers.

## Safety (not a timer)

| Property | rtrash | trash-cli | permanent `os.remove` |
|----------|--------|-----------|------------------------|
| FreeDesktop home + mount trash | yes | yes | no |
| Recoverable until empty | yes | yes | no |
| DE-shared trashcan | yes | yes | no |
| rm-compatible put flags | strong | partial | n/a (unlink) |
| In-process Python API | yes | no (CLI only) | yes (but permanent) |
| Native multi-call binary | yes | no (Python suite) | n/a |

## Prior empty microbench (rtrash-only)

An earlier same-host empty microbench (2001 top-level entries) measured bulk unlink empty about ~1.35× versus a pre-fastdelete rtrash binary.
That is rtrash evolution, not a trash-cli comparison.
Prefer the harness above for cross-tool claims.

## Large full-empty (rtrash vs prior rtrash)

Fixture on a Linux x86_64 btrfs home (2026-07-05): **8000** small top-level files plus one deep tree (1000 nested files and 250 wide dirs) → **8001** top-level trash entries.
Timed `rtrash empty --trash-dir=…` only; **7 trials** each after warm-up.

| Binary | avg wall (ms) | best | median | trimmed avg |
|--------|---------------|------|--------|-------------|
| Prior release (`fc7272f` baseline) | 274.4 | 236 | 286 | 275.0 |
| Post-fastdelete empty path (2026-07-05) | **144.0** | **138** | **143** | **143.8** |
| speedup | **~1.91×** | ~1.71× | **~2.00×** | **~1.91×** |

Every timed trial: `ec=0`, `top_before=8001`, `files_left=0`, `info_left=0`.
