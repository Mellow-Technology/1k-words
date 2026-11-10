#!/usr/bin/env bash
# Builds the libheif that 1k Words ships, with libde265 as its only codec,
# as shared libraries (KW-7).
#
# Homebrew's libheif links x265, a GPL encoder, so an app shipping it could
# only be distributed under the GPL. The app only reads HEIC and HEIF
# (src/heif.rs), so this one decodes HEVC with libde265 and has no encoders
# or other codecs: both are LGPL-3.0. libheif's codecs are on by default
# and found wherever they're installed, so each is turned off here, plugin
# loading too (it would load codecs from the system at runtime), and
# Homebrew is kept out of CMake's search. The script checks the result
# links nothing else.
#
#     scripts/build-libheif.sh [prefix]
#
# Installs into target/libheif/<version> unless given a prefix. The app's
# build finds it through pkg-config, so put it first on PKG_CONFIG_PATH:
#
#     PKG_CONFIG_PATH=$PWD/target/libheif/1.23.7/lib/pkgconfig cargo build --release
#
# scripts/bundle-macos.sh does this, with FFmpeg's LGPL build. The libraries
# are named @rpath/lib*.dylib, as FFmpeg's are (build-ffmpeg.sh).
#
# macOS only for now (KW-48); other platforms are KW-49.

set -euo pipefail

LIBHEIF_VERSION=1.23.7
LIBDE265_VERSION=1.1.3
# The same releases and checksums as Homebrew's formulas.
LIBHEIF_SHA256=f7483e818503ea6224c8863e4a56ac02c7089021a98472c811c12aa19faf2475
LIBDE265_SHA256=554228bd17788c99a7e63b37ab5634722190e6e2bf60c1dcb01cef328e133905

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PREFIX="${1:-$ROOT/target/libheif/$LIBHEIF_VERSION}"
# The oldest macOS the app supports (MACOSX_DEPLOYMENT_TARGET).
source "$ROOT/scripts/macos-target.sh"
WORK="$ROOT/target/libheif/src"

if [[ "$(uname)" != "Darwin" ]]; then
  echo "This builds libheif for macOS only so far (see KW-49)." >&2
  exit 1
fi
command -v cmake >/dev/null || {
  echo "CMake is needed: brew install cmake" >&2
  exit 1
}

# Downloads and unpacks a release, checking it against its checksum.
fetch() {
  local name=$1 version=$2 sha256=$3
  local tarball="$WORK/$name-$version.tar.gz"
  if [[ ! -f "$tarball" ]]; then
    echo "Downloading ${name} ${version}…"
    curl -fL --retry 5 --retry-delay 15 -o "$tarball.part" \
      "https://github.com/strukturag/$name/releases/download/v$version/$name-$version.tar.gz"
    mv "$tarball.part" "$tarball"
  fi
  echo "$sha256  $tarball" | shasum -a 256 -c - >/dev/null || {
    echo "$name-$version.tar.gz doesn't match its checksum; delete it and try again." >&2
    exit 1
  }
  rm -rf "$WORK/$name-$version" "$WORK/build-$name"
  tar -xzf "$tarball" -C "$WORK"
}

# What both builds share: shared libraries named @rpath/..., for the oldest
# supported macOS, found only in what's built here.
COMMON=(
  -DCMAKE_BUILD_TYPE=Release
  -DCMAKE_INSTALL_PREFIX="$PREFIX"
  -DCMAKE_INSTALL_NAME_DIR=@rpath
  -DCMAKE_OSX_DEPLOYMENT_TARGET="$MACOSX_DEPLOYMENT_TARGET"
  -DCMAKE_PREFIX_PATH="$PREFIX"
  -DCMAKE_IGNORE_PREFIX_PATH="/opt/homebrew;/usr/local"
  -DBUILD_SHARED_LIBS=ON
)
# libheif finds libde265 through pkg-config: only this one.
export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig"
export PKG_CONFIG_LIBDIR="$PREFIX/lib/pkgconfig"

mkdir -p "$WORK"
rm -rf "$PREFIX"

fetch libde265 "$LIBDE265_VERSION" "$LIBDE265_SHA256"
cmake -S "$WORK/libde265-$LIBDE265_VERSION" -B "$WORK/build-libde265" "${COMMON[@]}" \
  -DENABLE_SDL=OFF \
  -DENABLE_DECODER=OFF \
  -DENABLE_ENCODER=OFF \
  -DENABLE_SHERLOCK265=OFF
cmake --build "$WORK/build-libde265" --parallel "$(sysctl -n hw.ncpu)"
cmake --install "$WORK/build-libde265"

fetch libheif "$LIBHEIF_VERSION" "$LIBHEIF_SHA256"
cmake -S "$WORK/libheif-$LIBHEIF_VERSION" -B "$WORK/build-libheif" "${COMMON[@]}" \
  -DENABLE_PLUGIN_LOADING=OFF \
  -DWITH_LIBDE265=ON -DWITH_LIBDE265_PLUGIN=OFF \
  -DWITH_X265=OFF -DWITH_X264=OFF -DWITH_KVAZAAR=OFF \
  -DWITH_UVG266=OFF -DWITH_VVDEC=OFF -DWITH_VVENC=OFF \
  -DWITH_OpenH264_DECODER=OFF \
  -DWITH_DAV1D=OFF -DWITH_AOM_DECODER=OFF -DWITH_AOM_ENCODER=OFF \
  -DWITH_SvtEnc=OFF -DWITH_RAV1E=OFF \
  -DWITH_JPEG_DECODER=OFF -DWITH_JPEG_ENCODER=OFF \
  -DWITH_OpenJPEG_DECODER=OFF -DWITH_OpenJPEG_ENCODER=OFF \
  -DWITH_OPENJPH_ENCODER=OFF \
  -DWITH_FFMPEG_DECODER=OFF \
  -DWITH_UNCOMPRESSED_CODEC=OFF \
  -DWITH_LIBSHARPYUV=OFF \
  -DWITH_HEADER_COMPRESSION=OFF \
  -DWITH_EXAMPLES=OFF -DWITH_EXAMPLE_HEIF_THUMB=OFF -DWITH_EXAMPLE_HEIF_VIEW=OFF \
  -DWITH_GDK_PIXBUF=OFF \
  -DBUILD_TESTING=OFF \
  -DBUILD_DOCUMENTATION=OFF
cmake --build "$WORK/build-libheif" --parallel "$(sysctl -n hw.ncpu)"
cmake --install "$WORK/build-libheif"

# Nothing but each other and the system: no x265, aom or Homebrew.
for lib in "$PREFIX"/lib/libheif.dylib "$PREFIX"/lib/libde265.dylib; do
  stray="$(otool -L "$lib" | tail -n +2 | awk '{print $1}' |
    grep -v -E '^(@rpath/(libheif|libde265)\.|/usr/lib/|/System/)' || true)"
  if [[ -n "$stray" ]]; then
    echo "$(basename "$lib") links something it shouldn't:" >&2
    echo "$stray" >&2
    exit 1
  fi
done

echo
echo "Built libheif $LIBHEIF_VERSION with libde265 $LIBDE265_VERSION (LGPL-3.0, decoding only),"
echo "for macOS $MACOSX_DEPLOYMENT_TARGET and later, into $PREFIX"
