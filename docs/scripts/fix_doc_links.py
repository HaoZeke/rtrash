#!/usr/bin/env python3
"""Rewrite org/rst file hyperlinks in exported RST to Sphinx :doc: roles."""

from __future__ import annotations

import re
import sys
from pathlib import Path

# Pandoc anonymous links: `label <foo.org>`__  and single `_` form.
_LINK = re.compile(
    r"`([^`<]+)\s+<((?![a-z][a-z0-9+.-]*:)[^>]+?\.(?:rst|org))>`__?"
)


def fix_text(t: str) -> str:
    def repl(m: re.Match[str]) -> str:
        label = m.group(1).strip().replace("\n", " ")
        target = m.group(2)
        name = Path(target).name
        if name.endswith(".org"):
            name = name[: -len(".org")]
        elif name.endswith(".rst"):
            name = name[: -len(".rst")]
        return f":doc:`{label} <{name}>`"

    t = _LINK.sub(repl, t)
    # Pandoc often glues the next section title to a directive; ensure blank line
    # after grid / toctree blocks that lost spacing.
    t = re.sub(r"( +bindings)\n(Source & license)", r"\1\n\n\2", t)
    t = re.sub(r"(trash put\.)\n(\.\. toctree::)", r"\1\n\n\2", t)
    # Blank line after raw:: html hero so the next section is not "unexpected unindent".
    t = re.sub(r"(</div>\n)(Why rtrash)", r"\1\n\2", t)
    return t


def main() -> int:
    src = Path("docs/source")
    if not src.is_dir():
        print("docs/source missing", file=sys.stderr)
        return 1
    n = 0
    for path in sorted(src.glob("*.rst")):
        raw = path.read_text(encoding="utf-8")
        fixed = fix_text(raw)
        if fixed != raw:
            path.write_text(fixed, encoding="utf-8")
            n += 1
            print(f"fixed links in {path.name}")
    print(f"fix_doc_links: updated {n} file(s)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
