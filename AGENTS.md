# Working on rtrash as an agent

Run `python3 scripts/precheck.py --base origin/main` before every push. It
needs no build and finishes in under a second. CI runs the same script.

## Rules and what enforces them

| Rule | Enforced by | Past mistake |
| --- | --- | --- |
| Every `--help` long option is in all six completion scripts, the trash-* multicall entries and the man page | `scripts/precheck.py` (no build), `tests/completions_parity.rs` | rounds 141, 143c |
| Test scratch dirs come from `tests/common::scratch_dir` | the helper's counter; precheck `process::id()` ratchet | round 149 -> red 4a96122 |
| Two trash puts that a test orders by date are a whole second apart, by `common::next_trash_second()`, not a fixed sleep | precheck long-sleep check | 9708b71 |
| A test asserts what code does, not how fast it ran: no `assert!` that `elapsed()` stays under 5 s; count the polls through a seam, and keep a time bound only as a hang guard of 5 s or more | precheck tight-elapsed check | round 215: a 1.5 s bound in `next_trash_second_waits_for_a_later_second` failed on a busy macOS runner |
| A test that compares du table rows, against a literal or another run, takes each with `common::du_row`, which writes the AGE cell as `AGE`; to check an age, assert that cell alone | precheck du-row check | round 218: `block_size_variables_set_the_text_units` compared whole rows and failed when `0s` became `1s` |
| Code that reserves a `.trashinfo` and moves its payload runs inside `interrupt::critical()`, so Ctrl-C waits until record and payload agree; code that removes a payload and then its record opens the section once the payload is gone; a long copy that cleans up after itself runs inside `interrupt::abortable` | precheck record-section check; `ctrl_c_inside_a_record_waits_until_the_pair_is_whole`, `a_stopped_wipe_keeps_every_record_whose_payload_stays` | round 220: before it, Ctrl-C could leave an empty record or a partial cross-device copy with a record; round 222: an empty stopped partway left records whose payloads were gone |
| A precheck rule reads Rust through `rust_code`, which drops comments and blanks strings and char literals in one pass; a shape it once got wrong goes into `RUST_CODE_CASES` | precheck lexer self-check | round 221: the regex blanker lost step after a `'"'` char literal and hid 87 fns in 11 files from the rules |
| Finder paths are compared as `finder_shared::FinderPath` (links resolved, NFC), never as written | the type; `same_path` and `under` go through it | 125b, 165 |
| A test sandbox tag is unique per test file | precheck duplicate-tag check | round 149 |
| No function takes more than 7 parameters; group options in a struct | precheck arg count, clippy `-D warnings` | round 155 |
| Code never names `libc::time_t` or `libc::suseconds_t`, which libc deprecates on musl; a cast is `as _`, so the call or field gives the width | precheck musl-alias check; CI musl clippy job and `scripts/cross-clippy.sh` | round 225: `src/info.rs` failed clippy on x86_64-unknown-linux-musl |
| Completion tests for pwsh, elvish, fish and nu run, not skip, before a push | `RTRASH_REQUIRE_SHELLS=1` on the agent; pwsh is required on CI | round 143c |
| A changed message or newly valid value is updated in every test, including `src/` unit tests and `tests/windows_mount.rs` | precheck removed-literal warning (partial); judgment | rounds 144b, 154 |
| A permanent removal outside `permanent_guard` calls `permanent_guard::remove` or `remove_record`; any other call of a removal primitive sits in a function `PERMANENT_ALLOW` lists with its count and reason | precheck permanent-route check | round 194: two Windows restores hand-rolled `gate()` plus `remove_forcing` |
| Every read of an `RTRASH_TEST_*` variable in `src` is debug-only: `cfg!(debug_assertions)` before it in its function, or `#[cfg(debug_assertions)]` on the function or a module around it; a seam is read by its literal name, never through a const | precheck test-seam check | round 195: `RTRASH_TEST_BIN_DIR` points `status --fix` at any folder |
| Code under `cfg(windows)` or `cfg(target_os = "macos")` compiles before a push | `scripts/cross-clippy.sh` | round 112 |
| Code is rustfmt-clean before a push, and lines stay within 100 columns | precheck runs `cargo fmt --all --check` when a toolchain is there (no build); line-width warning otherwise | rounds 101b, 102d, 143b, 170-172 |
| A commit subject is a plain sentence that starts with a capital, with no `type(scope):` prefix | precheck subject check under `--base`; the prek commit-msg hook (`precheck.py --commit-msg`) | round 209: `feat(du): ...` reworded before it reached main |
| macOS and Windows behaviour is proven on CI before main moves: push the round to a `round*` branch, wait for green, then fast-forward main | push triggers on `main` and `round*` in every workflow | 37 of 43 red mains failed off Linux |

Judgment calls no check can make:

- A Finder `put_back` only rewrites `.DS_Store`; it does not bring the file
  back. A test that trashes the same name twice recreates the file first.
- Windows Recycle Bin times are whole seconds; macOS temp paths sit under
  `/private/var`; APFS refuses names that are not UTF-8. Write tests for the
  platform's rules, not Linux's.
