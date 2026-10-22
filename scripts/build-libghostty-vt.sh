#!/bin/sh
# Builds libghostty-vt (the terminal emulation core of Ghostty) into a prefix
# that build.rs finds through pkg-config.
#
#   scripts/build-libghostty-vt.sh [prefix]     # default: ./vendor/ghostty-vt
#   export PKG_CONFIG_PATH=$PWD/vendor/ghostty-vt/share/pkgconfig
#   export LD_LIBRARY_PATH=$PWD/vendor/ghostty-vt/lib
#
# Needs git and Zig (the version in Ghostty's build.zig.zon; currently 0.17).
# The libghostty-vt C API is not yet stable, so the commit is pinned.
set -eu

GHOSTTY_REPO=https://github.com/ghostty-org/ghostty.git
GHOSTTY_COMMIT=f54340ad77814fe319d136e5e915d6a48de8dd23

prefix=$(realpath -m "${1:-vendor/ghostty-vt}")
src="$prefix.src"

if [ ! -d "$src/.git" ]; then
    git init -q "$src"
    git -C "$src" remote add origin "$GHOSTTY_REPO"
fi
git -C "$src" fetch -q --depth 1 origin "$GHOSTTY_COMMIT"
git -C "$src" checkout -q FETCH_HEAD

cd "$src"
zig build -Demit-lib-vt=true -Doptimize=ReleaseFast --prefix "$prefix"
echo "libghostty-vt installed to $prefix"
