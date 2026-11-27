#!/usr/bin/env bash
# Ensure the root package, pyproject.toml, and the rtrash-hud crate agree.
# The hud crate's package version must match. If its rtrash dependency
# pins a version, that pin must match too.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
c="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
p="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/pyproject.toml" | head -1)"
hud="$root/crates/rtrash-hud/Cargo.toml"
h="$(sed -n 's/^version = "\(.*\)"/\1/p' "$hud" | head -1)"
pin="$(sed -n 's/^rtrash = .*version = "\([^"]*\)".*/\1/p' "$hud" | head -1)"
if [[ -z "$c" || -z "$p" || -z "$h" ]]; then
  echo "could not read versions (cargo='$c' pyproject='$p' rtrash-hud='$h')" >&2
  exit 1
fi
if [[ "$c" != "$p" || "$c" != "$h" ]]; then
  echo "version mismatch: Cargo.toml=$c pyproject.toml=$p rtrash-hud=$h" >&2
  exit 1
fi
if [[ -n "$pin" && "$pin" != "$c" ]]; then
  echo "version mismatch: Cargo.toml=$c rtrash-hud rtrash pin=$pin" >&2
  exit 1
fi
echo "versions match: $c"
