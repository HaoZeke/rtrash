#!/usr/bin/env bash
# Export docs/orgmode/*.org → docs/source/*.rst
# Prefer pandoc (available on many hosts); optional: RTRASH_DOC_EXPORTER=emacs
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"
ORG_DIR="docs/orgmode"
OUT_DIR="docs/source"
mkdir -p "$OUT_DIR"

exporter="${RTRASH_DOC_EXPORTER:-auto}"
if [[ "$exporter" == "auto" ]]; then
  if command -v pandoc >/dev/null 2>&1; then
    exporter=pandoc
  elif command -v emacs >/dev/null 2>&1; then
    exporter=emacs
  else
    echo "error: need pandoc or emacs to export org → rst" >&2
    exit 1
  fi
fi

echo "export_org_to_rst: using $exporter"
if [[ "$exporter" == "emacs" ]]; then
  emacs --batch -l docs/export.el
else
  shopt -s nullglob
  files=("$ORG_DIR"/*.org)
  if ((${#files[@]} == 0)); then
    echo "error: no org files in $ORG_DIR" >&2
    exit 1
  fi
  for org in "${files[@]}"; do
    base="$(basename "$org" .org)"
    pandoc -f org -t rst -o "$OUT_DIR/${base}.rst" "$org"
    echo "  wrote $OUT_DIR/${base}.rst"
  done
fi

python3 docs/scripts/fix_doc_links.py

python3 - << 'PY'
from pathlib import Path
idx = Path("docs/source/index.rst")
if not idx.is_file():
    raise SystemExit("missing docs/source/index.rst after export")
text = idx.read_text(encoding="utf-8")
text = text.replace("trash put.\n.. toctree::", "trash put.\n\n.. toctree::")
if ".. toctree::" not in text:
    text += """

.. toctree::
   :maxdepth: 1
   :caption: Guides
   :hidden:

   getting-started
   architecture
   benchmarks
   bindings
"""
idx.write_text(text, encoding="utf-8")
print("index.rst ready")
PY
