# Contributing

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
