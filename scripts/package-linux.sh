#!/usr/bin/env bash
# Packages the Linux build from bundle-linux.sh as a .deb and an .rpm
# (KW-49), with nFPM, built first if it isn't.
#
#     scripts/package-linux.sh
#
# Writes 1k-Words-<version>-linux-<arch>.deb and .rpm into target/bundle.
# Both install the same files:
#
#     /opt/1k-words/                    the app and its lib folder, as bundled
#     /usr/bin/1k-words                 a link to it
#     /usr/share/applications/          its desktop entry
#     /usr/share/icons/hicolor/         and icons
#     /usr/share/doc/1k-words/          the licences
#
# Their dependencies come from what the app and its libraries link, less
# what's bundled: on Debian and Ubuntu from dpkg-shlibdeps, as packages with
# versions; for RPM as the sonames themselves, which any RPM distribution
# resolves. Vulkan, which GPUI loads at runtime rather than links, is added
# by hand.

set -euo pipefail

# nFPM, from its GitHub release, checked against the release's checksums.
NFPM_VERSION=2.49.0
NFPM_SHA256_x86_64=2afaa5ffb5e3493402e31e7c67a5a1fb42133e3f66fc315ca0051934125561c7
NFPM_SHA256_arm64=d4eee71839e92c68cbd3b101640bad32d2856fc551acae0635be9118e48c9d9c

PACKAGE=1k-words
MAINTAINER="Mellow Technology"
HOMEPAGE=https://github.com/Mellow-Technology/1k-words

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

if [[ "$(uname)" != "Linux" ]]; then
  echo "This packages the app for Linux." >&2
  exit 1
fi
command -v dpkg-shlibdeps >/dev/null || {
  echo "dpkg-shlibdeps is needed (apt install dpkg-dev), for the .deb's dependencies." >&2
  exit 1
}

eval "$(cargo metadata --no-deps --format-version 1 | python3 -I -c '
import json, shlex, sys
package = next(p for p in json.load(sys.stdin)["packages"] if p["name"] == "thousand_words")
bundle = package["metadata"]["bundle"]
binary = next(t["name"] for t in package["targets"] if "bin" in t["kind"])
for key, value in [("NAME", bundle["name"]), ("VERSION", package["version"]), ("BINARY", binary)]:
    print(f"{key}={shlex.quote(value)}")
')"
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64) GOARCH=amd64 NFPM_ARCH=x86_64 NFPM_SHA256=$NFPM_SHA256_x86_64 ;;
  aarch64) GOARCH=arm64 NFPM_ARCH=arm64 NFPM_SHA256=$NFPM_SHA256_arm64 ;;
  *)
    echo "No nFPM for $ARCH." >&2
    exit 1
    ;;
esac

BUNDLE="$ROOT/target/bundle/${NAME// /-}-$VERSION-linux-$ARCH"
[[ -x "$BUNDLE/$BINARY" ]] || scripts/bundle-linux.sh

NFPM_DIR="$ROOT/target/nfpm/$NFPM_VERSION"
NFPM="$NFPM_DIR/nfpm"
if [[ ! -x "$NFPM" ]]; then
  echo "Downloading nFPM ${NFPM_VERSION}…"
  mkdir -p "$NFPM_DIR"
  tarball="$NFPM_DIR/nfpm.tar.gz"
  curl -fL --retry 5 --retry-delay 15 -o "$tarball" \
    "https://github.com/goreleaser/nfpm/releases/download/v$NFPM_VERSION/nfpm_${NFPM_VERSION}_Linux_$NFPM_ARCH.tar.gz"
  echo "$NFPM_SHA256  $tarball" | sha256sum -c - >/dev/null || {
    echo "nFPM's download doesn't match its checksum." >&2
    exit 1
  }
  tar -xzf "$tarball" -C "$NFPM_DIR" nfpm
  rm "$tarball"
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# The app and the libraries it ships.
elves=("$BUNDLE/$BINARY" "$BUNDLE"/lib/*.so*)
bundled="$(for lib in "$BUNDLE"/lib/*.so*; do basename "$lib"; done)"

# Debian's: dpkg-shlibdeps reads a package's files from debian/<package>
# (its DEBIAN folder marks the package's root, for $ORIGIN), and leaves out
# libraries found there, the bundled ones.
mkdir -p "$WORK/deb/debian/$PACKAGE/DEBIAN" "$WORK/deb/debian/$PACKAGE/opt"
cp -r "$BUNDLE" "$WORK/deb/debian/$PACKAGE/opt/$PACKAGE"
printf 'Source: %s\n\nPackage: %s\nArchitecture: any\n' "$PACKAGE" "$PACKAGE" > "$WORK/deb/debian/control"
if ! (cd "$WORK/deb" && dpkg-shlibdeps -O \
  "debian/$PACKAGE/opt/$PACKAGE/$BINARY" "debian/$PACKAGE/opt/$PACKAGE"/lib/*.so*) \
  >"$WORK/shlibdeps.out" 2>"$WORK/shlibdeps.log"; then
  grep -v 'warning:' "$WORK/shlibdeps.log" >&2
  echo "dpkg-shlibdeps couldn't work out the .deb's dependencies." >&2
  exit 1
fi
deb_depends="$(sed -n 's/^shlibs:Depends=//p' "$WORK/shlibdeps.out")"
deb_depends+=", libvulkan1"

# RPM's: each soname linked and not bundled, and glibc's newest symbol
# version, as rpmbuild would generate them.
needed="$(for elf in "${elves[@]}"; do
  readelf -d "$elf" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'
done | sort -u | grep -v -x -F "$bundled" | grep -v '^ld-linux')"
glibc="$(for elf in "${elves[@]}"; do
  objdump -T "$elf" | grep -o 'GLIBC_[0-9.]*'
done | sort -u -V | tail -n 1)"
suffix="()(64bit)"
rpm_depends=()
while read -r soname; do
  rpm_depends+=("$soname$suffix")
done <<<"$needed"
rpm_depends+=("libc.so.6($glibc)(64bit)" "libvulkan.so.1$suffix")

yaml_list() {
  local item
  for item in "$@"; do
    printf '    - "%s"\n' "$item"
  done
}
IFS=',' read -r -a deb_list <<<"$deb_depends"
deb_list=("${deb_list[@]# }")

cat > "$WORK/nfpm.yaml" <<YAML
name: $PACKAGE
arch: $GOARCH
platform: linux
version: $VERSION
section: graphics
priority: optional
maintainer: $MAINTAINER
description: |
  A photo library for photographers who'd rather be shooting than sorting.
  Brackets stack themselves, search finds what's in your photos, and
  syncing just works.
vendor: $MAINTAINER
homepage: $HOMEPAGE
license: LGPL-3.0-only
contents:
  - src: $BUNDLE/$BINARY
    dst: /opt/$PACKAGE/$BINARY
    file_info:
      mode: 0755
  - src: $BUNDLE/lib
    dst: /opt/$PACKAGE/lib
    type: tree
  - src: /opt/$PACKAGE/$BINARY
    dst: /usr/bin/$BINARY
    type: symlink
  - src: $BUNDLE/share/applications/$BINARY.desktop
    dst: /usr/share/applications/$BINARY.desktop
  - src: $BUNDLE/share/icons
    dst: /usr/share/icons
    type: tree
  - src: $BUNDLE/licences
    dst: /usr/share/doc/$PACKAGE
    type: tree
overrides:
  deb:
    depends:
$(yaml_list "${deb_list[@]}")
    recommends:
      - "mesa-vulkan-drivers | vulkan-icd"
  rpm:
    depends:
$(yaml_list "${rpm_depends[@]}")
    recommends:
      - "mesa-vulkan-drivers"
YAML

OUT_BASE="$ROOT/target/bundle/${NAME// /-}-$VERSION-linux-$ARCH"
for packager in deb rpm; do
  rm -f "$OUT_BASE.$packager"
  "$NFPM" package --config "$WORK/nfpm.yaml" --packager "$packager" --target "$OUT_BASE.$packager" >/dev/null
done

echo
echo "Packaged $NAME $VERSION for Linux ($ARCH):"
echo "  $OUT_BASE.deb"
echo "    Depends: $deb_depends"
echo "  $OUT_BASE.rpm"
echo "    Requires: ${rpm_depends[*]}"
