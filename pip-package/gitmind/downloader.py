from __future__ import annotations

import os
import platform
import ssl
import subprocess
import sys
import tempfile
import tarfile
import zipfile
from pathlib import Path
from urllib.error import URLError
from urllib.request import Request, urlopen

import certifi


def _platform_triple() -> str:
    system = platform.system().lower()
    machine = platform.machine().lower()

    if system == "windows":
        if machine in {"amd64", "x86_64"}:
            return "x86_64-pc-windows-gnu"
        if machine in {"x86", "i386", "i686"}:
            raise RuntimeError("32-bit Windows is not supported")
    elif system == "linux":
        if machine in {"amd64", "x86_64"}:
            return "x86_64-unknown-linux-gnu"
        if machine in {"aarch64", "arm64"}:
            return "aarch64-unknown-linux-gnu"
    elif system == "darwin":
        if machine in {"amd64", "x86_64"}:
            return "x86_64-apple-darwin"
        if machine in {"aarch64", "arm64"}:
            return "aarch64-apple-darwin"

    raise RuntimeError(f"Unsupported platform: {system} {machine}")


def _python_version_to_tag(version: str) -> str:
    if "rc" in version:
        core, suffix = version.split("rc")
        return f"{core}-rc.{suffix}"
    return version


def _asset(version: str) -> tuple[str, str]:
    tag = _python_version_to_tag(version)
    triple = _platform_triple()
    ext = "zip" if "windows" in triple else "tar.gz"
    url = (
        f"https://github.com/Goldziher/gitmind/releases/download/"
        f"v{tag}/gitmind-{triple}.{ext}"
    )
    return url, ext


def _download(url: str, destination: Path) -> None:
    request = Request(url, headers={"User-Agent": "gitmind-python-wrapper"})
    context = ssl.create_default_context(cafile=certifi.where())
    try:
        with urlopen(request, timeout=30, context=context) as response:
            if response.status != 200:
                raise RuntimeError(f"HTTP {response.status}: {response.reason}")
            destination.write_bytes(response.read())
    except URLError as exc:
        raise RuntimeError(f"Failed to download binary: {exc}") from exc


def _extract(archive: Path, ext: str, destination: Path) -> None:
    if ext == "zip":
        with zipfile.ZipFile(archive) as zf:
            for name in zf.namelist():
                if name.endswith("gitmind") or name.endswith("gitmind.exe"):
                    with zf.open(name) as src, destination.open("wb") as dst:
                        dst.write(src.read())
                    return
    else:
        with tarfile.open(archive, "r:gz") as tar:
            for member in tar.getmembers():
                if member.name.endswith("gitmind") or member.name.endswith("gitmind.exe"):
                    with tar.extractfile(member) as src, destination.open("wb") as dst:
                        dst.write(src.read())
                    return
    raise RuntimeError("Binary not found in downloaded archive")


def _cache_path(version: str) -> Path:
    cache_dir = Path.home() / ".cache" / "gitmind" / version
    cache_dir.mkdir(parents=True, exist_ok=True)
    suffix = ".exe" if platform.system().lower() == "windows" else ""
    return cache_dir / f"gitmind{suffix}"


def ensure_binary():
    """Ensure the binary is available, downloading if necessary."""
    from . import __version__

    override = os.getenv("GITMIND_BINARY")
    if override:
        return override

    binary_path = _cache_path(__version__)
    if binary_path.exists() and os.access(binary_path, os.X_OK):
        return str(binary_path)

    url, ext = _asset(__version__)
    print(f"Downloading gitmind binary v{__version__}...", file=sys.stderr)

    with tempfile.TemporaryDirectory() as tmpdir:
        archive_path = Path(tmpdir) / f"gitmind.{ext}"
        _download(url, archive_path)
        _extract(archive_path, ext, binary_path)

    if platform.system().lower() != "windows":
        binary_path.chmod(0o755)

    print("Binary downloaded successfully!", file=sys.stderr)
    return str(binary_path)


def run_gitmind(args):
    """Run the gitmind binary with the given arguments."""
    binary_path = ensure_binary()

    try:
        result = subprocess.run([binary_path] + args, check=False)
        sys.exit(result.returncode)
    except FileNotFoundError:
        raise RuntimeError(f"Binary not found at {binary_path}")
    except Exception as e:
        raise RuntimeError(f"Failed to run gitmind: {e}")
