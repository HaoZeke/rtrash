# Contributing

## Rust quality bar (matches CI)

CI (`.github/workflows/ci.yml`) runs the same commands on `main`/PRs. Prefer **`rg.terra`** or CI for compile-heavy steps (do not thrash a laptop with full `cargo test` loops).

```shell
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo nextest run --locked --profile ci   # or: cargo test --locked
cargo deny check                          # needs cargo-deny; policy in deny.toml
```

Or: `./scripts/quality.sh` (fmt → clippy → nextest/test → deny when available).

`Cargo.lock` is **committed** for this binary crate; use `--locked` so CI and local builds resolve the same graph.

**Release packaging:** tag releases are **`Release musl package`** (`scripts/package-release.sh` → versioned tarballs for `cargo-binstall`). [cargo-dist](https://opensource.axo.dev/cargo-dist/) (`dist-workspace.toml`) runs **PR-only** `dist plan` (`.github/workflows/release.yml`); after `dist generate --mode ci` run `./scripts/dist-pr-only-release-yml.sh`.

**Benchmarks on PRs:** ASV suite under `benchmarks/` (`asv.conf.json`). Workflow `Benchmark PR` builds release `rtrash` for base and head SHAs, runs `asv run --quick`, compares with asv-spyglass, and `HaoZeke/asv-perch` comments on the PR. Local:

```shell
cargo build --release --bin rtrash
export RTRASH_BIN=$PWD/target/release/rtrash
asv machine --yes
asv run -E existing:$(command -v python3) --quick
```

`prek` covers fast hygiene (whitespace, yaml/toml, large files, codespell). It does **not** compile the crate on every commit.

## Changelog fragments (towncrier)

User-visible changes need a fragment under `docs/newsfragments/`:

```shell
towncrier create -c "Describe the change." +my-change.added.md
# types: security removed deprecated added dev changed fixed misc
```

At release (after version bump):

```shell
towncrier build --version X.Y.Z --yes
git add CHANGELOG.md docs/newsfragments
```

Numeric names like `42.fixed.md` link to GitHub issue `#42`. Prefer `+slug.type.md` when there is no public issue.

## Version lockstep (cocogitto)

`cog.toml` bumps `Cargo.toml`, `pyproject.toml`, and `docs/source/conf.py` together and leaves `CHANGELOG.md` to towncrier (`disable_changelog = true`).

## Pre-commit (prek)

```shell
prek install
prek run -a
```

Hooks include trailing whitespace, yaml/toml checks, `check-added-large-files` (1 MB), codespell, and a Cargo/pyproject version lockstep check. CI re-runs prek and [large-file-auditor](https://github.com/HaoZeke/large-file-auditor).

## Link check (lychee)

```shell
lychee --config .lychee.toml README.md CHANGELOG.md 'docs/**/*.md' 'docs/orgmode/**/*.org'
```

## Documentation

Org sources live under `docs/orgmode/`; build with `./docs/build.sh`. PR builds upload a `documentation` artifact for [doc-previewer](https://github.com/HaoZeke/doc-previewer) comments.
