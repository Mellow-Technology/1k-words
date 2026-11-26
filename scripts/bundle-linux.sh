#!/usr/bin/env bash
# Builds 1k Words for Linux as a folder you can unpack and run, with the
# native libraries it needs inside it (KW-49): FFmpeg's LGPL build and
# libheif with libde265, from build-ffmpeg.sh and build-libheif.sh, built
# first if they aren't.
#
#     scripts/bundle-linux.sh
#
# Writes target/bundle/1k-Words-<version>-linux-<arch>/ and a .tar.gz of it:
#
#     1k-words                  the app, which finds its libraries in lib/
#     lib/                      FFmpeg, libheif and libde265
#     share/applications/       a desktop entry, and
#     share/icons/hicolor/      its icons, to install for a launcher
#     licences/                 the app's and each library's licence
#
# Everything else it loads (GPU drivers, X11 or Wayland, fontconfig, ALSA,
# D-Bus, the C and C++ runtimes) is the system's. It's built against the
# machine's glibc and libstdc++, so it runs on that distribution and newer
# ones: the release builds on Ubuntu 24.04. An AppImage or Flatpak, which
# would carry more of this, is still to do (KW-49).

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

if [[ "$(uname)" != "Linux" ]]; then
  echo "This bundles the app for Linux; on macOS use bundle-macos.sh." >&2
  exit 1
fi
command -v patchelf >/dev/null || {
  echo "patchelf is needed (apt install patchelf)." >&2
  exit 1
}

FFMPEG_VERSION="$(sed -n 's/^VERSION=//p' scripts/build-ffmpeg.sh)"
LIBHEIF_VERSION="$(sed -n 's/^LIBHEIF_VERSION=//p' scripts/build-libheif.sh)"
FFMPEG_DIR="$ROOT/target/ffmpeg/$FFMPEG_VERSION"
LIBHEIF_DIR="$ROOT/target/libheif/$LIBHEIF_VERSION"
[[ -f "$FFMPEG_DIR/lib/libavcodec.so" ]] || scripts/build-ffmpeg.sh
[[ -f "$LIBHEIF_DIR/lib/libheif.so" ]] || scripts/build-libheif.sh

# What the app is called, from Cargo.toml.
eval "$(cargo metadata --no-deps --format-version 1 | python3 -I -c '
import json, shlex, sys
package = next(p for p in json.load(sys.stdin)["packages"] if p["name"] == "thousand_words")
bundle = package["metadata"]["bundle"]
binary = next(t["name"] for t in package["targets"] if "bin" in t["kind"])
for key, value in [("NAME", bundle["name"]), ("VERSION", package["version"]), ("BINARY", binary)]:
    print(f"{key}={shlex.quote(value)}")
')"
ARCH="$(uname -m)"

echo "Building $NAME $VERSION for Linux ($ARCH)…"
# Source paths go into the binary, for panic messages. Map this machine's to
# neutral ones, as bundle-macos.sh does.
HOST="$(rustc -vV | sed -n 's/^host: //p')"
REMAP="--remap-path-prefix=$ROOT=/1k-words"
REMAP+=" --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo"
REMAP+=" --remap-path-prefix=$(rustc --print sysroot)=/rustc"
export "CARGO_TARGET_$(echo "$HOST" | tr 'a-z-' 'A-Z_')_RUSTFLAGS=$REMAP"
export CARGO_TARGET_WASM32_WASIP2_RUSTFLAGS="$REMAP"
export FFMPEG_DIR
export PKG_CONFIG_PATH="$LIBHEIF_DIR/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
cargo build --release --locked

DIR_NAME="${NAME// /-}-$VERSION-linux-$ARCH"
OUT="$ROOT/target/bundle/$DIR_NAME"
rm -rf "$OUT" "$OUT.tar.gz"
mkdir -p "$OUT/lib" "$OUT/licences" "$OUT/share/applications"

EXECUTABLE="$OUT/$BINARY"
cp "target/release/$BINARY" "$EXECUTABLE"

# Each library the app needs, under its soname (libavcodec.so.63), from the
# builds' versioned files. They find each other next to themselves.
for dir in "$FFMPEG_DIR/lib" "$LIBHEIF_DIR/lib"; do
  for lib in "$dir"/lib*.so; do
    soname="$(patchelf --print-soname "$lib")"
    [[ -e "$OUT/lib/$soname" ]] || cp -L "$lib" "$OUT/lib/$soname"
  done
done
chmod u+w "$OUT/lib"/*.so*
for lib in "$OUT/lib"/*.so*; do
  patchelf --set-rpath '$ORIGIN' "$lib"
done
# The app looks in its lib folder, not the build folders.
patchelf --set-rpath '$ORIGIN/lib' "$EXECUTABLE"

# A desktop entry and icons, for installing into ~/.local/share.
for size in 32 128 256 512; do
  icons="$OUT/share/icons/hicolor/${size}x${size}/apps"
  mkdir -p "$icons"
  cp "assets/app-icon/${size}x${size}.png" "$icons/$BINARY.png"
done
cat > "$OUT/share/applications/$BINARY.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=$NAME
Comment=Organize, browse and back up your photos
Exec=$BINARY
Icon=$BINARY
Categories=Graphics;Photography;
Terminal=false
DESKTOP

# Licences: the app's, then each library's, as their sources ship them.
cp COPYING COPYING.LESSER "$OUT/licences/"
FFMPEG_SOURCE="$ROOT/target/ffmpeg/src/ffmpeg-$FFMPEG_VERSION"
HEIF_SOURCES="$ROOT/target/libheif/src"
if [[ -d "$FFMPEG_SOURCE" ]]; then
  cp "$FFMPEG_SOURCE/COPYING.LGPLv2.1" "$OUT/licences/FFmpeg COPYING.LGPLv2.1"
  cp "$FFMPEG_SOURCE/LICENSE.md" "$OUT/licences/FFmpeg LICENSE.md"
fi
for source in "$HEIF_SOURCES"/libheif-* "$HEIF_SOURCES"/libde265-*; do
  [[ -d "$source" && -f "$source/COPYING" ]] || continue
  cp "$source/COPYING" "$OUT/licences/$(basename "$source" | sed 's/-[0-9.]*$//') COPYING"
done
[[ -f assets/fonts/OFL.txt ]] && cp assets/fonts/OFL.txt "$OUT/licences/Lato OFL.txt"

# Everything the app and its libraries load is in the bundle or the
# system's: nothing from the build folders, and nothing missing.
problems=0
for file in "$EXECUTABLE" "$OUT/lib"/*.so*; do
  while read -r line; do
    case "$line" in
      *"not found"*)
        echo "$(basename "$file") needs ${line%% *}, which isn't in the bundle or the system" >&2
        problems=1
        ;;
      *"=> $OUT/lib/"* | *"=> /lib/"* | *"=> /usr/lib/"* | *"=> /lib64/"* | *"=> /usr/lib64/"*) ;;
      *"=>"*)
        echo "$(basename "$file") loads ${line#*=> }, from outside the bundle and the system" >&2
        problems=1
        ;;
    esac
  done < <(env -u LD_LIBRARY_PATH ldd "$file")
done
# And none of this build's paths are left in it: the ones remapped above.
LEAKS=(-e "$ROOT/" -e "${CARGO_HOME:-$HOME/.cargo}/" -e "$(rustc --print sysroot)/")
if grep -r -l -F "${LEAKS[@]}" "$EXECUTABLE" "$OUT/lib" >/dev/null 2>&1; then
  echo "The bundle still has paths from this machine in:" >&2
  grep -r -l -F "${LEAKS[@]}" "$EXECUTABLE" "$OUT/lib" >&2
  grep -r -h -o -a -F "${LEAKS[@]}" "$EXECUTABLE" "$OUT/lib" | sort | uniq -c >&2
  problems=1
fi
[[ $problems == 0 ]] || exit 1

tar -czf "$OUT.tar.gz" -C "$(dirname "$OUT")" "$DIR_NAME"

echo
echo "Built $OUT"
echo "  and $OUT.tar.gz"
echo "  $VERSION, for Linux ($ARCH)"
