#!/usr/bin/env bash
# Builds the native libraries 1k Words ships on Windows, with vcpkg
# (KW-49): FFmpeg, LGPL only and with just the parts the app uses, and
# libheif with libde265 as its only codec. The macOS and Linux builds come
# from build-ffmpeg.sh and build-libheif.sh instead.
#
# Run in Git Bash (as on GitHub's Windows runners), with Visual Studio's
# C++ tools installed:
#
#     scripts/build-vcpkg-windows.sh
#
# Clones vcpkg at a pinned commit into target/vcpkg and installs the ports
# there as release-only DLLs (an x64-windows-release triplet). The app's
# build finds them through vcpkg-rs, in classic mode:
#
#     export VCPKG_ROOT="$PWD/target/vcpkg" VCPKGRS_TRIPLET=x64-windows-release VCPKGRS_DYNAMIC=1
#     cargo build --release
#
# scripts/bundle-windows.sh does this, then puts the DLLs next to the app.
#
# FFmpeg's port is LGPL unless its gpl feature is on, and libheif's links
# x265 (GPL) through its default hevc feature, so both are installed
# without their defaults, and the script checks neither feature crept in.

set -euo pipefail

# microsoft/vcpkg's master on 2026-11-26: FFmpeg 9.1, libheif 1.23.7.
VCPKG_COMMIT=e6d62cc9ce8f8d9687df14521118f2ec3c10ee6a
TRIPLET=x64-windows-release
PORTS=(
  "ffmpeg[core,avcodec,avformat,swresample,swscale,zlib]:$TRIPLET"
  "libheif[core,libde265]:$TRIPLET"
)

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VCPKG="$ROOT/target/vcpkg"

case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*) ;;
  *)
    echo "This builds the Windows libraries, in Git Bash on Windows." >&2
    exit 1
    ;;
esac

if [[ ! -d "$VCPKG/.git" ]]; then
  git clone --quiet https://github.com/microsoft/vcpkg.git "$VCPKG"
fi
git -C "$VCPKG" fetch --quiet origin "$VCPKG_COMMIT"
git -C "$VCPKG" checkout --quiet --force "$VCPKG_COMMIT"
if [[ ! -f "$VCPKG/vcpkg.exe" ]]; then
  (cd "$VCPKG" && cmd //c bootstrap-vcpkg.bat -disableMetrics)
fi

# Release builds only: the app never links the debug ones. (vcpkg has this
# triplet in triplets/community; written here so it can't change.)
mkdir -p "$VCPKG/overlay-triplets"
cat > "$VCPKG/overlay-triplets/$TRIPLET.cmake" <<'CMAKE'
set(VCPKG_TARGET_ARCHITECTURE x64)
set(VCPKG_CRT_LINKAGE dynamic)
set(VCPKG_LIBRARY_LINKAGE dynamic)
set(VCPKG_BUILD_TYPE release)
CMAKE
"$VCPKG/vcpkg.exe" install \
  --classic \
  --vcpkg-root "$VCPKG" \
  --overlay-triplets "$VCPKG/overlay-triplets" \
  --host-triplet "$TRIPLET" \
  --clean-after-build \
  "${PORTS[@]}"

# Nothing GPL: no gpl feature on FFmpeg, no x265 for libheif.
installed="$("$VCPKG/vcpkg.exe" list --classic --vcpkg-root "$VCPKG")"
if grep -q -E '^ffmpeg\[gpl\]|^ffmpeg\[nonfree\]|^x265:|^libheif\[hevc\]' <<<"$installed"; then
  echo "Something GPL or non-free was installed; it must be LGPL only:" >&2
  echo "$installed" >&2
  exit 1
fi

echo
echo "Installed into $VCPKG/installed/$TRIPLET:"
echo "$installed"
echo "Build the app against them with:"
echo "  VCPKG_ROOT=$VCPKG VCPKGRS_TRIPLET=$TRIPLET VCPKGRS_DYNAMIC=1 cargo build --release"
