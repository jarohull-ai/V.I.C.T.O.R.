#!/usr/bin/env python3
"""No-system test for the user-local installer."""

from __future__ import annotations

import os
import stat
import subprocess
import sys
import tempfile
from pathlib import Path

SCRIPT = Path(__file__).with_name("install.py")
HASH_SCRIPT = Path(__file__).with_name("sha256_manifest.py")


def run(*args: str, env: dict[str, str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(SCRIPT), *args],
        check=True,
        text=True,
        capture_output=True,
        env=env,
    )


def run_fail(*args: str, env: dict[str, str]) -> None:
    result = subprocess.run(
        [sys.executable, str(SCRIPT), *args],
        check=False,
        text=True,
        capture_output=True,
        env=env,
    )
    assert result.returncode != 0


with tempfile.TemporaryDirectory() as temporary:
    root = Path(temporary)
    release = root / "release"
    release.mkdir(mode=0o700)
    (release / "victor-daemon").write_bytes(b"fixture-daemon")
    (release / "victor-skill").write_bytes(b"fixture-cli")
    os.chmod(release / "victor-daemon", 0o700)
    os.chmod(release / "victor-skill", 0o700)
    env = os.environ.copy()
    env.update(
        {
            "HOME": str(root / "home"),
            "XDG_CONFIG_HOME": str(root / "config-home"),
            "XDG_STATE_HOME": str(root / "state-home"),
            "XDG_DATA_HOME": str(root / "data-home"),
            "XDG_RUNTIME_DIR": str(root / "runtime-parent"),
        }
    )
    for base in ("XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_DATA_HOME"):
        Path(env[base]).mkdir(mode=0o755)
    Path(env["XDG_RUNTIME_DIR"]).mkdir(mode=0o700)
    result = run("install", "--release-dir", str(release), "--systemd-user", env=env)
    assert "trusted definitions are placeholders" in result.stdout
    config = Path(env["XDG_CONFIG_HOME"]) / "victor"
    state = Path(env["XDG_STATE_HOME"]) / "victor"
    runtime = Path(env["XDG_RUNTIME_DIR"]) / "victor"
    data = Path(env["XDG_DATA_HOME"]) / "victor"
    for directory in (config, state, runtime, data, config / "trusted", data / "bin"):
        assert stat.S_IMODE(directory.stat().st_mode) == 0o700
        assert directory.stat().st_uid == os.geteuid()
    token = config / "token"
    assert stat.S_IMODE(token.stat().st_mode) == 0o600
    assert len(token.read_text().strip()) >= 16
    assert "REPLACE_AFTER_REVIEW" in (config / "trusted/local-model.json").read_text()
    assert "token" not in (config / "trusted/local-model.json").read_text().lower()
    unit = Path(env["XDG_CONFIG_HOME"]) / "systemd/user/victor.service"
    assert unit.is_file() and stat.S_IMODE(unit.stat().st_mode) == 0o600
    run("install", "--release-dir", str(release), "--systemd-user", env=env)
    run("uninstall", "--systemd-user", env=env)
    assert not (data / "bin/victor-daemon").exists()
    assert token.exists()
    unsafe = root / "unsafe"
    unsafe.mkdir(mode=0o755)
    run_fail(
        "install",
        "--release-dir",
        str(release),
        "--config-dir",
        str(unsafe),
        env=env,
    )
    linked_release = root / "linked-release"
    linked_release.symlink_to(release, target_is_directory=True)
    run_fail("install", "--release-dir", str(linked_release), env=env)

    first = root / "artifact-a"
    second = root / "artifact-b"
    first.write_bytes(b"a")
    second.write_bytes(b"b")
    manifest = root / "SHA256SUMS"
    result = subprocess.run(
        [sys.executable, str(HASH_SCRIPT), "--output", str(manifest), str(second), str(first)],
        check=True,
        text=True,
        capture_output=True,
        env=env,
    )
    assert "2 artifacts" in result.stdout
    rows = manifest.read_text().splitlines()
    assert rows == sorted(rows, key=lambda row: row.rsplit("  ", 1)[1])
    assert rows[0].startswith("ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb  ")
    linked_artifact = root / "linked-artifact"
    linked_artifact.symlink_to(first)
    run_fail_hash = subprocess.run(
        [sys.executable, str(HASH_SCRIPT), "--output", str(root / "bad"), str(linked_artifact)],
        check=False,
        text=True,
        capture_output=True,
        env=env,
    )
    assert run_fail_hash.returncode != 0
    print("local installer tests: ok")
