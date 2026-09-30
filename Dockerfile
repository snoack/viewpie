# Viewpie scans out onto KMS overlay planes, so this image needs the host's DRM
# and V4L2 devices passed in. It cannot render anything in an ordinary
# sandboxed container:
#
#   docker run -d --restart unless-stopped \
#       --device /dev/dri --device /dev/video10 \
#       -v /etc/viewpie:/etc/viewpie:ro viewpie
#
# Device nodes and nothing else: the screen, and the decoder for each codec
# the board has a block for. The numbers differ per board, and compose.yaml
# lists them; passing /dev/dri whole leaves the choice of card to viewpie.
# Neither privileged nor a bind of /dev is needed; /sys is mounted in every
# container already, which is where the decoder is found by name.
#
# --restart is the container's half of what the systemd unit does for the
# package: a crash has to bring the wall back. It cannot be baked into an
# image, so it belongs on the run command, or in the compose file beside this
# Dockerfile.
#
# The image installs the same .deb a Pi installs rather than copying the binary
# and writing an entry point of its own. Those two descriptions of the package
# drift otherwise, and the drift is invisible until something is missing at
# runtime on hardware.
#
# Build the .deb first with scripts/build-deb.sh, and pass it in:
#
#   docker build --platform linux/arm64 \
#       --build-arg DEB=dist/viewpie_0.1.0-1_arm64.deb -t viewpie .
#
# --platform is not optional: the package depends on the target's libav and
# libdrm, and those are not installable in a base image of another
# architecture. scripts/build-image.sh pairs each package with its platform.
FROM debian:trixie-slim

ARG DEB
COPY ${DEB} /tmp/viewpie.deb

# Raspberry Pi OS's libav, not Debian's. Debian's builds have no stateless
# V4L2 decoder, so H.265 on a Pi 4 or Pi 5 falls back to software inside the
# container however many device nodes it is given, which is most of the cost
# of a wall. The Pi archive's packages carry a higher epoch (8: against 7:),
# so apt prefers them wherever both exist without needing a pin.
# The archive's signing key is from 2012 and self-signs with SHA1, which
# trixie's verifier rejects by policy. The policy is relaxed for apt's
# verifier alone, which is narrower than the alternative of turning signature
# checking off to get these packages into a Debian base image.
# Debian's libav satisfies the package's dependencies just as well, so nothing
# in apt would notice if the Pi archive stopped winning. viewpie --check-libav
# asks libav itself and fails the build instead.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && curl -fsSL -o /usr/share/keyrings/raspberrypi.gpg.key \
        http://archive.raspberrypi.com/debian/raspberrypi.gpg.key \
    && printf 'deb [signed-by=/usr/share/keyrings/raspberrypi.gpg.key] http://archive.raspberrypi.com/debian/ trixie main\n' \
        > /etc/apt/sources.list.d/raspi.list \
    && printf '[hash_algorithms]\nsha1.second_preimage_resistance = "always"\nsha1.collision_resistance = "always"\n' \
        > /etc/apt-sequoia-policy.toml \
    && SEQUOIA_CRYPTO_POLICY=/etc/apt-sequoia-policy.toml apt-get update \
    && SEQUOIA_CRYPTO_POLICY=/etc/apt-sequoia-policy.toml \
        apt-get install -y --no-install-recommends /tmp/viewpie.deb \
    && viewpie --check-libav \
    && apt-get purge -y curl && apt-get autoremove -y \
    && rm -f /tmp/viewpie.deb /etc/apt-sequoia-policy.toml \
    && rm -rf /var/lib/apt/lists/*

# Not root. The package made this user, in the video group, and every device
# the wall opens is group video: the card, the decoders and their media nodes.
# That group is 44 on Debian and Raspberry Pi OS alike, so the host's nodes
# and the image's user agree without a --group-add. DRM master needs no
# privilege either, only to be the first to open the card. What root would
# add is a process parsing streams off the network with the run of the
# container.
USER viewpie
ENTRYPOINT ["/usr/bin/viewpie", "--config", "/etc/viewpie/viewpie.yaml"]
