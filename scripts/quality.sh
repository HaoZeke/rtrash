#!/usr/bin/env bash
# Local/CI-aligned quality bar for rtrash (binary crate).
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

echo "==> cargo fmt --check"
cargo fmt --check

echo "==> cargo clippy --locked --all-targets -- -D warnings"
cargo clippy --locked --all-targets -- -D warnings

if command -v cargo-nextest >/dev/null 2>&1 || cargo nextest --version >/dev/null 2>&1; then
  echo "==> cargo nextest run --locked --profile ci"
  cargo nextest run --locked --profile ci
else
  echo "==> cargo test --locked (install cargo-nextest for CI parity)"
  cargo test --locked
fi

if command -v cargo-deny >/dev/null 2>&1; then
  echo "==> cargo-deny check"
  cargo-deny check
elif cargo deny --version >/dev/null 2>&1; then
  echo "==> cargo deny check"
  cargo deny check
else
  echo "==> cargo deny: skipped (install cargo-deny)"
fi

echo "quality bar OK"
