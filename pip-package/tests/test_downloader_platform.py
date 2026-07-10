"""Tests for basemind/downloader.py platform resolution + cache pruning.

Cover the two behaviours that broke real installs:

* Apple Silicon under a Rosetta x86_64 shell must resolve to the NATIVE arm64
  binary — ``hw.optional.arm64`` is masked under Rosetta, so ``sysctl.proc_translated``
  is the signal that catches it. Genuine Intel Macs resolve to ``x86_64-apple-darwin``.
* Old per-version cache dirs must be pruned so binaries don't accrue across upgrades.
"""

from __future__ import annotations

from basemind import downloader


def _triple(monkeypatch, *, system: str, machine: str, sysctls: dict[str, str]) -> str:
    monkeypatch.setattr(downloader.platform, "system", lambda: system)
    monkeypatch.setattr(downloader.platform, "machine", lambda: machine)
    monkeypatch.setattr(downloader, "_sysctl", lambda name: sysctls.get(name, ""))
    return downloader._platform_triple()


def test_native_arm64_darwin_resolves_arm64(monkeypatch):
    triple = _triple(monkeypatch, system="Darwin", machine="arm64", sysctls={})
    assert triple == "aarch64-apple-darwin"


def test_rosetta_shell_on_apple_silicon_resolves_native_arm64(monkeypatch):
    triple = _triple(
        monkeypatch,
        system="Darwin",
        machine="x86_64",
        sysctls={"sysctl.proc_translated": "1"},
    )
    assert triple == "aarch64-apple-darwin"


def test_apple_silicon_native_signal_resolves_arm64(monkeypatch):
    triple = _triple(
        monkeypatch,
        system="Darwin",
        machine="x86_64",
        sysctls={"hw.optional.arm64": "1"},
    )
    assert triple == "aarch64-apple-darwin"


def test_genuine_intel_mac_resolves_x86_64(monkeypatch):
    triple = _triple(monkeypatch, system="Darwin", machine="x86_64", sysctls={})
    assert triple == "x86_64-apple-darwin"


def test_linux_and_windows_unaffected(monkeypatch):
    assert _triple(monkeypatch, system="Linux", machine="x86_64", sysctls={}) == "x86_64-unknown-linux-gnu"
    assert _triple(monkeypatch, system="Linux", machine="aarch64", sysctls={}) == "aarch64-unknown-linux-gnu"
    assert _triple(monkeypatch, system="Windows", machine="AMD64", sysctls={}) == "x86_64-pc-windows-msvc"


def test_prune_removes_old_versions_keeps_current(monkeypatch, tmp_path):
    cache = tmp_path / ".cache" / "basemind"
    for name in ("0.9.0", "0.14.0", "0.19.2"):
        (cache / name).mkdir(parents=True)
        (cache / name / "basemind").write_bytes(b"x")
    (cache / ".lock").mkdir()
    monkeypatch.setattr(downloader.Path, "home", classmethod(lambda cls: tmp_path))

    downloader._prune_stale_versions("0.19.2")

    remaining = sorted(p.name for p in cache.iterdir())
    assert remaining == [".lock", "0.19.2"]


def test_prune_is_noop_when_cache_absent(monkeypatch, tmp_path):
    monkeypatch.setattr(downloader.Path, "home", classmethod(lambda cls: tmp_path))
    downloader._prune_stale_versions("0.19.2")
