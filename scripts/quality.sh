#!/usr/bin/env bash
# Local/CI-aligned quality bar for rtrash (binary crate).
# Prefer a remote builder for clippy/test; lockfile must be present.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

echo "==> cargo fmt --check"
cargo fmt --check

echo "==> cargo clippy --locked --all-targets -- -D warnings"
cargo clippy --locked --all-targets -- -D warnings

echo "==> cargo test --locked"
cargo test --locked

if command -v cargo-deny >/dev/null 2>&1 || command -v cargo >/dev/null && cargo deny --version >/dev/null 2>&1; then
  echo "==> cargo deny check"
  if command -v cargo-deny >/dev/null 2>&1; then
    cargo-deny check
  else
    cargo deny check
  fi
else
  echo "==> cargo deny: skipped (install cargo-deny to run supply-chain checks)"
fi

echo "quality bar OK"
