#!/usr/bin/env bash
# Builds 1k Words as a macOS app, "1k Words.app", with the native libraries
# it needs inside it (KW-7): FFmpeg's LGPL build and libheif with libde265,
# from build-ffmpeg.sh and build-libheif.sh, built first if they aren't.
#
#     scripts/bundle-macos.sh
#
# Writes target/bundle/1k Words.app. Its name, identifier, version, category
# and icon come from Cargo.toml ([package] and [package.metadata.bundle]).
#
# The app's libraries go in Contents/Frameworks, where its rpath points, in
# place of the build folders it was linked against. The script checks that
# the app and its libraries load nothing outside the bundle but the
# system's, then signs it: ad hoc unless SIGN_IDENTITY names a Developer ID
# certificate, which also turns on the hardened runtime, with the
# entitlements in scripts/entitlements.plist. Notarizing is left to the
# release workflow (.github/workflows/release.yml).
#
# The licences of the app and of each library go in Contents/Resources/
# Licences (KW-8).

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
# The oldest macOS the app supports (MACOSX_DEPLOYMENT_TARGET), for the
# app's own code and its Info.plist.
source "$ROOT/scripts/macos-target.sh"

FFMPEG_VERSION="$(sed -n 's/^VERSION=//p' scripts/build-ffmpeg.sh)"
LIBHEIF_VERSION="$(sed -n 's/^LIBHEIF_VERSION=//p' scripts/build-libheif.sh)"
FFMPEG_DIR="$ROOT/target/ffmpeg/$FFMPEG_VERSION"
LIBHEIF_DIR="$ROOT/target/libheif/$LIBHEIF_VERSION"
[[ -f "$FFMPEG_DIR/lib/libavcodec.dylib" ]] || scripts/build-ffmpeg.sh
[[ -f "$LIBHEIF_DIR/lib/libheif.dylib" ]] || scripts/build-libheif.sh

# What the app is called, from Cargo.toml.
eval "$(cargo metadata --no-deps --format-version 1 | python3 -I -c '
import json, shlex, sys
package = next(p for p in json.load(sys.stdin)["packages"] if p["name"] == "thousand_words")
bundle = package["metadata"]["bundle"]
icns = next(icon for icon in bundle["icon"] if icon.endswith(".icns"))
binary = next(t["name"] for t in package["targets"] if "bin" in t["kind"])
for key, value in [("NAME", bundle["name"]), ("IDENTIFIER", bundle["identifier"]),
                   ("CATEGORY", bundle["category"]), ("ICNS", icns),
                   ("VERSION", package["version"]), ("BINARY", binary)]:
    print(f"{key}={shlex.quote(value)}")
')"

echo "Building $NAME $VERSION for macOS $MACOSX_DEPLOYMENT_TARGET and later…"
# Source paths go into the binary, for panic messages. Map this machine's to
# neutral ones, so the app doesn't carry them (or the builder's username).
# Set for each target rather than in RUSTFLAGS, which build.rs leaves out
# of the plugins' build: the plugins are WebAssembly, embedded in the app.
HOST="$(rustc -vV | sed -n 's/^host: //p')"
REMAP="--remap-path-prefix=$ROOT=/1k-words"
REMAP+=" --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo"
REMAP+=" --remap-path-prefix=$(rustc --print sysroot)=/rustc"
export "CARGO_TARGET_$(echo "$HOST" | tr 'a-z-' 'A-Z_')_RUSTFLAGS=$REMAP"
export CARGO_TARGET_WASM32_WASIP2_RUSTFLAGS="$REMAP"
export FFMPEG_DIR
# libheif-sys finds libheif through pkg-config: this one, ahead of Homebrew's.
export PKG_CONFIG_PATH="$LIBHEIF_DIR/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
cargo build --release --locked

APP="$ROOT/target/bundle/$NAME.app"
CONTENTS="$APP/Contents"
rm -rf "$APP"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Frameworks" "$CONTENTS/Resources/Licences"

cp "target/release/$BINARY" "$CONTENTS/MacOS/$BINARY"
cp "$ICNS" "$CONTENTS/Resources/AppIcon.icns"

# Each library the app needs, under the name it's linked by (such as
# libavcodec.63.dylib), from the builds' versioned files.
for dir in "$FFMPEG_DIR/lib" "$LIBHEIF_DIR/lib"; do
  for lib in "$dir"/lib*.dylib; do
    name="$(otool -D "$lib" | tail -n 1)"
    [[ "$name" == @rpath/* ]] || continue
    target="$CONTENTS/Frameworks/${name#@rpath/}"
    [[ -e "$target" ]] || cp -L "$lib" "$target"
  done
done
chmod u+w "$CONTENTS/Frameworks"/*.dylib

# The app looks for them in its Frameworks folder, not the build folders.
EXECUTABLE="$CONTENTS/MacOS/$BINARY"
otool -l "$EXECUTABLE" | awk '/LC_RPATH/ {getline; getline; print $2}' | while read -r rpath; do
  install_name_tool -delete_rpath "$rpath" "$EXECUTABLE"
done
install_name_tool -add_rpath "@executable_path/../Frameworks" "$EXECUTABLE"

cat > "$CONTENTS/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleDisplayName</key>
  <string>$NAME</string>
  <key>CFBundleExecutable</key>
  <string>$BINARY</string>
  <key>CFBundleIconFile</key>
  <string>AppIcon</string>
  <key>CFBundleIdentifier</key>
  <string>$IDENTIFIER</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>$NAME</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>$VERSION</string>
  <key>CFBundleVersion</key>
  <string>$VERSION</string>
  <key>LSApplicationCategoryType</key>
  <string>$CATEGORY</string>
  <key>LSMinimumSystemVersion</key>
  <string>$MACOSX_DEPLOYMENT_TARGET</string>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>NSPrincipalClass</key>
  <string>NSApplication</string>
  <key>NSRemovableVolumesUsageDescription</key>
  <string>$NAME imports photos from memory cards, cameras and USB drives.</string>
</dict>
</plist>
PLIST
plutil -lint -s "$CONTENTS/Info.plist"

# Licences: the app's, then each library's, as their sources ship them.
cp COPYING COPYING.LESSER "$CONTENTS/Resources/Licences/"
FFMPEG_SOURCE="$ROOT/target/ffmpeg/src/ffmpeg-$FFMPEG_VERSION"
HEIF_SOURCES="$ROOT/target/libheif/src"
if [[ -d "$FFMPEG_SOURCE" ]]; then
  cp "$FFMPEG_SOURCE/COPYING.LGPLv2.1" "$CONTENTS/Resources/Licences/FFmpeg COPYING.LGPLv2.1"
  cp "$FFMPEG_SOURCE/LICENSE.md" "$CONTENTS/Resources/Licences/FFmpeg LICENSE.md"
fi
for source in "$HEIF_SOURCES"/libheif-* "$HEIF_SOURCES"/libde265-*; do
  [[ -d "$source" && -f "$source/COPYING" ]] || continue
  cp "$source/COPYING" "$CONTENTS/Resources/Licences/$(basename "$source" | sed 's/-[0-9.]*$//') COPYING"
done

# Everything the app and its libraries load is in the bundle or the system.
problems=0
for file in "$EXECUTABLE" "$CONTENTS/Frameworks"/*.dylib; do
  while read -r dep; do
    case "$dep" in
      /System/* | /usr/lib/*) ;;
      @rpath/*)
        [[ -f "$CONTENTS/Frameworks/${dep#@rpath/}" ]] || {
          echo "$(basename "$file") needs ${dep#@rpath/}, which isn't in the bundle" >&2
          problems=1
        }
        ;;
      *)
        echo "$(basename "$file") loads $dep, from outside the bundle" >&2
        problems=1
        ;;
    esac
  done < <(otool -L "$file" | tail -n +2 | awk '{print $1}' | grep -v -F "$(otool -D "$file" | tail -n +2)")
done
# And none of this build's paths are left in it: the ones remapped above.
# Not all of $HOME: the prebuilt ONNX Runtime (from ort) has its own build
# machine's paths, /Users/runner/..., which is $HOME on GitHub's runners.
LEAKS=(-e "$ROOT/" -e "${CARGO_HOME:-$HOME/.cargo}/" -e "$(rustc --print sysroot)/")
if grep -r -l -F "${LEAKS[@]}" "$CONTENTS/MacOS" "$CONTENTS/Frameworks" >/dev/null 2>&1; then
  echo "The bundle still has paths from this machine in:" >&2
  grep -r -l -F "${LEAKS[@]}" "$CONTENTS/MacOS" "$CONTENTS/Frameworks" >&2
  grep -r -h -o -a -F "${LEAKS[@]}" "$CONTENTS/MacOS" "$CONTENTS/Frameworks" | sort | uniq -c >&2
  problems=1
fi
[[ $problems == 0 ]] || exit 1

# Libraries first, then the app, which seals them. A real identity gets the
# hardened runtime, whose library validation only loads libraries signed by
# the same team: these are. Ad hoc signatures have no team, so an ad hoc
# build isn't hardened.
IDENTITY="${SIGN_IDENTITY:--}"
SIGN_OPTIONS=(--force --sign "$IDENTITY")
APP_OPTIONS=()
if [[ "$IDENTITY" != "-" ]]; then
  SIGN_OPTIONS+=(--options runtime --timestamp)
  APP_OPTIONS+=(--entitlements "$ROOT/scripts/entitlements.plist")
fi
codesign "${SIGN_OPTIONS[@]}" "$CONTENTS/Frameworks"/*.dylib
codesign "${SIGN_OPTIONS[@]}" ${APP_OPTIONS[@]+"${APP_OPTIONS[@]}"} "$APP"
codesign --verify --strict "$APP"

echo
echo "Built $APP"
echo "  $VERSION, for macOS $MACOSX_DEPLOYMENT_TARGET and later, signed $([[ "$IDENTITY" == "-" ]] && echo "ad hoc" || echo "by $IDENTITY")"
