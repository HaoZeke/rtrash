#!/usr/bin/env bash
# Rebuild docs/demo/rtrash-quickstart.{cast,gif} from sequence.sh using a real rtrash.
#
# Usage:
#   ./docs/demo/record.sh              # record + gif (needs asciinema, agg, rtrash)
#   ./docs/demo/record.sh --dry-run    # print sequence only; exit 0 without tools
#   RTRASH_BIN=/path/to/rtrash ./docs/demo/record.sh
#
# Isolation (critical):
#   - temp HOME + XDG_DATA_HOME so put lands in sandbox home trash
#   - RTRASH_DEMO_PIN=--trash-dir=$XDG_DATA_HOME/Trash on every list/status/
#     restore/empty (unpinned suite commands discover volume trash and can
#     wipe real data — never record without the pin)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DEMO_DIR="$ROOT/docs/demo"
SEQ="$DEMO_DIR/sequence.sh"
CAST="$DEMO_DIR/rtrash-quickstart.cast"
GIF="$DEMO_DIR/rtrash-quickstart.gif"
COLS="${DEMO_COLS:-72}"
ROWS="${DEMO_ROWS:-20}"

dry=0
if [[ "${1:-}" == "--dry-run" ]]; then
  dry=1
fi

echo "demo sequence: $SEQ"
echo "--- sequence (source of truth) ---"
grep -E "run '|run \"|banner \"" "$SEQ" || true
echo "---"
if ! grep -q 'RTRASH_DEMO_PIN' "$SEQ"; then
  echo "error: sequence.sh must require RTRASH_DEMO_PIN (volume isolation)" >&2
  exit 1
fi
if ! grep -q '\$PIN\|RTRASH_DEMO_PIN' "$SEQ"; then
  echo "error: sequence must apply pin on suite commands" >&2
  exit 1
fi

if [[ "$dry" -eq 1 ]]; then
  echo "dry-run: not invoking asciinema/agg (committed assets left as-is)"
  exit 0
fi

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "error: missing '$1' on PATH (install it, or use --dry-run)" >&2
    exit 1
  fi
}

need asciinema
need agg

RTRASH_BIN="${RTRASH_BIN:-$(command -v rtrash || true)}"
if [[ -z "${RTRASH_BIN}" || ! -x "${RTRASH_BIN}" ]]; then
  for cand in \
    "${CARGO_TARGET_DIR:-$ROOT/target}/release/rtrash" \
    "$HOME/tmp/rtrash-target/release/rtrash" \
    "$ROOT/target/release/rtrash"; do
    if [[ -x "$cand" ]]; then
      RTRASH_BIN=$cand
      break
    fi
  done
fi
if [[ -z "${RTRASH_BIN}" || ! -x "${RTRASH_BIN}" ]]; then
  echo "error: rtrash binary not found (set RTRASH_BIN=... or cargo build --release)" >&2
  exit 1
fi
echo "using rtrash: $RTRASH_BIN ($("$RTRASH_BIN" --version 2>/dev/null || true))"

WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/rtrash-demo-XXXXXX")
cleanup() { rm -rf "$WORKDIR"; }
trap cleanup EXIT

export RTRASH_DEMO_WORK="$WORKDIR/work"
export XDG_DATA_HOME="$WORKDIR/xdg"
export HOME="$WORKDIR/home"
export RTRASH_DEMO_PIN="--trash-dir=${XDG_DATA_HOME}/Trash"
mkdir -p "$RTRASH_DEMO_WORK" "$XDG_DATA_HOME/Trash/files" "$XDG_DATA_HOME/Trash/info" "$HOME"
# Put rtrash first on PATH as plain name
ln -sfn "$RTRASH_BIN" "$WORKDIR/rtrash"
export PATH="$WORKDIR:$PATH"

echo "sandbox: WORK=$RTRASH_DEMO_WORK"
echo "sandbox: XDG_DATA_HOME=$XDG_DATA_HOME"
echo "sandbox: PIN=$RTRASH_DEMO_PIN"

# Preflight: unpinned empty must never run; pinned empty of empty pin is ok.
# Ensure put lands in pin and list only sees those files.
printf 'iso\n' >"$RTRASH_DEMO_WORK/iso.txt"
( cd "$RTRASH_DEMO_WORK" && rtrash put iso.txt )
n_list=$(rtrash list $RTRASH_DEMO_PIN | wc -l)
if [[ "$n_list" -lt 1 ]]; then
  echo "error: preflight list under pin is empty after put (isolation broken)" >&2
  exit 1
fi
# Must not list as many as unpinned multi-volume (heuristic: pin list == 1 here)
if [[ "$n_list" -ne 1 ]]; then
  echo "error: expected exactly 1 pinned trash item after one put, got $n_list" >&2
  rtrash list $RTRASH_DEMO_PIN >&2 || true
  exit 1
fi
rtrash empty --plain $RTRASH_DEMO_PIN
rm -f "$RTRASH_DEMO_WORK/iso.txt"
# After empty pin, work file was already put (gone). Good.

export COLUMNS=$COLS LINES=$ROWS TERM="${TERM:-xterm-256color}"
asciinema record \
  -f asciicast-v2 \
  --window-size "${COLS}x${ROWS}" \
  -c "bash '$SEQ'" \
  "$CAST" \
  --overwrite

# Postflight: cast must not mention foreign trash roots
if grep -E '/\.Trash-|pCloudDrive|/home/[^/]+/\.local/share/Trash' "$CAST" | grep -vq "$XDG_DATA_HOME"; then
  # allow only our sandbox path fragments
  if grep -E 'Removed [0-9]{2,} item|pCloudDrive|\.Trash-[0-9]+' "$CAST"; then
    echo "error: cast still shows multi-volume / bulk wipe signatures — recheck PIN" >&2
    exit 1
  fi
fi
if ! grep -q 'trash-dir\|--trash-dir' "$CAST" && ! grep -q 'Trash' "$CAST"; then
  : # pin may expand in banner; require "Removed 1 item" or similar for empty of one leftover
fi
if grep -E 'Removed 1[0-9] item|Removed [2-9][0-9] item' "$CAST"; then
  echo "error: cast empty removed double-digit items — isolation failed" >&2
  exit 1
fi

agg \
  --cols "$COLS" \
  --font-size 15 \
  --speed 1.15 \
  "$CAST" \
  "$GIF"

echo "wrote $CAST"
echo "wrote $GIF"
ls -la "$CAST" "$GIF"
