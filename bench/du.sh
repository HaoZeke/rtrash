#!/usr/bin/env bash
# Time `rtrash du` against other disk usage tools with hyperfine.
#
# Usage: bench/du.sh [TREE]...
#
# Each TREE is a directory, or the word `synthetic` for a generated tree of
# small files under $TMPDIR (removed on exit). Defaults: ~/.rustup, /usr and
# a synthetic tree.
#
# Environment:
#   RTRASH       rtrash binary to time (default: rtrash on PATH)
#   RUNS         timed runs per command, warm cache (default 10)
#   WARMUP       untimed runs before the warm runs (default 2)
#   COLD         1: also time cold-cache runs, 0: skip, auto: run them when
#                `sudo -n` can write /proc/sys/vm/drop_caches (default auto)
#   COLD_RUNS    timed runs per command, cold cache (default 3)
#   SYNTH_FILES  files in the synthetic tree (default 500000)
#   OUT          directory for hyperfine Markdown and JSON (default bench-out)
#
# Competitors run when installed: du(1), diskus, dust, dua, pdu, gdu.
# Missing tools are skipped. Install them with, for example:
#   cargo install --locked hyperfine diskus du-dust dua-cli parallel-disk-usage
set -euo pipefail

RTRASH=${RTRASH:-rtrash}
RUNS=${RUNS:-10}
WARMUP=${WARMUP:-2}
COLD=${COLD:-auto}
COLD_RUNS=${COLD_RUNS:-3}
SYNTH_FILES=${SYNTH_FILES:-500000}
OUT=${OUT:-bench-out}

command -v hyperfine >/dev/null || { echo "du.sh: hyperfine not found" >&2; exit 1; }
command -v "$RTRASH" >/dev/null || { echo "du.sh: $RTRASH not found" >&2; exit 1; }

drop_caches='sudo -n sh -c "sync; echo 3 > /proc/sys/vm/drop_caches"'
if [ "$COLD" = auto ]; then
  if sudo -n sh -c 'echo 3 > /proc/sys/vm/drop_caches' 2>/dev/null; then COLD=1; else COLD=0; fi
fi

synth_dir=""
cleanup() { if [ -n "$synth_dir" ]; then rm -rf "$synth_dir"; fi; }
trap cleanup EXIT

# 100 top directories, each with 10 subdirectories of equal file counts;
# every file holds 100 bytes.
make_synthetic() {
  synth_dir=$(mktemp -d "${TMPDIR:-/tmp}/rtrash-bench-synth.XXXXXX")
  python3 - "$synth_dir" "$SYNTH_FILES" <<'PY'
import os, sys
root, total = sys.argv[1], int(sys.argv[2])
leaves = 1000
per = max(1, total // leaves)
data = b"x" * 100
for i in range(leaves):
    d = os.path.join(root, f"d{i // 10:03d}", f"s{i % 10}")
    os.makedirs(d)
    for j in range(per):
        with open(os.path.join(d, f"f{j:05d}"), "wb") as f:
            f.write(data)
PY
}

trees=("$@")
[ ${#trees[@]} -eq 0 ] && trees=("$HOME/.rustup" /usr synthetic)

mkdir -p "$OUT"
{
  echo "host: $(uname -srm), $(nproc) CPUs"
  for t in "$RTRASH" du diskus dust dua pdu gdu hyperfine; do
    command -v "$t" >/dev/null && echo "$t: $("$t" --version 2>&1 | head -n1)"
  done
} | tee "$OUT/versions.txt"

for tree in "${trees[@]}"; do
  if [ "$tree" = synthetic ]; then
    echo "==> generating $SYNTH_FILES files"
    make_synthetic
    tree=$synth_dir
    label=synthetic
  else
    label=$(basename "$tree")
    label=${label#.}
  fi
  [ -d "$tree" ] || { echo "du.sh: skipping $tree (not a directory)" >&2; continue; }
  q=$(printf '%q' "$tree")
  echo "==> $tree: $("$RTRASH" du --no-git --json -n 0 "$tree" 2>/dev/null \
    | python3 -c 'import json,sys; r=json.load(sys.stdin)["roots"][0]; print(r["files"], "files,", r["dirs"], "dirs,", r["disk"], "bytes on disk")')" \
    | tee -a "$OUT/versions.txt"

  cmds=(
    -n "rtrash du" "$RTRASH du -n 20 $q"
    -n "rtrash du --no-git" "$RTRASH du -n 20 --no-git $q"
    -n "rtrash du -n 0 --no-git" "$RTRASH du -n 0 --no-git $q"
    -n "du -s" "du -s $q"
  )
  command -v diskus >/dev/null && cmds+=(-n diskus "diskus $q")
  command -v dust >/dev/null && cmds+=(-n "dust -n 20" "dust -n 20 $q")
  command -v dua >/dev/null && cmds+=(-n dua "dua $q")
  command -v pdu >/dev/null && cmds+=(-n "pdu -H" "pdu -H $q")
  command -v gdu >/dev/null && cmds+=(-n "gdu -n" "gdu -n -p $q")

  # Exit codes are ignored: unreadable entries make some tools exit 1.
  hyperfine -N -i --warmup "$WARMUP" --runs "$RUNS" \
    --export-markdown "$OUT/$label-warm.md" --export-json "$OUT/$label-warm.json" \
    "${cmds[@]}"
  if [ "$COLD" = 1 ]; then
    hyperfine -N -i --runs "$COLD_RUNS" --prepare "$drop_caches" \
      --export-markdown "$OUT/$label-cold.md" --export-json "$OUT/$label-cold.json" \
      "${cmds[@]}"
  fi
  if [ "$tree" = "$synth_dir" ]; then
    rm -rf "$synth_dir"
    synth_dir=""
  fi
done
