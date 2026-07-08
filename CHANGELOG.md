# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

This project uses [*towncrier*](https://towncrier.readthedocs.io/).
Unreleased fragments live in [`docs/newsfragments/`](docs/newsfragments/).
Name fragments `+slug.<type>.md` (no issue) or `<issue>.<type>.md` (links to GitHub).

<!-- towncrier release notes start -->

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
