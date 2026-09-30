#!/bin/sh
# Build viewpie's Debian packages and leave them in dist/.
#
#   scripts/build-deb.sh              # both architectures
#   scripts/build-deb.sh arm64        # just the Pi 5
#
# Needs only Docker: the build cross-compiles, so nothing is emulated and
# nothing has to be installed on a Pi.
set -eu

cd "$(dirname "$0")/.."
targets="${*:-armhf arm64}"
mkdir -p dist

for t in $targets; do
    case "$t" in
        armhf|arm64) ;;
        *) echo "unknown architecture $t (expected armhf or arm64)" >&2; exit 2 ;;
    esac
    echo "=== building for $t ==="
    docker build --build-arg "ARCH=$t" -f Dockerfile.build \
        -t "viewpie-build:$t" .
    # docker cp needs a container, and create makes one without running it.
    cid=$(docker create "viewpie-build:$t")
    docker cp "$cid:/out/." dist/
    docker rm "$cid" >/dev/null
done

ls -la dist/*.deb
