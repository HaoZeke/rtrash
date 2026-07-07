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

Requires system `trash-put` / `trash-empty` / `trash-list` (trash-cli version
printed by `trash-put --version` on the host).

**Fixture class:** 400 small files + one multi-file directory tree (80 nested
files); isolated `XDG_DATA_HOME`; two trials per tool; timed put of the whole
set then empty with `--trash-dir` pin.

Optional peers (trashy, gtrash) are measured only when present on `PATH`; the
last refresh noted them as **unavailable** on the verification host (see table
notes).

## Measured results (rg.terra / `rgam5terra`, 2026-07-07)

Host: Linux `rgam5terra`, kernel `7.0.13-arch1-1-rg`, x86_64. UTC stamp:
`2026-07-07T21:01:52Z`. Peers: **rtrash 0.1.0** (release build of this tree),
**trash-cli 0.24.5.26**. **trashy** and **gtrash**: not installed on the host
(no timings invented).

From `benches/compare_trash_cli.py` (two warm trials each; full log in
verification `bench-refresh.log`):

| Tool | put avg (s) | empty avg (s) |
|------|-------------|----------------|
| **rtrash** (release) | **0.00507** | **0.00267** |
| trash-cli 0.24.5.26 | 0.0674 | 0.0334 |
| **speedup (trash-cli / rtrash)** | **~13×** | **~12×** |

Per-trial post-conditions for **both** tools: `ec=0`, multi-entry trash after
put (`entries=401` including deep tree), `files_left=0` and `info_left=0` after
empty. `LIST_OK` for a single-file put on both tools. Harness footer:
`COMPARE_OK`.

### Prior snapshot (same host class, 2026-07-05)

Earlier run on the same host class reported ~19× put / ~13× empty vs the same
trash-cli version. Absolute times vary with load and filesystem state; the
**order-of-magnitude** advantage for native put/empty remains. Prefer the
**2026-07-07** table above as the current dated evidence.

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

An earlier same-host empty microbench (2001 top-level entries) measured
bulk-unlink empty ~1.35× vs the pre-fastdelete rtrash binary. That is **rtrash
evolution**, not trash-cli. Prefer this document’s harness for cross-tool claims.

## Large full-empty (rtrash vs prior rtrash, same host)

Fixture on **rg.terra** btrfs `/home` (2026-07-05): **8000** small top-level
files + one deep tree (1000 nested files + 250 wide dirs) → **8001** top-level
trash entries. Timed `rtrash empty --trash-dir=…` only; **7 trials** each after
warm-up.

| Binary | avg wall (ms) | best | median | trimmed avg |
|--------|---------------|------|--------|-------------|
| Prior release (`fc7272f` baseline binary) | 274.4 | 236 | 286 | 275.0 |
| Post-fastdelete empty path (2026-07-05) | **144.0** | **138** | **143** | **143.8** |
| speedup | **~1.91×** | ~1.71× | **~2.00×** | **~1.91×** |

Every timed trial: `ec=0`, `top_before=8001`, `files_left=0`, `info_left=0`.
