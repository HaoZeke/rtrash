#!/usr/bin/env sh
# Compile and lint the Windows and macOS code from Linux, the way CI's
# native jobs will see it. Needs the two targets once:
#   rustup target add x86_64-pc-windows-gnu aarch64-apple-darwin
# Linking is not needed: clippy stops after type checking.
set -eu
for target in x86_64-pc-windows-gnu aarch64-apple-darwin; do
    for features in "" "--no-default-features"; do
        echo "cross-clippy: $target $features"
        # shellcheck disable=SC2086
        cargo clippy --locked --all-targets --target "$target" $features -- -D warnings
    done
done
