# Working on rtrash as an agent

Run `python3 scripts/precheck.py --base origin/main` before every push. It
needs no build and finishes in under a second. CI runs the same script.

## Rules and what enforces them

| Rule | Enforced by | Past mistake |
| --- | --- | --- |
| Every `--help` long option is in all six completion scripts, the trash-* multicall entries and the man page | `scripts/precheck.py` (no build), `tests/completions_parity.rs` | rounds 141, 143c |
| Test scratch dirs come from `tests/common::scratch_dir` | the helper's counter; precheck `process::id()` ratchet | round 149 -> red 4a96122 |
| Two trash puts that a test orders by date are a whole second apart, by `common::next_trash_second()`, not a fixed sleep | precheck long-sleep check | 9708b71 |
| Finder paths are compared as `finder_shared::FinderPath` (links resolved, NFC), never as written | the type; `same_path` and `under` go through it | 125b, 165 |
| A test sandbox tag is unique per test file | precheck duplicate-tag check | round 149 |
| No function takes more than 7 parameters; group options in a struct | precheck arg count, clippy `-D warnings` | round 155 |
| Completion tests for pwsh, elvish, fish and nu run, not skip, before a push | `RTRASH_REQUIRE_SHELLS=1` on the agent; pwsh is required on CI | round 143c |
| A changed message or newly valid value is updated in every test, including `src/` unit tests and `tests/windows_mount.rs` | precheck removed-literal warning (partial); judgment | rounds 144b, 154 |
| Code under `cfg(windows)` or `cfg(target_os = "macos")` compiles before a push | `scripts/cross-clippy.sh` | round 112 |
| Lines stay within 100 columns | precheck warning, `cargo fmt --check` | rounds 101b, 102d, 143b |
| macOS and Windows behaviour is proven on CI before main moves: push the round to a `round*` branch, wait for green, then fast-forward main | push triggers on `main` and `round*` in every workflow | 37 of 43 red mains failed off Linux |

Judgment calls no check can make:

- A Finder `put_back` only rewrites `.DS_Store`; it does not bring the file
  back. A test that trashes the same name twice recreates the file first.
- Windows Recycle Bin times are whole seconds; macOS temp paths sit under
  `/private/var`; APFS refuses names that are not UTF-8. Write tests for the
  platform's rules, not Linux's.
