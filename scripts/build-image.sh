#!/bin/sh
# Build the runtime image for a Pi from the matching .deb.
#
#   scripts/build-image.sh                 # both architectures
#   scripts/build-image.sh arm64           # just the Pi 5
#
# Expects scripts/build-deb.sh to have run: the image installs that package
# rather than repeating what it contains, so the two cannot drift.
#
# Unlike the package build this cannot cross-compile -- apt has to run as the
# target architecture to resolve the package's dependencies -- so it needs
# qemu's handlers:
#
#   docker run --privileged --rm tonistiigi/binfmt --install all
set -eu

cd "$(dirname "$0")/.."
targets="${*:-armhf arm64}"

for t in $targets; do
    case "$t" in
        armhf) platform=linux/arm/v7 ;;
        arm64) platform=linux/arm64 ;;
        *) echo "unknown architecture $t (expected armhf or arm64)" >&2; exit 2 ;;
    esac
    # Found rather than reconstructed from the version and revision: those
    # live in Cargo.toml and debian/changelog, and guessing them here meant a
    # 0.1.0-2 package reported as missing while it sat in dist/.
    deb=$(ls dist/viewpie_*_"$t".deb 2>/dev/null | tail -1)
    [ -n "$deb" ] || { echo "no dist/viewpie_*_$t.deb; run scripts/build-deb.sh first" >&2; exit 1; }
    echo "=== building viewpie:$t from $deb ==="
    # --pull, or a base image cached for another architecture is reused and
    # the target's libraries turn out not to be installable.
    docker build --pull --platform "$platform" \
        --build-arg "DEB=$deb" -t "viewpie:$t" .
done
