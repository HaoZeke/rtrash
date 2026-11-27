#!/usr/bin/env sh
# Compile and lint the Windows, macOS, musl and FreeBSD code from Linux,
# the way CI's native and clippy jobs will see it. Needs the four targets
# once:
#   rustup target add x86_64-pc-windows-gnu aarch64-apple-darwin \
#     x86_64-unknown-linux-musl x86_64-unknown-freebsd
# and musl-gcc (Debian/Ubuntu: musl-tools) for mimalloc's C code on musl.
# FreeBSD needs no C compiler: mimalloc is for musl only.
# Linking is not needed: clippy stops after type checking.
set -eu
for target in x86_64-pc-windows-gnu aarch64-apple-darwin x86_64-unknown-linux-musl \
    x86_64-unknown-freebsd; do
    for features in "" "--no-default-features"; do
        echo "cross-clippy: $target $features"
        # shellcheck disable=SC2086
        cargo clippy --locked --all-targets --target "$target" $features -- -D warnings
    done
    # Not a default member. Windows drops the unix-only HUD tests; a helper
    # they alone use is dead_code there unless it is cfg-gated.
    echo "cross-clippy: $target rtrash-hud"
    cargo clippy --locked -p rtrash-hud --all-targets --target "$target" -- -D warnings
done
