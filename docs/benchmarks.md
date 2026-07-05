# Benchmarks and comparison vs Python trash-cli

## Verdict

| Question | Verdict |
|----------|---------|
| **Safer** than permanent `os.remove` / bare `rm`? | **Yes** — FreeDesktop trash + rm-shaped fail-safes; recoverable until empty |
| **Safer** than trash-cli? | **Roughly equivalent** on FreeDesktop correctness; rtrash adds stronger GNU-style put fail-safes (`-f`/`-i` last-wins, preserve-root, …) |
| **Better** for this niche? | **Yes** if you want one native multi-call binary **and** in-process Python (`import rtrash`) without spawning trash-cli |
| **Faster**? | **Yes on the measured fixtures** (below) — not a claim for every hardware/size |

trash-cli remains preferable when you only want distro Python packaging and no Rust toolchain.

## Reproduce

```shell
cargo build --release
export RTRASH_BIN=$PWD/target/release/rtrash
python3 benches/compare_trash_cli.py | tee compare-trash-cli.log
```

Requires system `trash-put` / `trash-empty` / `trash-list` (trash-cli **0.24.5.26** on the verification host).

**Fixture class:** 400 small files + one multi-file directory tree (80 nested files); isolated `XDG_DATA_HOME`; two trials per tool; timed put of the whole set then empty with `--trash-dir` pin.

## Measured results (rg.terra / `rgam5terra`, 2026-07-05)

From `benches/compare_trash_cli.py` (see verification `compare-trash-cli.log`):

| Tool | put avg (s) | empty avg (s) |
|------|-------------|----------------|
| **rtrash** (release) | **0.00496** | **0.00325** |
| trash-cli 0.24.5.26 | 0.0950 | 0.0422 |
| **speedup (trash-cli / rtrash)** | **~19×** | **~13×** |

Per-trial post-conditions for **both** tools: `ec=0`, multi-entry trash after put (`entries=401` including deep tree), `files_left=0` and `info_left=0` after empty. `LIST_OK` for a single-file put on both tools.

## Safety / “better” (not a timer)

| Property | rtrash | trash-cli | permanent `os.remove` |
|----------|--------|-----------|------------------------|
| FreeDesktop home + mount trash | yes | yes | no |
| Recoverable until empty | yes | yes | no |
| DE-shared trashcan | yes | yes | no |
| rm-compatible put flags | strong | partial | n/a (unlink) |
| In-process Python API | yes | no (CLI only) | yes (but permanent) |
| Native multi-call binary | yes | no (Python suite) | n/a |

## Prior empty microbench (rtrash-only)

An earlier same-host empty microbench (2001 top-level entries) measured bulk-unlink empty ~1.35× vs the pre-fastdelete rtrash binary. That is **rtrash evolution**, not trash-cli. Prefer this document’s harness for cross-tool claims.

## Large full-empty (rtrash vs prior rtrash, same host)

Fixture on **rg.terra** btrfs home (2026-07-05): **8000** small top-level files +
one deep tree (1000 nested files + 250 wide dirs) → **8001** top-level trash
entries. Timed `rtrash empty --trash-dir=…` only; five trials each after warm-up.

| Binary | avg wall (ms) | best | median |
|--------|---------------|------|--------|
| Prior release (`fc7272f`) | 153.4 | 138 | 154 |
| This empty path | **141.4** | **136** | **140** |
| speedup | **~1.08× avg** | ~1.01× best | **~1.10× median** |

Reproduce:

```shell
# build baseline from a prior commit, then current tree; use benches below
cargo build --release
# see goal verification empty-fast-bench harness (5000–8000 files + deep tree)
```

Correctness: every timed run ended with `files_left=0` and `info_left=0`.
