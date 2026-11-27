#!/usr/bin/env bash
# Builds 1k Words for Windows as a folder you can unzip and run, with the
# native libraries it needs next to it (KW-49): FFmpeg and libheif with
# libde265, from build-vcpkg-windows.sh, built first if they aren't.
#
# Run in Git Bash, as on GitHub's Windows runners:
#
#     scripts/bundle-windows.sh
#
# Writes target/bundle/1k-Words-<version>-windows-x86_64/ and a .zip of it:
#
#     1k-words.exe      the app
#     *.dll             FFmpeg, libheif, libde265 and zlib, which Windows
#                       finds next to the app
#     licences/         the app's and each library's licence
#
# Not signed (Authenticode) or packaged as an installer (MSI or MSIX) yet;
# both are KW-49.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*) ;;
  *)
    echo "This bundles the app for Windows, in Git Bash on Windows." >&2
    exit 1
    ;;
esac

TRIPLET="$(sed -n 's/^TRIPLET=//p' scripts/build-vcpkg-windows.sh)"
export VCPKG_ROOT="$ROOT/target/vcpkg"
INSTALLED="$VCPKG_ROOT/installed/$TRIPLET"
[[ -f "$INSTALLED/include/libheif/heif.h" && -f "$INSTALLED/include/libavcodec/avcodec.h" ]] ||
  scripts/build-vcpkg-windows.sh
# vcpkg-rs (in ffmpeg-sys and libheif-sys) links the DLL builds there.
export VCPKGRS_TRIPLET="$TRIPLET" VCPKGRS_DYNAMIC=1

# What the app is called, from Cargo.toml.
eval "$(cargo metadata --no-deps --format-version 1 | python -I -c '
import json, shlex, sys
package = next(p for p in json.load(sys.stdin)["packages"] if p["name"] == "thousand_words")
bundle = package["metadata"]["bundle"]
binary = next(t["name"] for t in package["targets"] if "bin" in t["kind"])
for key, value in [("NAME", bundle["name"]), ("VERSION", package["version"]), ("BINARY", binary)]:
    print(f"{key}={shlex.quote(value)}")
' | tr -d '\r')"

echo "Building $NAME $VERSION for Windows (x86_64)…"
# Source paths go into the binary, for panic messages. Map this machine's to
# neutral ones, as bundle-macos.sh does.
REMAP="--remap-path-prefix=$(cygpath -w "$ROOT")=/1k-words"
REMAP+=" --remap-path-prefix=$(cygpath -w "${CARGO_HOME:-$USERPROFILE/.cargo}")=/cargo"
REMAP+=" --remap-path-prefix=$(rustc --print sysroot | tr -d '\r')=/rustc"
# Windows gives a program's main thread 1 MB of stack, and bindgen, in
# ffmpeg-sys-next's build script, recurses deeply through FFmpeg's headers.
# Unoptimised, as release builds' build scripts are, it overflows that, so
# give the programs built here (build scripts and the app) 8 MB, as on Linux.
STACK="-C link-arg=/STACK:8388608"
export CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS="$REMAP $STACK"
export CARGO_TARGET_WASM32_WASIP2_RUSTFLAGS="$REMAP"
cargo build --release --locked

DIR_NAME="${NAME// /-}-$VERSION-windows-x86_64"
OUT="$ROOT/target/bundle/$DIR_NAME"
rm -rf "$OUT" "$OUT.zip"
mkdir -p "$OUT/licences"

cp "target/release/$BINARY.exe" "$OUT/"
# The libraries' DLLs: just what was installed for them, which is what
# they need.
cp "$INSTALLED"/bin/*.dll "$OUT/"

# Licences: the app's, then each library's, as vcpkg installs them.
cp COPYING COPYING.LESSER "$OUT/licences/"
for port in ffmpeg libheif libde265 zlib; do
  [[ -f "$INSTALLED/share/$port/copyright" ]] &&
    cp "$INSTALLED/share/$port/copyright" "$OUT/licences/$port copyright"
done
[[ -f assets/fonts/OFL.txt ]] && cp assets/fonts/OFL.txt "$OUT/licences/Lato OFL.txt"

(cd "$(dirname "$OUT")" && 7z a -tzip -bso0 -bsp0 "$DIR_NAME.zip" "$DIR_NAME")

echo
echo "Built $OUT"
echo "  and $OUT.zip"
echo "  $VERSION, for Windows (x86_64)"
