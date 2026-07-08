# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

This project uses [*towncrier*](https://towncrier.readthedocs.io/).
Unreleased fragments live in [`docs/newsfragments/`](docs/newsfragments/).
Name fragments `+slug.<type>.md` (no issue) or `<issue>.<type>.md` (links to GitHub).

<!-- towncrier release notes start -->

## [0.1.3](https://github.com/HaoZeke/rtrash/tree/v0.1.3) - 2026-07-08

### Added

- Add a ratatui restore browser on TTY (`rtrash restore`): filter, navigate, multi-restore session, force/confirm overwrite; `--plain` keeps line-mode selection.
- Interactive restore lists the full trash (trash-cli style) when PATH is omitted; add `--cwd-only` for the previous cwd-scoped pick; accept piped index selection without a TTY.
- Restore TUI: multi-select and fuzzy filter; add empty and put TUI browsers on TTY (multi-select, confirm); `--plain` skips TUI.
- TUI keybinds fully customizable via TOML (`keys.toml` / `RTRASH_KEYS`); `rtrash keys` CLI.
- Windows: system Recycle Bin backend for put/list/restore/empty (not FreeDesktop on-disk fiction).
- macOS: experimental FreeDesktop home trash (`$XDG_DATA_HOME/Trash`), not Finder Trash.

### Developer

- Refresh dated trash-cli comparison in docs/benchmarks.md (2026-07-08); harden benches/compare_trash_cli.py to reject multi-call rtrash as trash-cli.

### Changed

- TUI: live fuzzy filter, shared keys + ? help, viewport paging across restore/empty/put.

### Fixed

- Python bindings release the GIL around put/empty/restore/list I/O (PyO3 `detach`); force `--plain` (no TUI).


## [0.1.2](https://github.com/HaoZeke/rtrash/tree/v0.1.2) - 2026-07-08

### Fixed

- Bump optional PyO3 dependency to 0.29 (RUSTSEC-2026-0176 / RUSTSEC-2026-0177); cargo-deny CI green on GitHub Actions.


## [0.1.1](https://github.com/HaoZeke/rtrash/tree/v0.1.1) - 2026-07-08

### Developer

- Add CI quality bar (`cargo fmt --check`, `clippy -D warnings`, `test --locked`), commit `Cargo.lock`, and `cargo-deny` policy (`deny.toml`) with matching CONTRIBUTING/`scripts/quality.sh` docs.
- Add towncrier (`CHANGELOG.md` / `docs/newsfragments/`), cocogitto (`cog.toml` version lockstep), `prek.toml` (large-file guard, toml/yaml hygiene), lychee link check, `large-file-auditor` CI, and HaoZeke/doc-previewer PR comments for the Sphinx site.
- Adopt cargo-nextest (CI profile), cargo-dist (Linux musl releases + shell installer), and ASV PR benchmarks with asv-spyglass + HaoZeke/asv-perch comments (eOn-style).


## [0.1.0](https://github.com/HaoZeke/rtrash/tree/v0.1.0) - 2026-07-08

### Added

- Initial release: FreeDesktop trash CLI with rm-compatible put, multi-call suite (`trash-put` / `trash-list` / `trash-restore` / `trash-empty` / `trash-rm`), parallel empty via rayon, and `rtrash setup` for links, completions, and man page.
- Publish **rtrash 0.1.0** to [crates.io](https://crates.io/crates/rtrash) and [PyPI](https://pypi.org/project/rtrash/) (maturin/PyO3 bindings; Linux x86_64 wheels for CPython 3.10–3.14).
- GitHub Release musl static assets for **x86_64** and **aarch64** Linux with `cargo-binstall` metadata remapping glibc hosts to musl tarballs.
- Use FreeDesktop `directorysizes` cache for `status` and `empty --dry-run` reclaim estimates when the cache is valid.
- Embedded bash/zsh/fish completions and man page (`rtrash completions`, `rtrash man`, installed by `rtrash setup`).

### Developer

- Docs site at https://rtrash.rgoswami.me (Sphinx + Shibuya); orgmode sources under `docs/orgmode/`.
