#!/usr/bin/env bash
# Build a relocatable rtrash release tarball (musl static when the target is installed).
# Usage (from repo root, typically on a builder host):
#   ./scripts/package-release.sh
#   ./scripts/package-release.sh x86_64-unknown-linux-musl
# Env:
#   OUT_DIR   output directory (default: dist/)
#   SKIP_TEST set to 1 to skip cargo test before packaging
#
# Naming is locked to Cargo.toml [package.metadata.binstall]:
#   archive:  {name}-{version}-{target}.tar.gz
#   bin path: {name}-{version}-{target}/bin/{bin}
#   release:  $repo/releases/download/v{version}/...
# Tag releases as v$VERSION so cargo-binstall can resolve assets without flags.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

TARGET="${1:-x86_64-unknown-linux-musl}"
OUT_DIR="${OUT_DIR:-$ROOT/dist}"
# First package.version only (not nested tables).
VERSION="$(awk -F\" '/^version = / { print $2; exit }' Cargo.toml)"
NAME="rtrash-${VERSION}-${TARGET}"

echo "==> target=${TARGET} version=${VERSION}"

if [[ "${SKIP_TEST:-0}" != "1" ]]; then
  echo "==> cargo test (host)"
  cargo test
fi

echo "==> cargo build --release --target ${TARGET}"
# Ensure musl target exists when requested; soft-fail message if rustup missing target.
if ! rustc --print target-list 2>/dev/null | grep -qx "${TARGET}"; then
  if command -v rustup >/dev/null 2>&1; then
    rustup target add "${TARGET}"
  else
    echo "error: target ${TARGET} not installed and rustup not available" >&2
    exit 1
  fi
fi

cargo build --release --target "${TARGET}"

BIN="$ROOT/target/${TARGET}/release/rtrash"
if [[ ! -x "$BIN" ]]; then
  echo "error: missing binary at ${BIN}" >&2
  exit 1
fi

STAGE="$OUT_DIR/${NAME}"
rm -rf "$STAGE"
mkdir -p "$STAGE/bin" "$STAGE/share/man/man1" \
  "$STAGE/share/bash-completion/completions" \
  "$STAGE/share/zsh/site-functions" \
  "$STAGE/share/fish/vendor_completions.d"

cp -a "$BIN" "$STAGE/bin/rtrash"
# Multi-call links (relative) so the tarball is relocatable.
for n in trash trash-put trash-empty trash-list trash-restore trash-rm; do
  ln -sf rtrash "$STAGE/bin/$n"
done

# Prefer generating assets from the built binary so they match the release.
"$STAGE/bin/rtrash" completions bash >"$STAGE/share/bash-completion/completions/rtrash"
"$STAGE/bin/rtrash" completions zsh >"$STAGE/share/zsh/site-functions/_rtrash"
"$STAGE/bin/rtrash" completions fish >"$STAGE/share/fish/vendor_completions.d/rtrash.fish"
# Fish loads completions/<command>.fish only for that command name; multi-call
# tools need their own files (same content as the shared rtrash.fish script).
for n in trash trash-put trash-empty trash-list trash-restore trash-rm; do
  ln -sf rtrash.fish "$STAGE/share/fish/vendor_completions.d/${n}.fish"
  # bash-completion also looks up by argv0:
  ln -sf rtrash "$STAGE/share/bash-completion/completions/$n"
done
"$STAGE/bin/rtrash" man >"$STAGE/share/man/man1/rtrash.1"

cat >"$STAGE/INSTALL.txt" <<INSTALL
rtrash ${VERSION} (${TARGET})

1. Copy bin/* somewhere on PATH (e.g. ~/.local/bin or /usr/local/bin).
2. Or run the included binary's setup against a prefix:

     ./bin/rtrash setup --prefix=\$HOME/.local --force

   That installs multi-call links, shell completions (bash/zsh/fish), and the man page.

3. Optional: alias or link rm -> rtrash only if you intentionally want rm to trash:
     rtrash setup --prefix=\$HOME/.local --with-rm --force
INSTALL

mkdir -p "$OUT_DIR"
TAR="$OUT_DIR/${NAME}.tar.gz"
tar -C "$OUT_DIR" -czf "$TAR" "$NAME"
echo "OK: $TAR"
ls -la "$TAR" "$STAGE/bin/rtrash"
