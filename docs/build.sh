#!/usr/bin/env bash
# Build the rtrash Sphinx/Shibuya site: org → rst → html
# Usage (from repo root or this directory):
#   ./docs/build.sh
# Optional: RTRASH_DOC_EXPORTER=emacs|pandoc  RTRASH_DOC_VENV=path
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VENV="${RTRASH_DOC_VENV:-$ROOT/.venv-docs}"
REQ="$ROOT/docs/requirements.txt"
SRC="$ROOT/docs/source"
BUILD="$ROOT/docs/build"

echo "==> 1/3 export org → rst"
bash "$ROOT/docs/scripts/export_org_to_rst.sh"

echo "==> 2/3 ensure Python doc deps in $VENV"
if [[ ! -d "$VENV" ]]; then
  python3 -m venv "$VENV"
fi
# shellcheck disable=SC1091
source "$VENV/bin/activate"
python -m pip install -q --upgrade pip
python -m pip install -q -r "$REQ"

echo "==> 3/3 sphinx-build (Shibuya) → $BUILD"
rm -rf "$BUILD"
sphinx-build -b html -n "$SRC" "$BUILD" 2>&1
# -W can be harsh on unknown roles; if too strict use without -W
# Prefer success: retry without -W only if needed — try strict first.

echo ""
echo "OK: open $BUILD/index.html"
echo "    python3 -m http.server -d $BUILD 8000   # optional"
