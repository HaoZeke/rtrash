#!/usr/bin/env bash
# Build a relocatable rtrash release tarball (musl static when the target is installed).
# Usage (from repo root, typically on a builder host):
#   ./scripts/package-release.sh
#   ./scripts/package-release.sh x86_64-unknown-linux-musl
#   ./scripts/package-release.sh aarch64-unknown-linux-musl
#   ./scripts/package-release.sh x86_64-apple-darwin
#   ./scripts/package-release.sh aarch64-apple-darwin
#   ./scripts/package-release.sh x86_64-pc-windows-msvc   # on a Windows host, from bash
#   TARGETS="x86_64-unknown-linux-musl aarch64-unknown-linux-musl" ./scripts/package-release.sh --all
# Env:
#   OUT_DIR   output directory (default: dist/)
#   SKIP_TEST set to 1 to skip cargo test before packaging
#
# Naming is locked to Cargo.toml [package.metadata.binstall]:
#   archive:  {name}-{version}-{target}.tar.gz
#   bin path: {name}-{version}-{target}/bin/{bin}
#   release:  $repo/releases/download/v{version}/...
# Tag releases as v$VERSION so cargo-binstall can resolve assets without flags.
# Windows is the exception: it ships the asset name cargo-dist plans,
#   rtrash-x86_64-pc-windows-msvc.zip (+ .sha256), with rtrash.exe,
#   CHANGELOG.md, LICENSE and README.md at the archive root.
# Supported release targets (CI): x86_64/aarch64 Linux musl, Apple Darwin and
# x86_64 Windows MSVC.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

DEFAULT_TARGETS=(x86_64-unknown-linux-musl aarch64-unknown-linux-musl)
OUT_DIR="${OUT_DIR:-$ROOT/dist}"
# First package.version only (not nested tables).
VERSION="$(awk -F\" '/^version = / { print $2; exit }' Cargo.toml)"

if [[ "${1:-}" == "--all" ]]; then
  # shellcheck disable=SC2206
  TARGETS=(${TARGETS:-${DEFAULT_TARGETS[*]}})
  shift || true
elif [[ -n "${1:-}" ]]; then
  TARGETS=("$1")
else
  # Single default on this host arch when possible.
  case "$(uname -s):$(uname -m)" in
    Darwin:arm64) TARGETS=(aarch64-apple-darwin) ;;
    Darwin:*) TARGETS=(x86_64-apple-darwin) ;;
    Linux:aarch64|Linux:arm64) TARGETS=(aarch64-unknown-linux-musl) ;;
    *) TARGETS=(x86_64-unknown-linux-musl) ;;
  esac
fi

if [[ "${SKIP_TEST:-0}" != "1" ]]; then
  echo "==> cargo test (host)"
  cargo test
fi

write_sha256() {
  local dir="$1" file="$2"
  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$dir" && sha256sum "$file" >"${file}.sha256")
  elif command -v shasum >/dev/null 2>&1; then
    (cd "$dir" && shasum -a 256 "$file" >"${file}.sha256")
  else
    echo "error: neither sha256sum nor shasum is available" >&2
    return 1
  fi
}

# Runs after `cargo build --release --target TARGET` has produced rtrash.exe.
package_windows() {
  local TARGET="$1"
  local NAME="rtrash-${TARGET}"
  local BIN="$ROOT/target/${TARGET}/release/rtrash.exe"
  if [[ ! -f "$BIN" ]]; then
    echo "error: missing binary at ${BIN}" >&2
    return 1
  fi

  local STAGE="$OUT_DIR/${NAME}"
  rm -rf "$STAGE"
  mkdir -p "$STAGE"
  cp "$BIN" "$STAGE/rtrash.exe"
  cp "$ROOT/CHANGELOG.md" "$ROOT/LICENSE" "$ROOT/README.md" "$STAGE/"

  local ZIP="$OUT_DIR/${NAME}.zip"
  [[ "$ZIP" = /* ]] || ZIP="$ROOT/$ZIP"
  rm -f "$ZIP"
  if command -v zip >/dev/null 2>&1; then
    (cd "$STAGE" && zip -qr "$ZIP" .)
  elif command -v python3 >/dev/null 2>&1; then
    python3 -c 'import shutil, sys; shutil.make_archive(sys.argv[1], "zip", sys.argv[2])' \
      "$OUT_DIR/${NAME}" "$STAGE"
  elif command -v python >/dev/null 2>&1; then
    python -c 'import shutil, sys; shutil.make_archive(sys.argv[1], "zip", sys.argv[2])' \
      "$OUT_DIR/${NAME}" "$STAGE"
  else
    echo "error: neither zip nor python is available to make ${ZIP}" >&2
    return 1
  fi
  write_sha256 "$OUT_DIR" "${NAME}.zip"
  echo "OK: $ZIP"
  echo "OK: ${ZIP}.sha256"
  ls -la "$ZIP" "${ZIP}.sha256" "$STAGE/rtrash.exe"
}

package_one() {
  local TARGET="$1"
  local NAME="rtrash-${VERSION}-${TARGET}"
  echo "==> target=${TARGET} version=${VERSION}"

  echo "==> cargo build --release --target ${TARGET} -p rtrash"
  if ! rustc --print target-list 2>/dev/null | grep -qx "${TARGET}"; then
    if command -v rustup >/dev/null 2>&1; then
      rustup target add "${TARGET}"
    else
      echo "error: target ${TARGET} not installed and rustup not available" >&2
      return 1
    fi
  fi

  # musl builds link mimalloc (C); point cc-rs at musl-gcc from musl-tools.
  local cc_var="CC_${TARGET//-/_}"
  if [[ "${TARGET}" == *-musl && -z "${!cc_var:-}" ]] && command -v musl-gcc >/dev/null 2>&1; then
    export "${cc_var}=musl-gcc"
  fi
  # Name the CLI package. The optional rtrash-hud member is not a default
  # member and is not part of this tarball.
  cargo build --release --target "${TARGET}" -p rtrash

  if [[ "${TARGET}" == *-windows-* ]]; then
    package_windows "$TARGET"
    return
  fi

  local BIN="$ROOT/target/${TARGET}/release/rtrash"
  if [[ ! -x "$BIN" ]]; then
    echo "error: missing binary at ${BIN}" >&2
    return 1
  fi

  local STAGE="$OUT_DIR/${NAME}"
  rm -rf "$STAGE"
  mkdir -p "$STAGE/bin" "$STAGE/share/man/man1" \
    "$STAGE/share/bash-completion/completions" \
    "$STAGE/share/zsh/site-functions" \
    "$STAGE/share/fish/vendor_completions.d"

  cp -a "$BIN" "$STAGE/bin/rtrash"
  for n in trash trash-put trash-empty trash-list trash-restore trash-rm; do
    ln -sf rtrash "$STAGE/bin/$n"
  done

  "$STAGE/bin/rtrash" completions bash >"$STAGE/share/bash-completion/completions/rtrash"
  "$STAGE/bin/rtrash" completions zsh >"$STAGE/share/zsh/site-functions/_rtrash"
  "$STAGE/bin/rtrash" completions fish >"$STAGE/share/fish/vendor_completions.d/rtrash.fish"
  for n in trash trash-put trash-empty trash-list trash-restore trash-rm; do
    ln -sf rtrash.fish "$STAGE/share/fish/vendor_completions.d/${n}.fish"
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
  local TAR="$OUT_DIR/${NAME}.tar.gz"
  tar -C "$OUT_DIR" -czf "$TAR" "$NAME"
  local SUM="${TAR}.sha256"
  local tar_name
  tar_name="$(basename "$TAR")"
  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$OUT_DIR" && sha256sum "$tar_name" >"${tar_name}.sha256")
  elif command -v shasum >/dev/null 2>&1; then
    (cd "$OUT_DIR" && shasum -a 256 "$tar_name" >"${tar_name}.sha256")
  else
    echo "error: neither sha256sum nor shasum is available" >&2
    return 1
  fi
  echo "OK: $TAR"
  echo "OK: $SUM"
  ls -la "$TAR" "$SUM" "$STAGE/bin/rtrash"
}

for t in "${TARGETS[@]}"; do
  package_one "$t"
done
