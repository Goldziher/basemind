#!/usr/bin/env bash
set -euo pipefail

# NOTE: the archive contents are at the ROOT (no leading staging-dir component) so

if [ $# -ne 1 ]; then
	echo "Usage: $0 <target-triple>" >&2
	exit 1
fi

TRIPLE="$1"

case "$TRIPLE" in
x86_64-unknown-linux-gnu | aarch64-unknown-linux-gnu)
	SYSTEM="linux"
	BINEXT=""
	;;
aarch64-apple-darwin | x86_64-apple-darwin)
	SYSTEM="macos"
	BINEXT=""
	;;
x86_64-pc-windows-msvc)
	SYSTEM="windows"
	BINEXT=".exe"
	;;
*)
	echo "Unknown target triple: $TRIPLE" >&2
	exit 1
	;;
esac

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
	while IFS= read -r line; do
		lib=$(awk '{ for (i=1;i<=NF;i++){ if ($i=="=>" && $(i+1) ~ /^\//){print $(i+1); exit} if ($i ~ /^\// && $i !~ /^\(/){print $i; exit} } }' <<<"$line")
		[ -n "$lib" ] && [ -f "$lib" ] || continue
		base=$(basename "$lib")
		case "$base" in
		libc.so* | libm.so* | libpthread.so* | libdl.so* | librt.so* | libresolv.so* | \
			ld-linux*.so* | ld-musl*.so* | libgcc_s.so*) continue ;;
		esac
		[ -f "$STAGING_DIR/lib/$base" ] && continue
		cp -L "$lib" "$STAGING_DIR/lib/" 2>/dev/null || true
	done < <(ldd "$BIN_IN_STAGING" 2>/dev/null || true)

	if [ -d "${RELEASE_DIR}/deps" ]; then
		for lib in "${RELEASE_DIR}/deps"/*.so*; do
			[ -f "$lib" ] || continue
			base=$(basename "$lib")
			[ -f "$STAGING_DIR/lib/$base" ] && continue
			cp -L "$lib" "$STAGING_DIR/lib/" 2>/dev/null || true
		done
	fi

	# shellcheck disable=SC2016  # literal $ORIGIN is intended — patchelf/ld expands it at load time
	patchelf --set-rpath '$ORIGIN/lib' "$BIN_IN_STAGING"
	for so in "$STAGING_DIR/lib/"*.so*; do
		[ -f "$so" ] || continue
		# shellcheck disable=SC2016  # literal $ORIGIN is intended — patchelf/ld expands it at load time
		patchelf --set-rpath '$ORIGIN' "$so" 2>/dev/null || true
	done
	tar czf "basemind-${TRIPLE}.tar.gz" -C "$STAGING_DIR" .
	echo "✓ Created basemind-${TRIPLE}.tar.gz"
	;;

macos)
	echo "Gathering macOS dynamic dependencies (otool transitive closure)..."
	copied_bases=()
	copied_olds=()
	was_copied() {
		[ ${#copied_bases[@]} -eq 0 ] && return 1
		local b="$1" existing
		for existing in "${copied_bases[@]}"; do
			[ "$existing" = "$b" ] && return 0
		done
		return 1
	}
	QUEUE=("$BIN_IN_STAGING")
	while [ ${#QUEUE[@]} -gt 0 ]; do
		cur="${QUEUE[0]}"
		QUEUE=("${QUEUE[@]:1}")
		otool -L "$cur" 2>/dev/null | tail -n +2 | while read -r dep _; do echo "$dep"; done >/tmp/_otool_$$ || true
		while IFS= read -r dep; do
			[ -n "$dep" ] || continue
			case "$dep" in
			/usr/lib/* | /System/*) continue ;;
			@rpath/* | @loader_path/* | @executable_path/*) continue ;;
			esac
			[ -f "$dep" ] || continue
			base=$(basename "$dep")
			was_copied "$base" && continue
			cp -L "$dep" "$STAGING_DIR/lib/$base" 2>/dev/null || continue
			chmod u+w "$STAGING_DIR/lib/$base" 2>/dev/null || true
			copied_bases+=("$base")
			copied_olds+=("$dep")
			QUEUE+=("$STAGING_DIR/lib/$base")
		done </tmp/_otool_$$
		rm -f /tmp/_otool_$$
	done

	idx=0
	while [ "$idx" -lt ${#copied_bases[@]} ]; do
		base="${copied_bases[$idx]}"
		old="${copied_olds[$idx]}"
		install_name_tool -id "@loader_path/lib/$base" "$STAGING_DIR/lib/$base" 2>/dev/null || true
		install_name_tool -change "$old" "@loader_path/lib/$base" "$BIN_IN_STAGING" 2>/dev/null || true
		for other in "$STAGING_DIR/lib/"*.dylib; do
			[ -f "$other" ] || continue
			install_name_tool -change "$old" "@loader_path/$base" "$other" 2>/dev/null || true
		done
		idx=$((idx + 1))
	done
	install_name_tool -add_rpath "@loader_path/lib" "$BIN_IN_STAGING" 2>/dev/null || true

	echo "Re-signing bundled dylibs + binary (ad-hoc) after install_name_tool..."
	for dylib in "$STAGING_DIR/lib/"*.dylib; do
		[ -f "$dylib" ] || continue
		codesign --force --sign - "$dylib"
	done
	codesign --force --sign - "$BIN_IN_STAGING"
	codesign --verify --strict "$BIN_IN_STAGING"

	if [ "$TRIPLE" = "x86_64-apple-darwin" ]; then
		echo "Vendoring ONNX Runtime (ort-dynamic) for Intel macOS..."
		export HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK=1
		brew install --bottle-tag=sonoma onnxruntime || brew install onnxruntime
		ORT_PREFIX="$(brew --prefix onnxruntime)/lib"
		ort_lib="$ORT_PREFIX/libonnxruntime.dylib"
		[ -f "$ort_lib" ] || {
			echo "ONNX Runtime dylib not found at $ort_lib" >&2
			exit 1
		}
		cp "$ort_lib" "$STAGING_DIR/libonnxruntime.dylib"
		chmod u+w "$STAGING_DIR/libonnxruntime.dylib"
		install_name_tool -id "@loader_path/libonnxruntime.dylib" "$STAGING_DIR/libonnxruntime.dylib"
		changed=1
		while [ "$changed" = 1 ]; do
			changed=0
			for lib in "$STAGING_DIR"/*.dylib; do
				[ -f "$lib" ] || continue
				while IFS= read -r dep; do
					[ -n "$dep" ] || continue
					base=$(basename "$dep")
					src="$dep"
					case "$dep" in
					@rpath/*) src="$ORT_PREFIX/$base" ;;
					esac
					if [ ! -f "$STAGING_DIR/$base" ]; then
						[ -f "$src" ] || continue
						cp "$src" "$STAGING_DIR/$base"
						chmod u+w "$STAGING_DIR/$base"
						install_name_tool -id "@loader_path/$base" "$STAGING_DIR/$base"
						changed=1
					fi
					install_name_tool -change "$dep" "@loader_path/$base" "$lib" 2>/dev/null || true
				done < <(otool -L "$lib" | tail -n +2 | awk '{print $1}' |
					grep -E '^(/opt/homebrew|/usr/local|@rpath)/' || true)
			done
		done
		for dylib in "$STAGING_DIR"/*.dylib; do
			[ -f "$dylib" ] || continue
			codesign --force --sign - "$dylib"
		done
		echo "✓ Vendored ONNX Runtime + closure next to the binary"
	fi

	tar czf "basemind-${TRIPLE}.tar.gz" -C "$STAGING_DIR" .
	echo "✓ Created basemind-${TRIPLE}.tar.gz"
	;;

windows)
	echo "Gathering Windows DLL dependencies..."
	if [ -d "${RELEASE_DIR}/deps" ]; then
		for dll in "${RELEASE_DIR}/deps"/*.dll; do
			[ -f "$dll" ] || continue
			base=$(basename "$dll")
			[ -f "$STAGING_DIR/$base" ] && continue
			cp -L "$dll" "$STAGING_DIR/" 2>/dev/null || true
		done
	fi
	for ort_path in "C:/Program Files/onnxruntime" "C:/Program Files (x86)/onnxruntime" "${ONNXRUNTIME_ROOT:-}"; do
		[ -n "$ort_path" ] && [ -d "$ort_path/lib" ] || continue
		for dll in "$ort_path/lib"/*.dll; do
			[ -f "$dll" ] || continue
			base=$(basename "$dll")
			[ -f "$STAGING_DIR/$base" ] && continue
			cp -L "$dll" "$STAGING_DIR/" 2>/dev/null || true
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
