#!/usr/bin/env bash
# Rebuild docs/demo casts+GIFs from sequence scripts using a real rtrash.
#
# Usage:
#   ./docs/demo/record.sh                 # both quickstart + suite
#   ./docs/demo/record.sh quickstart|suite
#   ./docs/demo/record.sh --dry-run
#   RTRASH_BIN=/path/to/rtrash ./docs/demo/record.sh
#
# Isolation (critical):
#   temp HOME + XDG_DATA_HOME + RTRASH_DEMO_PIN=--trash-dir=$XDG_DATA_HOME/Trash
#   on every list/status/restore/empty/rm (unpinned discovery can wipe real trash).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DEMO_DIR="$ROOT/docs/demo"
COLS="${DEMO_COLS:-78}"
ROWS="${DEMO_ROWS:-22}"

which_demos=()
dry=0
for a in "$@"; do
  case "$a" in
    --dry-run) dry=1 ;;
    quickstart|suite) which_demos+=("$a") ;;
    -h|--help)
      echo "Usage: $0 [--dry-run] [quickstart|suite]..."
      exit 0
      ;;
    *) echo "unknown arg: $a" >&2; exit 2 ;;
  esac
done
if [[ ${#which_demos[@]} -eq 0 ]]; then
  which_demos=(quickstart suite)
fi

echo "demos: ${which_demos[*]}"
for name in "${which_demos[@]}"; do
  seq="$DEMO_DIR/sequence.sh"
  [[ "$name" == suite ]] && seq="$DEMO_DIR/sequence-suite.sh"
  echo "--- $name: $seq ---"
  grep -E "run '|run \"|banner \"" "$seq" || true
  if ! grep -q 'RTRASH_DEMO_PIN' "$seq"; then
    echo "error: $seq must require RTRASH_DEMO_PIN" >&2
    exit 1
  fi
done
echo "---"

if [[ "$dry" -eq 1 ]]; then
  echo "dry-run: not invoking asciinema/agg"
  exit 0
fi

need() {
  command -v "$1" >/dev/null 2>&1 || { echo "error: missing '$1'" >&2; exit 1; }
}
need asciinema
need agg

RTRASH_BIN="${RTRASH_BIN:-$(command -v rtrash || true)}"
if [[ -z "${RTRASH_BIN}" || ! -x "${RTRASH_BIN}" ]]; then
  for cand in \
    "${CARGO_TARGET_DIR:-$ROOT/target}/release/rtrash" \
    "$HOME/tmp/rtrash-target/release/rtrash" \
    "$ROOT/target/release/rtrash"; do
    [[ -x "$cand" ]] && RTRASH_BIN=$cand && break
  done
fi
[[ -n "${RTRASH_BIN}" && -x "${RTRASH_BIN}" ]] || {
  echo "error: rtrash binary not found (set RTRASH_BIN=...)" >&2
  exit 1
}
echo "using rtrash: $RTRASH_BIN ($("$RTRASH_BIN" --version 2>/dev/null || true))"

record_one() {
  local name=$1
  local seq=$2
  local cast=$3
  local gif=$4

  local WORKDIR
  WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/rtrash-demo-XXXXXX")
  # Never call bare `rm` — PATH may point multi-call rtrash at `rm`.
  cleanup() { /bin/rm -rf "$WORKDIR"; }
  trap cleanup EXIT

  export RTRASH_DEMO_WORK="$WORKDIR/work"
  export XDG_DATA_HOME="$WORKDIR/xdg"
  export HOME="$WORKDIR/home"
  export RTRASH_DEMO_PIN="--trash-dir=${XDG_DATA_HOME}/Trash"
  export RTRASH_DEMO_BIN_DIR="$WORKDIR/bin"
  mkdir -p "$RTRASH_DEMO_WORK" "$XDG_DATA_HOME/Trash/files" "$XDG_DATA_HOME/Trash/info" \
    "$HOME" "$RTRASH_DEMO_BIN_DIR"

  ln -sfn "$RTRASH_BIN" "$RTRASH_DEMO_BIN_DIR/rtrash"
  # multi-call names → same binary
  for n in trash-put trash-list trash-restore trash-empty trash-rm rm trash; do
    ln -sfn "$RTRASH_BIN" "$RTRASH_DEMO_BIN_DIR/$n"
  done
  export PATH="$RTRASH_DEMO_BIN_DIR:$PATH"

  echo "sandbox[$name]: PIN=$RTRASH_DEMO_PIN"

  # Preflight: one put → exactly one pinned list row → empty
  printf 'iso\n' >"$RTRASH_DEMO_WORK/iso.txt"
  ( cd "$RTRASH_DEMO_WORK" && rtrash put iso.txt )
  n_list=$(rtrash list $RTRASH_DEMO_PIN | wc -l)
  if [[ "$n_list" -ne 1 ]]; then
    echo "error: preflight expected 1 pinned item, got $n_list" >&2
    rtrash list $RTRASH_DEMO_PIN >&2 || true
    exit 1
  fi
  rtrash empty --plain $RTRASH_DEMO_PIN

  export COLUMNS=$COLS LINES=$ROWS TERM="${TERM:-xterm-256color}"
  asciinema record \
    -f asciicast-v2 \
    --window-size "${COLS}x${ROWS}" \
    -c "bash '$seq'" \
    "$cast" \
    --overwrite

  if grep -E 'pCloudDrive|/\.Trash-' "$cast"; then
    echo "error: cast leaked volume paths" >&2
    exit 1
  fi
  if grep -E 'Removed 1[0-9] item|Removed [2-9][0-9] item' "$cast"; then
    echo "error: cast empty removed double-digit items" >&2
    exit 1
  fi

  agg --cols "$COLS" --font-size 14 --speed 1.2 "$cast" "$gif"
  echo "wrote $cast"
  echo "wrote $gif"

  # Keep Sphinx/static copies in sync for the docs site embeds.
  STATIC_DEMO="$ROOT/docs/source/_static/demo"
  mkdir -p "$STATIC_DEMO"
  cp -f "$cast" "$gif" "$STATIC_DEMO/"
  ls -la "$cast" "$gif"
  trap - EXIT
  cleanup
}

for name in "${which_demos[@]}"; do
  case "$name" in
    quickstart)
      record_one quickstart \
        "$DEMO_DIR/sequence.sh" \
        "$DEMO_DIR/rtrash-quickstart.cast" \
        "$DEMO_DIR/rtrash-quickstart.gif"
      ;;
    suite)
      record_one suite \
        "$DEMO_DIR/sequence-suite.sh" \
        "$DEMO_DIR/rtrash-suite.cast" \
        "$DEMO_DIR/rtrash-suite.gif"
      ;;
  esac
done
