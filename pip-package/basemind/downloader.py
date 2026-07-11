from __future__ import annotations

import hashlib
import os
import platform
import shutil
import ssl
import subprocess
import sys
import tempfile
import tarfile
import time
import zipfile
from pathlib import Path
from urllib.error import URLError
from urllib.request import Request, urlopen

import certifi


def _sysctl(name: str) -> str:
    """Return the trimmed value of a sysctl, or "" if it can't be read."""
    try:
        result = subprocess.run(
            ["sysctl", "-n", name],
            capture_output=True,
            text=True,
            check=False,
        )
    except (OSError, ValueError):
        return ""
    return result.stdout.strip()


def _is_apple_silicon(machine: str) -> bool:
    """Detect Apple Silicon hardware, even from an x86_64 process under Rosetta.

    ``platform.machine()`` reflects the *process* arch, so an x86_64 Python under
    Rosetta reports ``x86_64`` on Apple Silicon hardware. Two hardware signals
    resolve it, either of which is conclusive:

    * ``sysctl.proc_translated`` = 1 → running under Rosetta, which exists ONLY on
      Apple Silicon. Rosetta MASKS ``hw.optional.arm64``, so that check alone misses
      the Rosetta case — probe ``proc_translated`` too.
    * ``hw.optional.arm64`` = 1 → native arm64 process.
    """
    if machine in {"aarch64", "arm64"}:
        return True
    return _sysctl("sysctl.proc_translated") == "1" or _sysctl("hw.optional.arm64") == "1"


def _platform_triple() -> str:
    system = platform.system().lower()
    machine = platform.machine().lower()

    if system == "windows":
        if machine in {"amd64", "x86_64"}:
            return "x86_64-pc-windows-msvc"
        if machine in {"x86", "i386", "i686"}:
            raise RuntimeError("32-bit Windows is not supported")
    elif system == "linux":
        if machine in {"amd64", "x86_64"}:
            return "x86_64-unknown-linux-gnu"
        if machine in {"aarch64", "arm64"}:
            return "aarch64-unknown-linux-gnu"
    elif system == "darwin":
        return "aarch64-apple-darwin" if _is_apple_silicon(machine) else "x86_64-apple-darwin"

    raise RuntimeError(f"Unsupported platform: {system} {machine}")


def _python_version_to_tag(version: str) -> str:
    if "rc" in version:
        core, suffix = version.split("rc")
        return f"{core}-rc.{suffix}"
    return version


def _asset(version: str) -> tuple[str, str, str, str]:
    """Return (archive_url, ext, asset_name, checksums_url) for this platform."""
    tag = _python_version_to_tag(version)
    triple = _platform_triple()
    ext = "zip" if "windows" in triple else "tar.gz"
    asset_name = f"basemind-{triple}.{ext}"
    base = f"https://github.com/Goldziher/basemind/releases/download/v{tag}"
    archive_url = f"{base}/{asset_name}"
    checksums_url = f"{base}/basemind_{tag}_checksums.txt"
    return archive_url, ext, asset_name, checksums_url


def _is_retryable_error(error: Exception | str) -> bool:
    """Check if an error is transient and worth retrying."""
    error_str = str(error).lower()
    return any(
        substring in error_str
        for substring in [
            "timeout",
            "connection",
            "refused",
            "reset",
            "unreachable",
            "http 5",
            "temporarily unavailable",
        ]
    )


def _retry_with_backoff(fn, max_attempts: int = 3, delays: list[int] | None = None) -> None:
    """Execute fn with exponential backoff retry on transient errors.

    Only retries on transient errors (network, 5xx). Deterministic failures
    (404, bad checksum) propagate immediately.
    """
    if delays is None:
        delays = [1, 2, 4]

    last_error = None
    for attempt in range(max_attempts):
        try:
            return fn()
        except Exception as error:
            last_error = error
            if not _is_retryable_error(error) or attempt >= max_attempts - 1:
                raise

            delay = delays[attempt]
            print(
                f"Transient error (attempt {attempt + 1}/{max_attempts}): {error}; retrying in {delay}s...",
                file=sys.stderr,
            )
            time.sleep(delay)

    if last_error:
        raise last_error


def _download(url: str, destination: Path) -> None:
    """Download a file with retry-with-backoff on transient errors."""

    def download_attempt():
        request = Request(url, headers={"User-Agent": "basemind-python-wrapper"})
        context = ssl.create_default_context(cafile=certifi.where())
        try:
            with urlopen(request, timeout=30, context=context) as response:
                if response.status != 200:
                    raise RuntimeError(f"HTTP {response.status}: {response.reason}")
                destination.write_bytes(response.read())
        except URLError as exc:
            raise RuntimeError(f"Failed to download binary: {exc}") from exc

    _retry_with_backoff(download_attempt)


def _download_text(url: str) -> str:
    """Download text content with retry-with-backoff on transient errors."""

    def download_attempt():
        request = Request(url, headers={"User-Agent": "basemind-python-wrapper"})
        context = ssl.create_default_context(cafile=certifi.where())
        try:
            with urlopen(request, timeout=30, context=context) as response:
                if response.status != 200:
                    raise RuntimeError(f"HTTP {response.status}: {response.reason}")
                return response.read().decode("utf-8")
        except URLError as exc:
            raise RuntimeError(f"Failed to download checksums: {exc}") from exc

    return _retry_with_backoff(download_attempt)


def _expected_digest(checksums_text: str, asset_name: str) -> str | None:
    """Find the sha256 digest for asset_name in a `sha256<space>filename` file."""
    for line in checksums_text.splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        parts = stripped.split()
        if len(parts) < 2:
            continue
        name = parts[-1].lstrip("*")
        if name == asset_name:
            return parts[0].lower()
    return None


def _verify_checksum(archive: Path, asset_name: str, checksums_url: str) -> None:
    """Verify the archive sha256 against the release checksums file.

    Fails CLOSED: any failure to fetch the checksums, locate the entry, or
    match the digest raises, aborting the install rather than continuing with
    an unverified binary.
    """
    try:
        checksums_text = _download_text(checksums_url)
    except RuntimeError as exc:
        raise RuntimeError(
            f"could not fetch checksums ({checksums_url}): {exc} — refusing to install unverified binary"
        ) from exc

    expected = _expected_digest(checksums_text, asset_name)
    if not expected:
        raise RuntimeError(
            f"no checksum entry for {asset_name} in {checksums_url} — refusing to install unverified binary"
        )

    digest = hashlib.sha256()
    digest.update(archive.read_bytes())
    actual = digest.hexdigest().lower()
    if actual != expected:
        raise RuntimeError(f"checksum mismatch for {asset_name} (expected {expected}, got {actual})")


def _extract(archive: Path, ext: str, destination: Path) -> None:
    """Extract the full archive tree (binary + bundled lib/) into destination."""
    if ext == "zip":
        with zipfile.ZipFile(archive) as zf:
            zf.extractall(destination)
    else:
        with tarfile.open(archive, "r:gz") as tar:
            tar.extractall(destination)


def _binary_name() -> str:
    return "basemind.exe" if platform.system().lower() == "windows" else "basemind"


def _cache_dir(version: str) -> Path:
    """Directory holding the extracted binary plus its bundled lib/ tree."""
    cache_dir = Path.home() / ".cache" / "basemind" / version
    cache_dir.mkdir(parents=True, exist_ok=True)
    return cache_dir


def _prune_stale_versions(keep_version: str) -> None:
    """Remove old per-version cache dirs so binaries don't accrue across upgrades.

    Only ever runs once the ``keep_version`` binary is confirmed present, so a
    user's only working copy is never deleted before a replacement exists. Any dir
    whose name isn't ``keep_version`` is dead weight (this wrapper only runs its own
    version). Best-effort: a dir in use by another process is skipped on error.
    """
    root = Path.home() / ".cache" / "basemind"
    if not root.is_dir():
        return
    for entry in root.iterdir():
        if entry.name == keep_version or not entry.is_dir():
            continue
        if not entry.name[:1].isdigit():
            continue
        shutil.rmtree(entry, ignore_errors=True)


def ensure_binary():
    """Ensure the binary is available, downloading if necessary.

    Handles concurrent invocations via atomic rename: download+extract into a
    temp dir, then atomically move into the cache to prevent corruption from
    parallel installs.
    """
    from . import __version__

    override = os.getenv("BASEMIND_BINARY")
    if override:
        return override

    cache_dir = _cache_dir(__version__)
    binary_path = cache_dir / _binary_name()
    if binary_path.exists() and os.access(binary_path, os.X_OK):
        _prune_stale_versions(__version__)
        return str(binary_path)

    archive_url, ext, asset_name, checksums_url = _asset(__version__)
    print(f"Downloading basemind binary v{__version__}...", file=sys.stderr)

    lock_path = cache_dir / ".lock"
    cache_dir.mkdir(parents=True, exist_ok=True)

    lock_acquired = False
    try:
        lock_fd = os.open(str(lock_path), os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o644)
        lock_acquired = True
        os.close(lock_fd)
    except FileExistsError:
        for attempt in range(30):
            time.sleep(0.1)
            if binary_path.exists() and os.access(binary_path, os.X_OK):
                return str(binary_path)
        raise RuntimeError(
            f"Timeout waiting for concurrent binary installation of {__version__}. "
            f"If this persists, remove {cache_dir} and retry."
        )

    try:
        if binary_path.exists() and os.access(binary_path, os.X_OK):
            return str(binary_path)

        # Stage under the cache root, not the system temp dir: the final ~keep
        # ``staging_dir.replace(cache_dir)`` is an atomic rename that fails with ~keep
        # EXDEV (cross-device link) when /tmp and $HOME are on different filesystems (#38). ~keep
        with tempfile.TemporaryDirectory(dir=cache_dir.parent) as tmpdir:
            archive_path = Path(tmpdir) / asset_name
            _download(archive_url, archive_path)
            _verify_checksum(archive_path, asset_name, checksums_url)

            staging_dir = Path(tmpdir) / "staging"
            staging_dir.mkdir()
            _extract(archive_path, ext, staging_dir)

            try:
                staging_dir.replace(cache_dir)
            except (OSError, FileExistsError):
                if not (binary_path.exists() and os.access(binary_path, os.X_OK)):
                    shutil.rmtree(cache_dir, ignore_errors=True)
                    staging_dir.replace(cache_dir)
                if not binary_path.exists():
                    raise RuntimeError(f"binary {_binary_name()} not found after extracting {asset_name}")

        if not binary_path.exists():
            raise RuntimeError(f"binary {_binary_name()} not found after extracting {asset_name}")

        if platform.system().lower() != "windows":
            binary_path.chmod(0o755)

        _prune_stale_versions(__version__)

        print("Binary downloaded successfully!", file=sys.stderr)
        return str(binary_path)
    finally:
        if lock_acquired:
            try:
                lock_path.unlink()
            except FileNotFoundError:
                pass


def run_basemind(args):
    """Run the basemind binary with the given arguments."""
    binary_path = ensure_binary()

    try:
        result = subprocess.run([binary_path] + args, check=False)
        sys.exit(result.returncode)
    except FileNotFoundError:
        raise RuntimeError(f"Binary not found at {binary_path}")
    except Exception as e:
        raise RuntimeError(f"Failed to run basemind: {e}")
