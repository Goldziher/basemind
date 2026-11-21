#!/usr/bin/env bash
# Builds libde265 and libheif from pinned release tarballs and installs both as shared libraries under
# <prefix>, for the `documents-heic` Cargo feature on Linux and macOS (Windows takes the same versions
# from a pinned vcpkg, see .github/actions/install-libheif). libheif decodes HEVC through libde265 and
# is built with every other codec off, the GPL x265 encoder included, and without plugin loading, so
# it neither links nor loads an encoder. Release archives bundle both libraries under their LGPL
# notices (licenses/libheif.txt, licenses/libde265.txt; the versions here are named in
# ATTRIBUTIONS.md, which scripts/check_native_notices.py checks).
#
# Usage: build-libheif.sh <prefix>
#
# A prefix that already holds this libheif version is left alone, so a restored cache costs nothing.
# On macOS the install names are absolute paths under <prefix>, which package-release.sh rewrites
# when it bundles the libraries; MACOSX_DEPLOYMENT_TARGET is honoured as for any CMake build.
set -euo pipefail

LIBDE265_VERSION=1.1.3
LIBDE265_SHA256=554228bd17788c99a7e63b37ab5634722190e6e2bf60c1dcb01cef328e133905
LIBHEIF_VERSION=1.23.7
LIBHEIF_SHA256=f7483e818503ea6224c8863e4a56ac02c7089021a98472c811c12aa19faf2475

if [ $# -ne 1 ]; then
  echo "Usage: $0 <prefix>" >&2
  exit 2
fi

mkdir -p "$1"
prefix="$(cd "$1" && pwd)"
pc="$prefix/lib/pkgconfig/libheif.pc"
if [ -f "$pc" ] && grep -qx "Version: $LIBHEIF_VERSION" "$pc"; then
  echo "✓ libheif $LIBHEIF_VERSION is already installed in $prefix"
  exit 0
fi

jobs="$(getconf _NPROCESSORS_ONLN)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Downloads the release tarball of strukturag/<name> at v<version>, checks it against <sha256> and
# unpacks it into $work/<name>-<version>.
fetch() {
  local name="$1" version="$2" sha256="$3"
  local tarball="$work/$name-$version.tar.gz"
  curl -fsSL -o "$tarball" "https://github.com/strukturag/$name/releases/download/v$version/$name-$version.tar.gz"
  if command -v sha256sum >/dev/null; then
    echo "$sha256  $tarball" | sha256sum -c -
  else
    echo "$sha256  $tarball" | shasum -a 256 -c -
  fi
  tar xzf "$tarball" -C "$work"
}

# Configures, builds and installs the CMake project in $work/<source> into the prefix.
install_cmake_project() {
  local source="$1"
  shift
  cmake -S "$work/$source" -B "$work/$source/build" \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX="$prefix" \
    -DCMAKE_INSTALL_LIBDIR=lib \
    -DCMAKE_INSTALL_NAME_DIR="$prefix/lib" \
    -DCMAKE_PREFIX_PATH="$prefix" \
    -DBUILD_SHARED_LIBS=ON \
    -DBUILD_TESTING=OFF \
    "$@"
  cmake --build "$work/$source/build" --parallel "$jobs"
  cmake --install "$work/$source/build"
}

echo "::group::Building libde265 $LIBDE265_VERSION"
fetch libde265 "$LIBDE265_VERSION" "$LIBDE265_SHA256"
install_cmake_project "libde265-$LIBDE265_VERSION" \
  -DENABLE_SDL=OFF \
  -DENABLE_DECODER=OFF \
  -DENABLE_ENCODER=OFF
echo "::endgroup::"

echo "::group::Building libheif $LIBHEIF_VERSION (libde265 decoder only, no x265)"
fetch libheif "$LIBHEIF_VERSION" "$LIBHEIF_SHA256"
export PKG_CONFIG_PATH="$prefix/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
install_cmake_project "libheif-$LIBHEIF_VERSION" \
  -DWITH_LIBDE265=ON \
  -DWITH_LIBDE265_PLUGIN=OFF \
  -DWITH_X265=OFF \
  -DWITH_X264=OFF \
  -DWITH_KVAZAAR=OFF \
  -DWITH_UVG266=OFF \
  -DWITH_VVDEC=OFF \
  -DWITH_VVENC=OFF \
  -DWITH_OpenH264_DECODER=OFF \
  -DWITH_OpenH264_ENCODER=OFF \
  -DWITH_FFMPEG_DECODER=OFF \
  -DWITH_DAV1D=OFF \
  -DWITH_AOM_DECODER=OFF \
  -DWITH_AOM_ENCODER=OFF \
  -DWITH_SvtEnc=OFF \
  -DWITH_RAV1E=OFF \
  -DWITH_JPEG_DECODER=OFF \
  -DWITH_JPEG_ENCODER=OFF \
  -DWITH_OpenJPEG_DECODER=OFF \
  -DWITH_OpenJPEG_ENCODER=OFF \
  -DWITH_OPENJPH_DECODER=OFF \
  -DWITH_OPENJPH_ENCODER=OFF \
  -DWITH_UNCOMPRESSED_CODEC=OFF \
  -DWITH_HEADER_COMPRESSION=OFF \
  -DWITH_LIBSHARPYUV=OFF \
  -DWITH_GDK_PIXBUF=OFF \
  -DWITH_EXAMPLES=OFF \
  -DWITH_EXAMPLE_HEIF_THUMB=OFF \
  -DWITH_EXAMPLE_HEIF_VIEW=OFF \
  -DBUILD_DOCUMENTATION=OFF \
  -DENABLE_PLUGIN_LOADING=OFF
echo "::endgroup::"

case "$(uname -s)" in
Darwin) linked="$(otool -L "$prefix/lib/libheif.1.dylib")" ;;
*) linked="$(objdump -p "$prefix/lib/libheif.so.1" | grep NEEDED)" ;;
esac
if ! grep -q libde265 <<<"$linked" || grep -qi x265 <<<"$linked"; then
  echo "error: libheif must link libde265 and not x265; it links:" >&2
  echo "$linked" >&2
  exit 1
fi
echo "✓ libheif $LIBHEIF_VERSION with libde265 $LIBDE265_VERSION installed in $prefix"
