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
#
# With ZIG_CLEAN_RESOLV_CONF=1 (needs root), Zig runs in a private mount
# namespace that sees an /etc/resolv.conf holding only plain IPv4 nameserver
# lines. Zig 0.17's resolv.conf parser fails (ResolvConfParseFailed) on some
# files that glibc accepts, such as the one in snapcraft's LXD build instance.
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
set -- zig build -Demit-lib-vt=true -Doptimize=ReleaseFast --prefix "$prefix"
if [ "${ZIG_CLEAN_RESOLV_CONF:-0}" = 1 ]; then
    resolv=$(mktemp)
    grep -E '^nameserver[[:space:]]+[0-9.]+[[:space:]]*$' /etc/resolv.conf >"$resolv" || true
    [ -s "$resolv" ] || echo "nameserver 127.0.0.53" >"$resolv"
    unshare --mount sh -c 'mount --bind "$0" /etc/resolv.conf && exec "$@"' "$resolv" "$@"
    rm -f "$resolv"
else
    "$@"
fi
echo "libghostty-vt installed to $prefix"
