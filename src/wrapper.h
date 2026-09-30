/* The headers bindgen reads: libav for the decode side, and the kernel's CEC
 * interface for telling whether anyone is watching.
 *
 * libdrm is not here: the `drm` crate covers the presenter, and leaving its
 * headers out means a cross build needs only the libav development files and
 * the kernel headers every C toolchain already has.
 *
 * Nothing in libav is transcribed by hand. That matters beyond tidiness: the
 * struct layouts differ between a 32-bit and a 64-bit Pi, and bindgen reads
 * whichever headers the build sees. It also keeps the bindings portable --
 * Raspberry Pi's ffmpeg adds SAND128 and RPI4_8 pixel formats that upstream
 * does not have, and because nothing here matches AVPixelFormat exhaustively,
 * bindings generated against stock Debian still run against the Pi's build.
 * (That exhaustive match is exactly why the ffmpeg-next crate cannot compile
 * on a Pi at all.)
 */
#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_drm.h>
#include <libavutil/pixfmt.h>
#include <linux/cec.h>
