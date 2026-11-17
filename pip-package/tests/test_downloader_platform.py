"""Tests for basemind/downloader.py platform resolution + cache pruning.

Cover the two behaviours that broke real installs:

* Apple Silicon under a Rosetta x86_64 shell must resolve to the NATIVE arm64
  binary — ``hw.optional.arm64`` is masked under Rosetta, so ``sysctl.proc_translated``
  is the signal that catches it. Genuine Intel Macs are rejected: no binary is published for them.
* Old per-version cache dirs must be pruned so binaries don't accrue across upgrades.
"""

from __future__ import annotations

import pytest
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


def test_genuine_intel_mac_is_rejected_with_a_clear_message(monkeypatch):
    with pytest.raises(RuntimeError, match="Intel"):
        _triple(monkeypatch, system="Darwin", machine="x86_64", sysctls={})


def test_linux_and_windows_unaffected(monkeypatch):
    assert _triple(monkeypatch, system="Linux", machine="x86_64", sysctls={}) == "x86_64-unknown-linux-gnu"
    assert _triple(monkeypatch, system="Linux", machine="aarch64", sysctls={}) == "aarch64-unknown-linux-gnu"
    assert _triple(monkeypatch, system="Windows", machine="AMD64", sysctls={}) == "x86_64-pc-windows-msvc"


def _age(path, days):
    import os
    import time

    then = time.time() - days * 86400
    os.utime(path, (then, then))


def test_prune_removes_only_versions_unused_for_the_window(monkeypatch, tmp_path):
    cache = tmp_path / ".cache" / "basemind"
    for name in ("0.9.0", "0.14.0", "0.18.0", "0.19.2"):
        (cache / name).mkdir(parents=True)
        (cache / name / "basemind").write_bytes(b"x")
    _age(cache / "0.9.0", 30)
    # Another environment still runs 0.14.0: its marker is fresh although the directory is old.
    _age(cache / "0.14.0", 30)
    (cache / "0.14.0" / ".last-used").touch()
    # 0.18.0 is old but its install is in progress.
    _age(cache / "0.18.0", 30)
    (cache / ".lock-0.18.0").touch()
    monkeypatch.setattr(downloader.Path, "home", classmethod(lambda cls: tmp_path))

    downloader._prune_stale_versions("0.19.2")

    remaining = sorted(p.name for p in cache.iterdir())
    assert remaining == [".lock-0.18.0", "0.14.0", "0.18.0", "0.19.2"]


def _fake_release(monkeypatch, tmp_path, version="0.19.2"):
    """ensure_binary with the network replaced: the "download" writes a binary to extract."""
    monkeypatch.setattr(downloader.Path, "home", classmethod(lambda cls: tmp_path))
    monkeypatch.delenv("BASEMIND_BINARY", raising=False)
    import basemind

    monkeypatch.setattr(basemind, "__version__", version)
    monkeypatch.setattr(downloader, "_asset", lambda v: ("url", "tar.gz", "asset.tar.gz", "sums"))
    monkeypatch.setattr(downloader, "_download", lambda url, path: path.write_bytes(b"archive"))
    monkeypatch.setattr(downloader, "_verify_checksum", lambda *a: None)

    def extract(archive, ext, destination):
        (destination / downloader._binary_name()).write_text("#!/bin/sh\n")

    monkeypatch.setattr(downloader, "_extract", extract)
    return tmp_path / ".cache" / "basemind" / version


def test_an_incomplete_cache_dir_is_replaced_and_the_lock_released(monkeypatch, tmp_path):
    cache_dir = _fake_release(monkeypatch, tmp_path)
    cache_dir.mkdir(parents=True)
    (cache_dir / "leftover").write_text("from a crashed install")

    binary = downloader.ensure_binary()

    assert binary == str(cache_dir / downloader._binary_name())
    assert sorted(p.name for p in cache_dir.iterdir()) == [".last-used", downloader._binary_name()]
    assert not (cache_dir.parent / ".lock-0.19.2").exists()


def test_a_lock_a_crashed_install_left_is_broken(monkeypatch, tmp_path):
    cache_dir = _fake_release(monkeypatch, tmp_path)
    lock = cache_dir.parent / ".lock-0.19.2"
    lock.parent.mkdir(parents=True)
    lock.touch()
    _age(lock, 1)

    assert downloader.ensure_binary() == str(cache_dir / downloader._binary_name())


def test_a_waiter_outlasts_a_slow_install(monkeypatch, tmp_path):
    cache_dir = _fake_release(monkeypatch, tmp_path)
    lock = cache_dir.parent / ".lock-0.19.2"
    lock.parent.mkdir(parents=True)
    lock.touch()
    sleeps = []

    def sleep(seconds):
        sleeps.append(seconds)
        if len(sleeps) == 50:  # ten seconds in: the other process finishes its install
            cache_dir.mkdir()
            binary = cache_dir / downloader._binary_name()
            binary.write_text("#!/bin/sh\n")
            binary.chmod(0o755)

    monkeypatch.setattr(downloader.time, "sleep", sleep)

    assert downloader.ensure_binary() == str(cache_dir / downloader._binary_name())
    assert len(sleeps) == 50


def test_prune_is_noop_when_cache_absent(monkeypatch, tmp_path):
    monkeypatch.setattr(downloader.Path, "home", classmethod(lambda cls: tmp_path))
    downloader._prune_stale_versions("0.19.2")


def test_staging_dir_is_created_on_the_cache_filesystem(monkeypatch, tmp_path):
    """#38: staging must sit under the cache root so the final atomic rename is not a
    cross-device (EXDEV) link when the system temp dir and $HOME are on different filesystems."""
    monkeypatch.setattr(downloader.Path, "home", classmethod(lambda cls: tmp_path))
    monkeypatch.delenv("BASEMIND_BINARY", raising=False)

    captured: dict[str, object] = {}

    class _Sentinel(Exception):
        pass

    def fake_tempdir(*args, **kwargs):
        captured["dir"] = kwargs.get("dir")
        raise _Sentinel

    monkeypatch.setattr(downloader.tempfile, "TemporaryDirectory", fake_tempdir)
    monkeypatch.setattr(downloader, "_download", lambda *a, **k: None)

    with pytest.raises(_Sentinel):
        downloader.ensure_binary()

    assert captured["dir"] == tmp_path / ".cache" / "basemind"
