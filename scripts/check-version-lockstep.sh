#!/usr/bin/env bash
# Ensure Cargo.toml and pyproject.toml package versions match.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
c="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
p="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/pyproject.toml" | head -1)"
if [[ -z "$c" || -z "$p" ]]; then
  echo "could not read versions (cargo='$c' pyproject='$p')" >&2
  exit 1
fi
if [[ "$c" != "$p" ]]; then
  echo "version mismatch: Cargo.toml=$c pyproject.toml=$p" >&2
  exit 1
fi
echo "versions match: $c"
