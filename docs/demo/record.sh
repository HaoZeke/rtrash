#!/usr/bin/env bash
# Rebuild docs/demo/rtrash-quickstart.{cast,gif} from sequence.sh using a real rtrash.
#
# Usage:
#   ./docs/demo/record.sh              # record + gif (needs asciinema, agg, rtrash)
#   ./docs/demo/record.sh --dry-run    # print sequence only; exit 0 without tools
#   RTRASH_BIN=/path/to/rtrash ./docs/demo/record.sh
#
# Always isolates trash via XDG_DATA_HOME / HOME under a temp dir.
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
grep -E "run '|banner \"" "$SEQ" || true
echo "---"

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
  # Prefer a release build next to the tree when present.
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
export PATH="$(dirname "$RTRASH_BIN"):$PATH"
mkdir -p "$RTRASH_DEMO_WORK" "$XDG_DATA_HOME" "$HOME"
# Ensure the demo hits *this* binary even if multi-call names exist.
ln -sfn "$RTRASH_BIN" "$WORKDIR/bin-rtrash"
export PATH="$WORKDIR:$PATH"
# Put rtrash first as plain name
ln -sfn "$RTRASH_BIN" "$WORKDIR/rtrash"

# Headless-friendly recording (asciinema 3.x works without a real TTY).
export COLUMNS=$COLS LINES=$ROWS TERM="${TERM:-xterm-256color}"
asciinema record \
  -f asciicast-v2 \
  --window-size "${COLS}x${ROWS}" \
  -c "bash '$SEQ'" \
  "$CAST" \
  --overwrite

agg \
  --cols "$COLS" \
  --font-size 15 \
  --speed 1.15 \
  "$CAST" \
  "$GIF"

echo "wrote $CAST"
echo "wrote $GIF"
ls -la "$CAST" "$GIF"
