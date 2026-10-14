#!/usr/bin/env bash
# Time `rtrash du` against dua, gdu and du on the same trees, warm cache.
# Usage: benches/compare_du.sh RTRASH_BIN TREE...
# Tools missing from PATH are left out; set DUA, GDU or HYPERFINE to a path
# to use a binary that is not on PATH. Results go to stdout as a markdown
# table per tree (hyperfine --export-markdown).
set -euo pipefail
rtrash=${1:?usage: compare_du.sh RTRASH_BIN TREE...}
shift
[ "$#" -gt 0 ] || { echo "compare_du.sh: name at least one TREE" >&2; exit 2; }
hyperfine=${HYPERFINE:-hyperfine}
dua=${DUA:-dua}
gdu=${GDU:-gdu}
command -v "$hyperfine" >/dev/null || { echo "compare_du.sh: hyperfine not found" >&2; exit 2; }
out=$(mktemp)
trap 'rm -f "$out"' EXIT
for tree in "$@"; do
  cmds=("$rtrash du $tree")
  command -v "$dua" >/dev/null && cmds+=("$dua aggregate $tree")
  command -v "$gdu" >/dev/null && cmds+=("$gdu -n -p $tree")
  cmds+=("du -s $tree")
  "$hyperfine" -N --warmup 2 --runs 8 --export-markdown "$out" "${cmds[@]}" >&2
  echo "### $tree"
  cat "$out"
  echo
done
