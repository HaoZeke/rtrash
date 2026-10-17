#!/usr/bin/env sh
# Compile and lint the Windows, macOS and musl code from Linux, the way
# CI's native and musl jobs will see it. Needs the three targets once:
#   rustup target add x86_64-pc-windows-gnu aarch64-apple-darwin x86_64-unknown-linux-musl
# and musl-gcc (Debian/Ubuntu: musl-tools) for mimalloc's C code on musl.
# Linking is not needed: clippy stops after type checking.
set -eu
for target in x86_64-pc-windows-gnu aarch64-apple-darwin x86_64-unknown-linux-musl; do
    for features in "" "--no-default-features"; do
        echo "cross-clippy: $target $features"
        # shellcheck disable=SC2086
        cargo clippy --locked --all-targets --target "$target" $features -- -D warnings
    done
done
