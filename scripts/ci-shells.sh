#!/usr/bin/env bash
# Put pinned elvish, fish and nu into the folder given for the completion
# tests in tests/completions_parity.rs, on a Linux x86_64 or macOS runner.
# Each download is checked against its SHA-256, and files already in the
# folder (a restored cache) are reused. Add the folder to PATH afterwards;
# on macOS, fish installs from its package into /usr/local/bin.
#   scripts/ci-shells.sh folder
set -euo pipefail
dir=${1:?usage: scripts/ci-shells.sh folder}
elvish=v0.21.0
nu=0.116.1
fish=4.9.3
case "$(uname -s)-$(uname -m)" in
Linux-x86_64)
    e_os=linux-amd64
    e_sum=d78f961addda5d1d3bdd0c1a70ad23ca77a308c27008fb8b9ec83777b58f7572
    n_os=x86_64-unknown-linux-gnu
    n_sum=d6d8ace4be491ed8abba4026e73b60c0470b4671d7039307e7d772282ef863da
    f_file=fish-$fish-linux-x86_64.tar.xz
    f_sum=8f643d10ad1abb0ef7072764c01ceb5cf61b2d373fda400cf60f0bad69b7e095
    ;;
Darwin-arm64)
    e_os=darwin-arm64
    e_sum=9fb02a6900dc1e05b4093cf6326201e326323098d3279fd57cedbdc2381b436d
    n_os=aarch64-apple-darwin
    n_sum=302fb3dfc5d44e7fe0d5b8227be8e75b4c3956ca8d4804234e5c68fbdd3e4917
    f_file=fish-$fish.pkg
    f_sum=fb489929e25e66b8c026a1da8935f68f828134e115a1706f360ed73149b843a9
    ;;
Darwin-x86_64)
    e_os=darwin-amd64
    e_sum=460f26b45c19810acb7175f3f0c26345784770d17aaeaf339711d244f2f4d40a
    n_os=x86_64-apple-darwin
    n_sum=44306f0766f2be0e763d0c7a6a6774537589a335797a199d8e5b473d766ec6bd
    f_file=fish-$fish.pkg
    f_sum=fb489929e25e66b8c026a1da8935f68f828134e115a1706f360ed73149b843a9
    ;;
*)
    echo "ci-shells: no pinned shells for $(uname -sm)" >&2
    exit 1
    ;;
esac

mkdir -p "$dir"
cd "$dir"

# fetch URL FILE SHA256: download FILE unless it is there, then check it.
fetch() {
    [ -f "$2" ] || curl -fsSL --retry 3 -o "$2" "$1"
    echo "$3  $2" | shasum -a 256 -c -
}

fetch "https://dl.elv.sh/$e_os/elvish-$elvish.tar.gz" "elvish-$elvish-$e_os.tar.gz" "$e_sum"
tar -xzf "elvish-$elvish-$e_os.tar.gz" elvish

n_dl=https://github.com/nushell/nushell/releases/download
fetch "$n_dl/$nu/nu-$nu-$n_os.tar.gz" "nu-$nu-$n_os.tar.gz" "$n_sum"
tar -xzf "nu-$nu-$n_os.tar.gz" --strip-components=1 "nu-$nu-$n_os/nu"

f_dl=https://github.com/fish-shell/fish-shell/releases/download
fetch "$f_dl/$fish/$f_file" "$f_file" "$f_sum"
case $f_file in
*.pkg) sudo installer -pkg "$f_file" -target / ;;
*) tar -xJf "$f_file" fish ;;
esac
