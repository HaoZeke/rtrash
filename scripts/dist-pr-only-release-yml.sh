#!/usr/bin/env bash
# After `dist generate --mode ci`, keep cargo-dist CI as PR-only plan.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
python3 - <<'PY'
from pathlib import Path
import re
p = Path(".github/workflows/release.yml")
text = p.read_text()
new_on = """# LOCAL POLICY: tag GitHub Releases are owned by release-musl.yml
# (package-release.sh / cargo-binstall layout). This workflow is PR-only
# `dist plan`. Re-run after `dist generate --mode ci`.
on:
  pull_request:

"""
text2, n = re.subn(r"^on:\n(?:.*\n)*?(?=^jobs:)", new_on, text, count=1, flags=re.M)
if n != 1:
    raise SystemExit(f"rewrite failed n={n}")
text2 = text2.replace("name: Release\n", "name: cargo-dist plan\n", 1)
# idempotent if already renamed
if "name: cargo-dist plan\n" not in text2 and "name: Release\n" not in text2:
    text2 = re.sub(r"^name: .*\n", "name: cargo-dist plan\n", text2, count=1, flags=re.M)
p.write_text(text2)
print("ok", p)
PY
