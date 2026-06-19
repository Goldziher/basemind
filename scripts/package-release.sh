#!/usr/bin/env bash
set -euo pipefail

# package-release.sh: Bundle basemind binary with dynamically-linked native libs
# into a portable archive. Called after `cargo build --release --features full`.
#
# Usage: package-release.sh <target-triple>
#   target-triple: x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu,
#                  x86_64-apple-darwin, aarch64-apple-darwin,
#                  x86_64-pc-windows-msvc

if [ $# -ne 1 ]; then
  echo "Usage: $0 <target-triple>" >&2
  exit 1
fi

TRIPLE="$1"

# Derive paths and binary name from the triple
case "$TRIPLE" in
  x86_64-unknown-linux-gnu)
    BINARY_PATH="target/release/basemind"
    SYSTEM="linux"
    BINEXT=""
    ;;
  aarch64-unknown-linux-gnu)
    BINARY_PATH="target/release/basemind"
    SYSTEM="linux"
    BINEXT=""
    ;;
  x86_64-apple-darwin)
    BINARY_PATH="target/release/basemind"
    SYSTEM="macos"
    BINEXT=""
    ;;
  aarch64-apple-darwin)
    BINARY_PATH="target/release/basemind"
    SYSTEM="macos"
    BINEXT=""
    ;;
  x86_64-pc-windows-msvc)
    BINARY_PATH="target/release/basemind.exe"
    SYSTEM="windows"
    BINEXT=".exe"
    ;;
  *)
    echo "Unknown target triple: $TRIPLE" >&2
    exit 1
    ;;
esac

if [ ! -f "$BINARY_PATH" ]; then
  echo "Binary not found at $BINARY_PATH" >&2
  exit 1
fi

# Create staging directory
STAGING_DIR="basemind-staging-$TRIPLE"
rm -rf "$STAGING_DIR"
mkdir -p "$STAGING_DIR/lib"

# Copy the binary to the staging directory
cp "$BINARY_PATH" "$STAGING_DIR/"

case "$SYSTEM" in
  linux)
    echo "Gathering Linux dynamic dependencies..."

    # Use ldd to recursively collect all dynamic libraries
    LIBS_TO_COPY=()

    # Collect libs via ldd, filtering out system libs and the kernel loader
    while IFS= read -r line; do
      # Extract the library path (first token that starts with / or the second token if =>)
      lib=$(echo "$line" | awk '{
        for (i=1; i<=NF; i++) {
          if ($i ~ /^\//) {
            print $i
            break
          }
          if ($i == "=>") {
            if ($(i+1) !~ /^\(/) {
              print $(i+1)
              break
            }
          }
        }
      }')

      if [ -n "$lib" ] && [ -f "$lib" ]; then
        # Skip system lib paths (/lib/x86_64-linux-gnu, /usr/lib, /lib64)
        if ! [[ "$lib" =~ ^/lib/x86_64-linux-gnu/ ]] && \
           ! [[ "$lib" =~ ^/usr/lib/ ]] && \
           ! [[ "$lib" =~ ^/lib64/ ]] && \
           ! [[ "$lib" =~ ^/lib/aarch64-linux-gnu/ ]]; then
          LIBS_TO_COPY+=("$lib")
        fi
      fi
    done < <(ldd "$STAGING_DIR/basemind" 2>/dev/null || true)

    # Also check for libs via otool-like inspection for non-standard paths
    # This catches onnxruntime and kreuzberg-bundled libs
    if command -v ldd >/dev/null 2>&1; then
      # Additional sweep: find any .so libs that might be in the build tree
      if [ -d "target/release/deps" ]; then
        for lib in target/release/deps/*.so* ; do
          [ -f "$lib" ] || continue
          LIBS_TO_COPY+=("$lib")
        done
      fi
    fi

    # Deduplicate and copy
    for lib in "${LIBS_TO_COPY[@]}"; do
      if [ -f "$lib" ]; then
        cp -L "$lib" "$STAGING_DIR/lib/" 2>/dev/null || true
      fi
    done

    # Set rpath so the binary finds libs in ./lib
    patchelf --set-rpath '$ORIGIN/lib' "$STAGING_DIR/basemind"

    # Create the archive
    tar czf "basemind-${TRIPLE}.tar.gz" "$STAGING_DIR"
    echo "✓ Created basemind-${TRIPLE}.tar.gz"
    ;;

  macos)
    echo "Gathering macOS dynamic dependencies..."

    # Use otool -L to recursively collect dylib dependencies
    LIBS_TO_COPY=()

    while IFS= read -r line; do
      # Extract dylib path (skip lines starting with Tab or lines with @)
      dylib=$(echo "$line" | sed -n 's/^[[:space:]]*\([^[:space:]]*\.dylib\).*/\1/p')

      if [ -n "$dylib" ] && [ -f "$dylib" ]; then
        # Skip system dylibs in /usr/lib, /System, /opt/homebrew (Homebrew frameworks)
        if ! [[ "$dylib" =~ ^/usr/lib/ ]] && \
           ! [[ "$dylib" =~ ^/System/ ]] && \
           ! [[ "$dylib" =~ ^/opt/homebrew/ ]]; then
          LIBS_TO_COPY+=("$dylib")
        fi
      fi
    done < <(otool -L "$STAGING_DIR/basemind" 2>/dev/null || true)

    # Additional sweep for kreuzberg/ort bundled libs in usr/local
    if [ -d "/usr/local/lib" ]; then
      for lib in /usr/local/lib/*.dylib ; do
        [ -f "$lib" ] || continue
        LIBS_TO_COPY+=("$lib")
      done
    fi

    # Deduplicate and copy
    declare -A COPIED
    for lib in "${LIBS_TO_COPY[@]}"; do
      if [ -f "$lib" ] && [ -z "${COPIED[$lib]:-}" ]; then
        cp -L "$lib" "$STAGING_DIR/lib/" 2>/dev/null || true
        COPIED["$lib"]=1
      fi
    done

    # Update install_name_tool references so the binary finds libs at @loader_path/lib
    if [ -d "$STAGING_DIR/lib" ] && [ "$(ls -A "$STAGING_DIR/lib")" ]; then
      for dylib in "$STAGING_DIR/lib"/*.dylib; do
        if [ -f "$dylib" ]; then
          # Update the dylib's own install_name
          dylib_name=$(basename "$dylib")
          install_name_tool -id "@loader_path/lib/$dylib_name" "$dylib" || true

          # Update references in the main binary to use @loader_path
          install_name_tool -change "$dylib" "@loader_path/lib/$dylib_name" "$STAGING_DIR/basemind" || true
        fi
      done
    fi

    # Set rpath on the binary itself
    install_name_tool -add_rpath "@loader_path/lib" "$STAGING_DIR/basemind" 2>/dev/null || true

    # Create the archive
    tar czf "basemind-${TRIPLE}.tar.gz" "$STAGING_DIR"
    echo "✓ Created basemind-${TRIPLE}.tar.gz"
    ;;

  windows)
    echo "Gathering Windows dynamic dependencies..."

    # On Windows, MSVC runtime is shared system-wide, and ONNX Runtime provides runtime dependencies.
    # vcpkg's libheif:x64-windows-static-md links libheif statically, so we collect the dynamic ONNX Runtime DLLs.

    DLLS_TO_COPY=()

    # Check for ONNX Runtime DLLs (typically in Program Files or bundled by cargo-ort)
    if [ -d "target/release/deps" ]; then
      for dll in target/release/deps/*.dll ; do
        [ -f "$dll" ] || continue
        DLLS_TO_COPY+=("$dll")
      done
    fi

    # Also check common ORT install paths
    ORT_PATHS=(
      "C:/Program Files/onnxruntime"
      "C:/Program Files (x86)/onnxruntime"
      "${ONNXRUNTIME_ROOT}"
    )

    for ort_path in "${ORT_PATHS[@]}"; do
      if [ -d "$ort_path/lib" ]; then
        for dll in "$ort_path/lib"/*.dll ; do
          [ -f "$dll" ] || continue
          DLLS_TO_COPY+=("$dll")
        done
      fi
    done

    # Deduplicate and copy DLLs to the staging dir (co-located with the binary)
    declare -A COPIED
    for dll in "${DLLS_TO_COPY[@]}"; do
      if [ -f "$dll" ] && [ -z "${COPIED[$dll]:-}" ]; then
        cp -L "$dll" "$STAGING_DIR/" 2>/dev/null || true
        COPIED["$dll"]=1
      fi
    done

    # Create the zip archive
    cd "$STAGING_DIR"
    7z a -tzip "../basemind-${TRIPLE}.zip" . >/dev/null 2>&1 || \
      zip -q -r "../basemind-${TRIPLE}.zip" . || \
      powershell -Command "Compress-Archive -Path '*' -DestinationPath '../basemind-${TRIPLE}.zip' -Force"
    cd ..

    echo "✓ Created basemind-${TRIPLE}.zip"
    ;;
esac

# Cleanup
rm -rf "$STAGING_DIR"

echo "✓ Release package ready: basemind-${TRIPLE}.$([ "$SYSTEM" = "windows" ] && echo "zip" || echo "tar.gz")"
