The `cargo-dist plan` workflow now uses dist 0.33.1, which runs on `ubuntu-22.04` instead of the retired `ubuntu-20.04` image, so the PR job starts again.
`dist-workspace.toml` sets `allow-dirty = ["ci"]`, since `scripts/dist-pr-only-release-yml.sh` rewrites the generated `release.yml` and `dist plan` would otherwise reject it as out of date.
