# Benchmarks and comparison vs Python trash-cli

## Verdict

| Question | Verdict |
|----------|---------|
| **Safer** than permanent `os.remove` / bare `rm`? | **Yes** — FreeDesktop trash and rm-shaped fail-safes; recoverable until empty |
| **Safer** than trash-cli? | **Roughly equivalent** FreeDesktop correctness; rtrash has stronger GNU-style put fail-safes (`-f`/`-i` last-wins, preserve-root, and related flags) |
| **Better** for this niche? | **Yes** if you want one native multi-call binary and in-process Python (`import rtrash`) without spawning trash-cli |
| **Faster**? | **Yes on the 2026-09-13 terra fixture** — put 6.9×, list 35×, empty 6.0× vs trash-cli 0.24.5.26 (see table) |

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

## Measured results (Linux x86_64, 2026-09-13)

Harness: `benches/compare_trash_cli.py` on **terra-x86_64** (`Linux 7.0.13-arch1-1-rg x86_64`, 32 CPUs, `/home` **btrfs**). After the put-batch fix: `benches/compare-trash-cli-2026-09-13-putfix.txt` (ends `COMPARE_OK`). Earlier same-day log (per-name fsync + catalog upsert) is historical.

Peers: **rtrash 0.1.4** (release `-p rtrash`), **trash-cli 0.24.5.26** (`/usr/bin/trash-put`, not multi-call rtrash). Optional: trashy **missing**, gtrash **missing** (not implied losers).

Fixture: 400 small files + one tree (80 nested files) = 401 trash entries; two trials; isolated `XDG_DATA_HOME`. List is timed on the full can after put (before empty). EXDEV put and 10k+ empty remain **unmeasured**.

| Tool | put avg (s) | list avg (s) | empty avg (s) |
|------|-------------|--------------|----------------|
| rtrash 0.1.4 release | **0.013118** | **0.001957** | **0.008739** |
| trash-cli 0.24.5.26 | 0.091093 | 0.068604 | 0.052841 |
| trash-cli / rtrash | **6.94×** | **35.05×** | **6.05×** |

Both tools: `ec=0`, `entries=401`, empty leaves `files_left=0` `info_left=0`, `LIST_OK` for a single-file put.

A same-day earlier run (per-name `.trashinfo` fsync plus per-name catalog encrypt) had rtrash put at 0.275 s. Batching the catalog store and deferring info/ fsync to the end of the argv is what flipped put. Single-name `put_one` still fsyncs that `.trashinfo` before the payload move.

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
