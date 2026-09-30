# viewpie

A camera wall for a Raspberry Pi. Point it at your RTSP cameras and it fills
the screen with them — no desktop, no browser, no window manager.

- **Every camera at full frame rate.** The whole wall goes up in one atomic
  KMS commit, so nine feeds advance on the same vsync instead of dividing the
  display between them.
- **Cheap.** Hardware-decoded frames go from the decoder to the screen without
  the CPU touching a pixel: nine H.264 cameras at 1080p cost 18% of one core
  on a Pi 3, at each camera's full frame rate.
- **Arrange it how you like.** A plain grid, or a layout you draw, with big
  cameras and small ones. A viewport can rotate through several feeds.
- **Leave it running.** A camera that drops out is reconnected, its viewport
  blanked meanwhile; a crash is restarted by systemd or Docker.
- **Two screens.** Both are driven from the same loop and need not share a
  refresh rate.

Decoding is ffmpeg/libav; output is a KMS atomic committer talking to the
kernel directly, with the decoder's dmabuf handed straight to a plane.

## Installing

Download the `.deb` for your architecture from the
[latest release](https://github.com/snoack/viewpie/releases/latest). Run
`dpkg --print-architecture` if you are unsure which that is:

```sh
sudo apt install ./viewpie_*_arm64.deb   # 64-bit
sudo apt install ./viewpie_*_armhf.deb   # 32-bit
```

The service is enabled for you, and waits for a wall to show. Describe one in
`/etc/viewpie/viewpie.yaml` (see below), then start it:

```sh
sudo systemctl restart viewpie
```

The file holds camera credentials, so the package keeps it readable only by
the `viewpie` user, and removes it on purge.

The wall waits rather than giving up on anything that might yet arrive: a
screen that is off, and cameras that are unreachable, are retried until they
appear, and a crash is restarted on a backoff from five seconds to a minute.
A configuration it cannot start from stops the service instead, so the error
stays legible in `systemctl status viewpie` rather than scrolling past every
five seconds.

### Or with Docker

The image installs the same package from the same archives, so a container
decodes in hardware exactly as the package does. It costs about two points of
a core more: 15% against 13% for a nine-camera wall on a Pi 4, measured four
times each. Keep the lines for your board and delete the rest:

```yaml
services:
  viewpie:
    image: ghcr.io/snoack/viewpie:latest
    restart: unless-stopped
    devices:
      # The screen. /dev/dri on its own leaves the choice to viewpie, which
      # picks the card that has a connected connector.
      - /dev/dri/card0      # Pi 3
      - /dev/dri/card1      # Pi 4, Pi 5

      # The decoders. Drop them both to decode everything in software.
      - /dev/video10        # H.264: Pi 3, Pi 4
      - /dev/video19        # H.265: Pi 4, Pi 5

      # H.265 is stateless and driven through the Request API, so it needs
      # the media node beside its decoder as well. Which number that is
      # changes from boot to boot, so pass every one the board has and let
      # viewpie find it -- but only those: see below.
      - /dev/media0         # Pi 4, Pi 5
      - /dev/media1
      - /dev/media2
      - /dev/media3         # Pi 4 only

      # HDMI-CEC, for the remote and pause_off_screen: one per HDMI port.
      - /dev/cec0
      - /dev/cec1           # Pi 4, Pi 5

    volumes:
      - /etc/viewpie:/etc/viewpie:ro
```

```sh
docker compose up -d
```

Device nodes and nothing else: no privileged, no bind of `/dev`. The `video`
numbers are fixed by the drivers, but the `media` ones are handed out in
registration order and move between boots: on one Pi 4 the H.265 decoder came
up on `media0`, then `media1`, then `media0` again across three reboots. How
many there are does not move, since the same drivers register them each time,
so passing every one keeps up with the decoder wherever it lands. Pass none
that is missing, though: Docker refuses to start a container naming a device
that does not exist. A Pi 4 has four and a Pi 5 three, plus any a camera
module adds; wildcards are not an option, since each entry is one path.

To see what a board has right now:

```sh
ls -d /sys/class/drm/card*-* | sed 's|.*/|/dev/dri/|;s|-.*||' | sort -u
for v in $(grep -lE 'decode|-dec$' /sys/class/video4linux/*/name); do
    dirname "$v" | sed 's|.*/video4linux/|/dev/|'
done
ls /dev/media* /dev/cec*
```

The configuration stays on the host and is mounted read-only: RTSP urls embed
credentials and have no business inside an image.

The container runs as the unprivileged `viewpie` user (uid 100), in the
`video` group (gid 44) that owns every device above. So the file has to be
readable by one of those, not only by root. And on a host whose `video` group
is not 44, give the container that group's number with `group_add`.

## Configuration

Name your cameras, then say where each goes. A list of viewports fills the
smallest square grid that holds them, in order:

```yaml
feeds:
  porch: rtsp://10.0.0.5:7447/porch
  yard: rtsp://10.0.0.5:7447/yard
  gate: rtsp://10.0.0.5:7447/gate

displays:
  - viewports:
      - feeds: [porch]
      - feeds: [yard]
      - feeds: [gate]
```

The names are what the log calls each camera. A viewport names the feeds it
shows, and any number of viewports, on either display, may name the same one.

Anything else is drawn as a picture. A name repeated over a square block of
cells spans them, and `.` leaves a cell empty:

```yaml
spacing: 1                  # background pixels between neighbouring viewports

feeds:
  driveway: rtsp://10.0.0.5:7447/driveway
  porch: rtsp://10.0.0.5:7447/porch
  yard: rtsp://10.0.0.5:7447/yard
  garage: rtsp://10.0.0.5:7447/garage
  street: rtsp://10.0.0.5:7447/street

displays:
  - layout: |
      a a b
      a a c
      d e .

    viewports:
      - slot: a
        feeds: [driveway]
      - slot: b
        feeds: [porch]
      - slot: c
        feeds: [yard]
      - slot: d
        feeds: [street]           # also in e's rotation, decoded once
      - slot: e
        rotation_interval: 20     # seconds per feed
        feeds: [garage, street]
```

A layout that does not add up, such as a slot spanning two cells by one or
named in two places, is refused at startup naming the slot, rather than drawn
subtly wrong. So is a key that means nothing: a misspelled `conector` would
otherwise leave the wall on the wrong screen with nothing in the log. And so
is a feed no viewport shows, or a viewport naming one that does not exist.

`rtsps://` urls work as they come, including the `?enableSrtp` form a UniFi
Protect recorder hands out, with no transport or certificate settings to
write.

`connector` (`HDMI-A-1`, as in `/sys/class/drm`) and `mode` may be left out.
Without a mode the display keeps the one it is already in, or, plugged in
after the wall started, gets the one it prefers. A second display must name
its connector, and the two share a pool of sixteen overlay planes, so a
viewport on one screen is one the other cannot have.

### The television's remote

If the television speaks HDMI-CEC, and has it turned on in its menus, its
remote drives the wall:

- **Left and right** step every rotating viewport to the previous or next
  feed, which then stays up for a whole interval.
- **Down and up** show one camera fullscreen, and go through the others.
- **Back** returns to the grid. So does ten minutes without a key pressed.

The wall shows up among the television's inputs as viewpie. It never switches
the television over to itself.

Fullscreen, a camera is its grid stream scaled up, which looks soft for a
small substream. A sharper stream can be given for it:

```yaml
feeds:
  porch: rtsp://10.0.0.5:7447/porch                # one stream for both
  yard:
    url: rtsp://10.0.0.5:7447/yard                 # the grid's, 640x360
    fullscreen_url: rtsp://10.0.0.5:7447/yard_hd   # 1280x720
```

It is connected only while the camera is fullscreen, with the grid's stream
standing in for the second or two it takes to arrive. Pick one no larger than
the display, and in a codec the board decodes in hardware (see below).

### Pausing off screen

```yaml
pause_off_screen: true
```

disconnects the cameras while nobody can see the wall: while its display is
unplugged, or its television is in standby or on another input. The
television says so over HDMI-CEC; without that, only unplugging pauses the
wall. Back on screen, the cameras show again within a second or two, and a
display left fullscreen shows its grid.

One device can keep an LG from saying it switched away: one that joins CEC
and then answers nothing. The wall then stays paused until the next switch
the television does announce, such as to its own apps and back.

The file the package installs at `/etc/viewpie/viewpie.yaml` lists every
setting, commented out, including `background`, per-display `spacing` and the
`rotation_interval` default every viewport inherits.

## What runs where

|      | H.264               | H.265               |
|------|---------------------|---------------------|
| Pi 3 | hardware, zero-copy | software            |
| Pi 4 | hardware, zero-copy | hardware, zero-copy |
| Pi 5 | software            | hardware, zero-copy |

A Pi 5 has no H.264 block and a Pi 3 no H.265 one. Either way you get a
picture; the difference is what it costs. Hardware decoding needs Raspberry Pi
OS's ffmpeg. Upstream ffmpeg decodes in software on every path.

Measured on all three, against live cameras. A Pi 4 is the only one with both
blocks, so it is the only board where a mixed wall costs nothing extra: nine
cameras including an H.265 one came to 12% of a core there, against 18% for
the same wall all-H.264 on a Pi 3 and 42% once one feed was H.265 and had to
be decoded in software.

## Building

Needs only Docker; the packages cross-compile, so no toolchain goes near a Pi:

```sh
scripts/build-deb.sh              # both architectures, into dist/
scripts/build-deb.sh armhf        # just the 32-bit one
```

The images install those same packages. Building one runs apt as the target
architecture, so it needs qemu:

```sh
docker run --privileged --rm tonistiigi/binfmt --install all
scripts/build-image.sh
```

## Licence

GPL-3.0-or-later. See `LICENSE`.
