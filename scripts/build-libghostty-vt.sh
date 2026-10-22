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
# With ZIG_PREFETCH_WITH_CURL=1, Ghostty's Zig dependencies are downloaded
# with curl and added to Zig's cache with `zig fetch <file>` before building.
# Zig's own resolver fails in some build containers (snapcraft's LXD
# instances fail with ResolvConfParseFailed), and curl does not.
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

# Downloads every dependency URL listed in build.zig.zon files under the given
# directories, until no new ones appear (fetched packages list their own).
prefetch_deps() {
    cache=$(zig env | sed -n 's/.*\.global_cache_dir = "\(.*\)",/\1/p; s/.*"global_cache_dir": *"\([^"]*\)".*/\1/p' | head -n1)
    downloads=$(mktemp -d)
    seen="$downloads/seen"
    : >"$seen"
    while :; do
        urls=$(find "$@" ${cache:+"$cache/p"} -name build.zig.zon \
                   -not -path '*/test/*' -not -path '*/example/*' 2>/dev/null \
               | xargs -r grep -ho '\.url = "https://[^"]*"' \
               | sed 's/.*"\(.*\)"/\1/' | grep -v COMMIT | sort -u \
               | grep -vxF -f "$seen" || true)
        [ -n "$urls" ] || break
        for url in $urls; do
            echo "$url" >>"$seen"
            file="$downloads/$(basename "$url")"
            curl -fsSL --retry 3 -o "$file" "$url"
            zig fetch "$file" >/dev/null
            rm -f "$file"
        done
    done
    rm -rf "$downloads"
}

cd "$src"
if [ "${ZIG_PREFETCH_WITH_CURL:-0}" = 1 ]; then
    prefetch_deps "$src/build.zig.zon" "$src/pkg"
fi
zig build -Demit-lib-vt=true -Doptimize=ReleaseFast --prefix "$prefix"
echo "libghostty-vt installed to $prefix"
