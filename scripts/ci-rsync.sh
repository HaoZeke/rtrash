#!/usr/bin/env bash
# Build pinned rsync 3 into the folder given. A macOS runner's own rsync
# is openrsync or 2.6.9, so filter_from_leaves_out_what_rsync_leaves_out
# (tests/du.rs) would skip its rsync half there. Homebrew bottles rsync
# for Apple silicon only, so an Intel runner would build it and its
# libraries from source; this build leaves out the optional libraries
# and takes seconds. The tarball is checked against its SHA-256, and a
# binary already in the folder (a restored cache) is reused. Add the
# folder to PATH afterwards.
#   scripts/ci-rsync.sh folder
set -euo pipefail
dir=${1:?usage: scripts/ci-rsync.sh folder}
version=3.5.1
sum=c55f9c9dc10fb8bec397b399a0fdded53cc9a2d8e30891bb0d63724d25c37bef
mkdir -p "$dir"
cd "$dir"
if [ -x rsync ]; then
    case $(./rsync --version) in
    *" version $version "*) exit 0 ;;
    esac
fi
tarball=rsync-$version.tar.gz
[ -f "$tarball" ] || curl -fsSL --retry 3 -o "$tarball" \
    "https://download.samba.org/pub/rsync/src/$tarball"
echo "$sum  $tarball" | shasum -a 256 -c -
rm -rf "rsync-$version"
tar -xzf "$tarball"
(
    cd "rsync-$version"
    ./configure --disable-openssl --disable-xxhash --disable-zstd --disable-lz4 \
        --disable-idn --disable-md2man --with-included-popt --with-included-zlib \
        >build.log 2>&1 &&
        make -j"$(getconf _NPROCESSORS_ONLN)" rsync >>build.log 2>&1 ||
        { cat build.log; exit 1; }
)
cp "rsync-$version/rsync" rsync
rm -rf "rsync-$version"
