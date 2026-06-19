#!/usr/bin/env bash
set -euo pipefail

# package-release.sh: Bundle the basemind binary with its dynamically-linked
# non-system native libraries (ONNX Runtime, Tesseract, Leptonica, libheif, image
# codecs, ...) into a self-contained, relocatable archive.
#
# Called after `cargo build --release --features full --bin basemind --target <triple>`,
# so the binary lives at `target/<triple>/release/`.
#
# Usage: package-release.sh <target-triple>
#   x86_64-unknown-linux-gnu | aarch64-unknown-linux-gnu
#   x86_64-apple-darwin       | aarch64-apple-darwin
#   x86_64-pc-windows-msvc
#
# Output: basemind-<triple>.tar.gz   (Linux/macOS: binary + lib/ at archive root)
#         basemind-<triple>.zip      (Windows: basemind.exe + *.dll at archive root)
# NOTE: the archive contents are at the ROOT (no leading staging-dir component) so
# the npm/pip/launcher consumers extract straight into their bin dir.

if [ $# -ne 1 ]; then
  echo "Usage: $0 <target-triple>" >&2
  exit 1
fi

TRIPLE="$1"

case "$TRIPLE" in
  x86_64-unknown-linux-gnu | aarch64-unknown-linux-gnu) SYSTEM="linux"; BINEXT="" ;;
  x86_64-apple-darwin | aarch64-apple-darwin) SYSTEM="macos"; BINEXT="" ;;
  x86_64-pc-windows-msvc) SYSTEM="windows"; BINEXT=".exe" ;;
  *) echo "Unknown target triple: $TRIPLE" >&2; exit 1 ;;
esac

# cargo build --target <triple> writes here (NOT target/release/).
RELEASE_DIR="target/${TRIPLE}/release"
BINARY_PATH="${RELEASE_DIR}/basemind${BINEXT}"

if [ ! -f "$BINARY_PATH" ]; then
  echo "Binary not found at $BINARY_PATH" >&2
  exit 1
fi

STAGING_DIR="basemind-staging-${TRIPLE}"
rm -rf "$STAGING_DIR"
mkdir -p "$STAGING_DIR/lib"
cp "$BINARY_PATH" "$STAGING_DIR/basemind${BINEXT}"
BIN_IN_STAGING="$STAGING_DIR/basemind${BINEXT}"

case "$SYSTEM" in
  linux)
    echo "Gathering Linux dynamic dependencies (ldd transitive closure)..."
    # ldd already reports the full transitive closure. Bundle everything EXCEPT
    # the glibc core (libc/libm/pthread/dl/rt/resolv, the loader, libgcc_s) — those
    # must come from the host to avoid ABI breakage. App libs that apt installs into
    # system dirs (e.g. libtesseract in /usr/lib/<triple>) ARE bundled.
    declare -A COPIED
    while IFS= read -r line; do
      lib=$(awk '{ for (i=1;i<=NF;i++){ if ($i=="=>" && $(i+1) ~ /^\//){print $(i+1); exit} if ($i ~ /^\// && $i !~ /^\(/){print $i; exit} } }' <<<"$line")
      [ -n "$lib" ] && [ -f "$lib" ] || continue
      base=$(basename "$lib")
      case "$base" in
        libc.so* | libm.so* | libpthread.so* | libdl.so* | librt.so* | libresolv.so* | \
        ld-linux*.so* | ld-musl*.so* | libgcc_s.so*) continue ;;
      esac
      [ -n "${COPIED[$base]:-}" ] && continue
      cp -L "$lib" "$STAGING_DIR/lib/" 2>/dev/null && COPIED[$base]=1 || true
    done < <(ldd "$BIN_IN_STAGING" 2>/dev/null || true)

    # ORT (ort/download-binaries) may drop its .so into the build deps dir rather
    # than a system path ldd resolves.
    if [ -d "${RELEASE_DIR}/deps" ]; then
      for lib in "${RELEASE_DIR}/deps"/*.so*; do
        [ -f "$lib" ] || continue
        base=$(basename "$lib"); [ -n "${COPIED[$base]:-}" ] && continue
        cp -L "$lib" "$STAGING_DIR/lib/" 2>/dev/null && COPIED[$base]=1 || true
      done
    fi

    patchelf --set-rpath '$ORIGIN/lib' "$BIN_IN_STAGING"
    tar czf "basemind-${TRIPLE}.tar.gz" -C "$STAGING_DIR" .
    echo "✓ Created basemind-${TRIPLE}.tar.gz"
    ;;

  macos)
    echo "Gathering macOS dynamic dependencies (otool transitive closure)..."
    # BFS over otool -L starting at the binary. Bundle any referenced dylib that is
    # NOT an OS lib (/usr/lib, /System). Homebrew deps live under /opt/homebrew (arm)
    # or /usr/local (x86) — both are bundled. Track each lib's install-name token as
    # it appears in load commands so install_name_tool -change targets the right id.
    declare -A COPIED
    declare -a QUEUE=("$BIN_IN_STAGING")
    while [ ${#QUEUE[@]} -gt 0 ]; do
      cur="${QUEUE[0]}"; QUEUE=("${QUEUE[@]:1}")
      # iterate dependency lines (skip the first line: the object's own id)
      otool -L "$cur" 2>/dev/null | tail -n +2 | while read -r dep _; do echo "$dep"; done > /tmp/_otool_$$ || true
      while IFS= read -r dep; do
        [ -n "$dep" ] || continue
        case "$dep" in
          /usr/lib/* | /System/*) continue ;;
          @rpath/* | @loader_path/* | @executable_path/*) continue ;; # already relocated
        esac
        [ -f "$dep" ] || continue
        base=$(basename "$dep")
        [ -n "${COPIED[$base]:-}" ] && continue
        cp -L "$dep" "$STAGING_DIR/lib/$base" 2>/dev/null || continue
        chmod u+w "$STAGING_DIR/lib/$base" 2>/dev/null || true
        COPIED[$base]="$dep"
        QUEUE+=("$STAGING_DIR/lib/$base")
      done < /tmp/_otool_$$
      rm -f /tmp/_otool_$$
    done

    # Rewrite install names: each bundled lib -> @loader_path/lib/<name>, and rewrite
    # references in the binary and in every bundled lib.
    for base in "${!COPIED[@]}"; do
      old="${COPIED[$base]}"
      install_name_tool -id "@loader_path/lib/$base" "$STAGING_DIR/lib/$base" 2>/dev/null || true
      install_name_tool -change "$old" "@loader_path/lib/$base" "$BIN_IN_STAGING" 2>/dev/null || true
      for other in "$STAGING_DIR/lib/"*.dylib; do
        [ -f "$other" ] || continue
        install_name_tool -change "$old" "@loader_path/$base" "$other" 2>/dev/null || true
      done
    done
    install_name_tool -add_rpath "@loader_path/lib" "$BIN_IN_STAGING" 2>/dev/null || true

    tar czf "basemind-${TRIPLE}.tar.gz" -C "$STAGING_DIR" .
    echo "✓ Created basemind-${TRIPLE}.tar.gz"
    ;;

  windows)
    echo "Gathering Windows DLL dependencies..."
    # vcpkg's libheif:x64-windows-static-md links libheif statically; the dynamic
    # piece is ONNX Runtime, co-located next to the .exe (Windows resolves DLLs from
    # the exe directory). MSVC runtime is assumed present on the host.
    declare -A COPIED
    if [ -d "${RELEASE_DIR}/deps" ]; then
      for dll in "${RELEASE_DIR}/deps"/*.dll; do
        [ -f "$dll" ] || continue
        base=$(basename "$dll"); [ -n "${COPIED[$base]:-}" ] && continue
        cp -L "$dll" "$STAGING_DIR/" 2>/dev/null && COPIED[$base]=1 || true
      done
    fi
    for ort_path in "C:/Program Files/onnxruntime" "C:/Program Files (x86)/onnxruntime" "${ONNXRUNTIME_ROOT:-}"; do
      [ -n "$ort_path" ] && [ -d "$ort_path/lib" ] || continue
      for dll in "$ort_path/lib"/*.dll; do
        [ -f "$dll" ] || continue
        base=$(basename "$dll"); [ -n "${COPIED[$base]:-}" ] && continue
        cp -L "$dll" "$STAGING_DIR/" 2>/dev/null && COPIED[$base]=1 || true
      done
    done

    (cd "$STAGING_DIR" && {
      7z a -tzip "../basemind-${TRIPLE}.zip" . >/dev/null 2>&1 ||
        zip -q -r "../basemind-${TRIPLE}.zip" . ||
        powershell -Command "Compress-Archive -Path '*' -DestinationPath '../basemind-${TRIPLE}.zip' -Force"
    })
    echo "✓ Created basemind-${TRIPLE}.zip"
    ;;
esac

rm -rf "$STAGING_DIR"
echo "✓ Release package ready: basemind-${TRIPLE}.$([ "$SYSTEM" = "windows" ] && echo "zip" || echo "tar.gz")"
