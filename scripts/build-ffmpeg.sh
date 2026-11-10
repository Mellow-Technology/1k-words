#!/usr/bin/env bash
# Builds the FFmpeg that 1k Words ships: LGPL only, as shared libraries,
# with just what the app uses (KW-6).
#
# Homebrew's FFmpeg is built with --enable-gpl, so an app linked against it
# can only be distributed under the GPL. This one is never configured with
# --enable-gpl or --enable-nonfree, and the script checks that before it
# finishes. It keeps FFmpeg's default LGPL-2.1-or-later licence, which the
# app's LGPL-3.0 is compatible with.
#
# What the app uses (src/video): Live Photos' QuickTime (.mov) files, with
# H.264 or HEVC video decoded by VideoToolbox and AAC audio, scaled and
# resampled by libswscale and libswresample.
#
#     scripts/build-ffmpeg.sh [prefix]
#
# Installs into target/ffmpeg/<version> unless given a prefix, then build
# the app against it with FFMPEG_DIR:
#
#     FFMPEG_DIR=$PWD/target/ffmpeg/9.0.2 cargo build --release
#
# The libraries are named @rpath/libav*.dylib, so the app finds them through
# its rpath: build.rs adds FFMPEG_DIR/lib for running from target/, and a
# bundled app looks in its own Frameworks folder.
#
# macOS only for now (KW-48); other platforms are KW-49.

set -euo pipefail

VERSION=9.0.2
# Of ffmpeg-9.0.2.tar.xz, checked against FFmpeg's PGP signature
# (ffmpeg-9.0.2.tar.xz.asc, by the "FFmpeg release signing key",
# FCF986EA15E6E293A5644F10B4322F04D67658D8) when the version was pinned.
SHA256=8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PREFIX="${1:-$ROOT/target/ffmpeg/$VERSION}"
# The oldest macOS the app supports (MACOSX_DEPLOYMENT_TARGET).
source "$ROOT/scripts/macos-target.sh"
WORK="$ROOT/target/ffmpeg/src"
TARBALL="$WORK/ffmpeg-$VERSION.tar.xz"
SOURCE="$WORK/ffmpeg-$VERSION"

if [[ "$(uname)" != "Darwin" ]]; then
  echo "This builds FFmpeg for macOS only so far (see KW-49)." >&2
  exit 1
fi

mkdir -p "$WORK"
if [[ ! -f "$TARBALL" ]]; then
  echo "Downloading FFmpeg ${VERSION}…"
  curl -fL --retry 5 --retry-delay 15 -o "$TARBALL.part" "https://ffmpeg.org/releases/ffmpeg-$VERSION.tar.xz"
  mv "$TARBALL.part" "$TARBALL"
fi
echo "$SHA256  $TARBALL" | shasum -a 256 -c - >/dev/null || {
  echo "ffmpeg-$VERSION.tar.xz doesn't match its checksum; delete it and try again." >&2
  exit 1
}

# A clean tree each time, so no earlier configuration lingers.
rm -rf "$SOURCE"
tar -xf "$TARBALL" -C "$WORK"
cd "$SOURCE"

# FFmpeg keeps its configure line in the library (avutil_configuration()),
# so it's configured with a neutral prefix and installed into PREFIX with
# DESTDIR, to keep this machine's paths out of what's shipped. Nothing
# needs the prefix: the app's build finds FFmpeg through FFMPEG_DIR.
./configure \
  --prefix=/ffmpeg \
  --install-name-dir='@rpath' \
  --enable-shared \
  --disable-static \
  --disable-programs \
  --disable-doc \
  --disable-debug \
  --disable-network \
  --disable-avdevice \
  --disable-avfilter \
  `# Nothing found on this Mac is pulled in unless named below.` \
  --disable-autodetect \
  --enable-videotoolbox \
  --enable-zlib \
  `# Then only the parts the app uses.` \
  --disable-everything \
  --enable-protocol=file \
  --enable-demuxer=mov \
  --enable-decoder=h264,hevc,aac \
  --enable-parser=h264,hevc,aac \
  --enable-hwaccel=h264_videotoolbox,hevc_videotoolbox \
  --extra-cflags="-mmacosx-version-min=$MACOSX_DEPLOYMENT_TARGET" \
  --extra-ldflags="-mmacosx-version-min=$MACOSX_DEPLOYMENT_TARGET"

# configure records the licence it chose. Stop before installing anything
# that isn't LGPL.
for flag in CONFIG_GPL CONFIG_NONFREE; do
  if ! grep -q "^#define $flag 0$" config.h; then
    echo "FFmpeg was configured with $flag; it must be LGPL only." >&2
    exit 1
  fi
done

make -j"$(sysctl -n hw.ncpu)"
rm -rf "$PREFIX" "$WORK/stage"
make install DESTDIR="$WORK/stage"
mkdir -p "$(dirname "$PREFIX")"
mv "$WORK/stage/ffmpeg" "$PREFIX"
rm -rf "$WORK/stage"

# The licence as the built library reports it, from its configuration.
LICENSE="$(grep '^#define FFMPEG_LICENSE ' config.h | cut -d'"' -f2)"
echo
echo "Built FFmpeg $VERSION ($LICENSE), for macOS $MACOSX_DEPLOYMENT_TARGET and later, into $PREFIX"
echo "Build the app against it with:"
echo "  FFMPEG_DIR=$PREFIX cargo build --release"
