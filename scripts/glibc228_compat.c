/*
 * glibc 2.28 compatibility shim for ort's prebuilt ONNX Runtime.
 *
 * The pyke prebuilt ONNX Runtime (libort_sys) shipped via the `ort` crate is
 * compiled against glibc >= 2.38 and references symbols that do not exist in
 * glibc 2.28 (the manylinux_2_28 / RHEL 8 / Debian 11 floor):
 *
 *   __isoc23_strtol / __isoc23_strtoll / __isoc23_strtoull  (glibc 2.38, C23)
 *   __libc_single_threaded                                  (glibc 2.32)
 *
 * Linking this object into the release binary backfills those symbols so the
 * prebuilt links against a 2.28 floor without recompiling ONNX Runtime from
 * source. It is linked ONLY by the Linux release build (via a target-scoped
 * RUSTFLAGS link-arg in the publish workflow) and never by a normal
 * `cargo install basemind`, where the host glibc already provides them.
 */
#include <stdlib.h>

/*
 * glibc 2.32+. 0 means "the process may be multi-threaded", forcing libc and
 * any consumer to take the thread-safe path. Conservative and correct on any
 * glibc; the only cost is foregoing a single-threaded fast path.
 */
char __libc_single_threaded = 0;

/*
 * glibc 2.38 C23 variants. They differ from the classic functions only by
 * accepting C23 binary (0b) integer literals under base 0/2; delegating to the
 * classic strtol family is safe for ONNX Runtime's integer parsing.
 */
long __isoc23_strtol(const char *nptr, char **endptr, int base) {
    return strtol(nptr, endptr, base);
}

long long __isoc23_strtoll(const char *nptr, char **endptr, int base) {
    return strtoll(nptr, endptr, base);
}

unsigned long long __isoc23_strtoull(const char *nptr, char **endptr, int base) {
    return strtoull(nptr, endptr, base);
}
