#!/usr/bin/env python3
"""Bounded tests for the read-only trusted-definition pre-flight tool."""

from __future__ import annotations

import hashlib
import json
import os
import stat
import subprocess
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "scripts/local/validate_trusted.py"


def digest(path: Path) -> str:
    return "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest()


def run(*args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["python3", str(TOOL), *args],
        cwd=ROOT,
        text=True,
        capture_output=True,
        check=False,
    )


def main() -> None:
    with tempfile.TemporaryDirectory() as raw:
        base = Path(raw)
        trusted = base / "trusted"
        trusted.mkdir(mode=0o700)
        work = base / "work"
        work.mkdir(mode=0o700)
        executable = base / "fixture"
        executable.write_bytes(b"fixture")
        executable.chmod(0o700)
        model = base / "model.bin"
        model.write_bytes(b"model")
        model.chmod(0o600)
        # A fixture sandbox must be owned by the test user; the system bwrap
        # binary is intentionally not trusted by this synthetic test.
        sandbox = executable

        spec = {
            "command": [str(executable)],
            "working_dir": str(work),
            "environment": {"LANG": "C"},
            "timeout_ms": 1000,
            "max_frame_bytes": 4096,
            "max_stderr_bytes": 1024,
            "max_restart_attempts": 0,
            "sandbox_required": False,
            "sandbox_executable": None,
            "executable_sha256": digest(executable),
        }
        spec_hash = "sha256:" + hashlib.sha256(
            json.dumps(spec, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        ).hexdigest()
        (trusted / "local-model.json").write_text(
            json.dumps({"config_sha256": spec_hash, "spec": spec}), encoding="utf-8"
        )
        (trusted / "local-model.json").chmod(0o600)
        result = run("validate", "--directory", str(trusted))
        assert result.returncode == 0, result.stdout + result.stderr
        report = json.loads(result.stdout)
        assert report["definitions"]
        assert report["definitions"][0]["environment_keys"] == ["LANG"]
        assert '"environment":' not in result.stdout

        # A changed definition hash must be rejected before daemon startup.
        document = json.loads((trusted / "local-model.json").read_text(encoding="utf-8"))
        document["config_sha256"] = "sha256:" + "0" * 64
        (trusted / "local-model.json").write_text(json.dumps(document), encoding="utf-8")
        (trusted / "local-model.json").chmod(0o600)
        changed = run("validate", "--directory", str(trusted))
        assert changed.returncode == 1 and "config_sha256" in changed.stdout

        missing = run("validate", "--directory", str(base / "missing"))
        assert missing.returncode == 2 and "NotConfigured" in missing.stdout

        hashed = run("hash", str(executable), str(model))
        assert hashed.returncode == 0
        assert digest(executable) in hashed.stdout and digest(model) in hashed.stdout

        # The validator must reject a symlink and must not replace it.
        link = trusted / "local-stt.json"
        link.symlink_to(executable)
        result = run("validate", "--directory", str(trusted))
        assert result.returncode == 1 and "symlink" in result.stdout
        assert link.is_symlink()

        # Do not accidentally weaken the file mode in the test fixture.
        assert stat.S_IMODE(executable.stat().st_mode) == 0o700
        assert os.getuid() == executable.stat().st_uid

    print("trusted definition checks: OK")


if __name__ == "__main__":
    main()
