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
# Prefer a real Python trash-cli install; multi-call rtrash on PATH is rejected.
# Example: python3 -m venv .venv-bench && .venv-bench/bin/pip install 'trash-cli==0.24.5.26'
export TRASH_CLI_BIN_DIR=$PWD/.venv-bench/bin   # or any dir with trash-put/empty/list
python3 benches/compare_trash_cli.py | tee compare-trash-cli.log
```

The harness refuses `trash-put` when it is an rtrash multi-call link (version string contains `rtrash`).
Use `TRASH_CLI_BIN_DIR` or a PATH entry that is Python trash-cli.

**Fixture:** 400 small files plus one multi-file directory tree (80 nested files); isolated `XDG_DATA_HOME`; two trials per tool; timed put of the full set, then empty with a `--trash-dir` pin.

Optional peers (trashy, gtrash) are reported only if present on `PATH` and not rtrash multi-call.
On the 2026-07-08 run they were not installed.

## Measured results (Linux x86_64, 2026-07-08)

Host class: **Linux x86_64** (`sysname=Linux machine=x86_64`, build host for this tree).
Peers: **rtrash 0.1.2** (release build of this tree), **trash-cli 0.24.5.26** (isolated venv; not multi-call rtrash).
Optional: trashy **missing**, gtrash **missing**.

From `benches/compare_trash_cli.py` (two trials each; log ends with `COMPARE_OK`):

| Tool | put avg (s) | empty avg (s) |
|------|-------------|----------------|
| **rtrash** (release) | **0.005449** | **0.002986** |
| trash-cli 0.24.5.26 | 0.074310 | 0.039565 |
| **speedup (trash-cli / rtrash)** | **~13.6×** | **~13.3×** |

Both tools: `ec=0`, multi-entry trash after put (`entries=401`), empty leaves `files_left=0` and `info_left=0`, `LIST_OK` for a single-file put.

### Earlier snapshot (same host class, 2026-07-07)

Prior run (rtrash 0.1.0 vs trash-cli 0.24.5.26): put ~0.00507 s / empty ~0.00267 s vs trash-cli ~0.0674 / ~0.0334 (~13× / ~12×).
Absolute times vary with load and filesystem state; prefer the **2026-07-08** table above as the current dated numbers.

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
