#!/usr/bin/env python3
"""Install or uninstall a user-local V.I.C.T.O.R. release."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import secrets
import shutil
import stat
import sys
from pathlib import Path

APP = "victor"
BINARIES = ("victor-daemon", "victor-skill")


def fail(message: str) -> None:
    raise SystemExit(f"install failed: {message}")


def current_uid() -> int:
    return os.geteuid()


def reject_symlink(path: Path) -> None:
    try:
        if path.is_symlink():
            fail(f"symlink is not allowed: {path}")
    except OSError as error:
        fail(f"cannot inspect {path}: {error}")


def validate_existing_directory(path: Path) -> None:
    reject_symlink(path)
    try:
        info = path.lstat()
    except FileNotFoundError:
        fail(f"directory does not exist: {path}")
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != current_uid():
        fail(f"directory is not owned by the current user: {path}")
    if stat.S_IMODE(info.st_mode) & 0o077:
        fail(f"directory is accessible by group/other: {path}")


def validate_parent_directory(path: Path) -> None:
    reject_symlink(path)
    try:
        info = path.lstat()
    except FileNotFoundError:
        fail(f"directory does not exist: {path}")
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != current_uid():
        fail(f"directory is not owned by the current user: {path}")
    if stat.S_IMODE(info.st_mode) & 0o022:
        fail(f"directory is writable by group/other: {path}")


def ensure_parent_directory(path: Path) -> None:
    if not path.is_absolute():
        fail(f"directory must be absolute: {path}")
    if path.exists() or path.is_symlink():
        validate_parent_directory(path)
        return
    ensure_parent_directory(path.parent)
    path.mkdir()
    os.chmod(path, 0o700)


def ensure_directory(path: Path) -> None:
    if not path.is_absolute():
        fail(f"directory must be absolute: {path}")
    if path.exists() or path.is_symlink():
        validate_existing_directory(path)
        return
    ensure_parent_directory(path.parent)
    path.mkdir()
    os.chmod(path, 0o700)
    validate_existing_directory(path)


def safe_file(path: Path) -> None:
    reject_symlink(path)
    try:
        info = path.lstat()
    except FileNotFoundError:
        return
    if not stat.S_ISREG(info.st_mode) or info.st_uid != current_uid():
        fail(f"existing path is not a private regular file: {path}")
    if stat.S_IMODE(info.st_mode) & 0o077:
        fail(f"file is accessible by group/other: {path}")


def write_private(path: Path, content: bytes, mode: int = 0o600) -> None:
    safe_file(path)
    flags = os.O_WRONLY | os.O_CREAT | os.O_TRUNC
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        fd = os.open(path, flags, mode)
        with os.fdopen(fd, "wb") as handle:
            handle.write(content)
        os.chmod(path, mode)
    except OSError as error:
        fail(f"cannot write {path}: {error}")


def xdg_dir(name: str, fallback: Path) -> Path:
    value = os.environ.get(name)
    return Path(value) if value else fallback


def paths(args: argparse.Namespace) -> dict[str, Path]:
    home = Path.home()
    config_home = xdg_dir("XDG_CONFIG_HOME", home / ".config")
    state_home = xdg_dir("XDG_STATE_HOME", home / ".local" / "state")
    data_home = xdg_dir("XDG_DATA_HOME", home / ".local" / "share")
    runtime_value = os.environ.get("XDG_RUNTIME_DIR")
    if not runtime_value:
        fail("XDG_RUNTIME_DIR must be set; refusing a shared /tmp fallback")
    return {
        "config": Path(args.config_dir) if args.config_dir else config_home / APP,
        "state": Path(args.state_dir) if args.state_dir else state_home / APP,
        "runtime": Path(args.runtime_dir) if args.runtime_dir else Path(runtime_value) / APP,
        "data": Path(args.data_dir) if args.data_dir else data_home / APP,
        "systemd": config_home / "systemd" / "user" / "victor.service",
    }


def example_files() -> dict[str, str]:
    return {
        "local-model.json": json.dumps(
            {
                "config_sha256": "sha256:REPLACE_AFTER_REVIEW",
                "spec": {
                    "command": ["/absolute/path/to/local-model"],
                    "working_dir": "/absolute/path/to/private-working-dir",
                    "environment": {},
                    "timeout_ms": 30000,
                    "max_frame_bytes": 1048576,
                    "max_stderr_bytes": 65536,
                    "max_restart_attempts": 1,
                    "sandbox_required": True,
                    "sandbox_executable": "/usr/bin/bwrap",
                    "executable_sha256": "sha256:REPLACE_AFTER_REVIEW",
                },
            },
            indent=2,
        )
        + "\n",
        "local-stt.json": json.dumps(
            {
                "config_sha256": "sha256:REPLACE_AFTER_REVIEW",
                "spec": {
                    "executable": "/absolute/path/to/whisper-cli",
                    "model": "/absolute/path/to/whisper-model.bin",
                    "language": "pl",
                    "threads": 2,
                    "timeout_ms": 30000,
                    "max_wav_bytes": 16777216,
                    "working_dir": "/absolute/path/to/private-working-dir",
                    "environment": {},
                    "sandbox_required": True,
                    "sandbox_executable": "/usr/bin/bwrap",
                    "executable_sha256": "sha256:REPLACE_AFTER_REVIEW",
                    "model_sha256": "sha256:REPLACE_AFTER_REVIEW",
                },
            },
            indent=2,
        )
        + "\n",
        "local-tts.json": json.dumps(
            {
                "config_sha256": "sha256:REPLACE_AFTER_REVIEW",
                "spec": {
                    "command": ["/absolute/path/to/local-tts"],
                    "voice": "REPLACE_AFTER_REVIEW",
                    "model": "/absolute/path/to/tts-model.bin",
                    "language": "pl",
                    "output_format": {"sample_rate": 22050, "channels": 1, "encoding": "pcm_s16le"},
                    "min_speed": 0.5,
                    "max_speed": 2.0,
                    "working_dir": "/absolute/path/to/private-working-dir",
                    "environment": {},
                    "timeout_ms": 30000,
                    "max_frame_bytes": 1048576,
                    "max_audio_bytes": 16777216,
                    "max_stderr_bytes": 65536,
                    "max_restart_attempts": 1,
                    "sandbox_required": True,
                    "sandbox_executable": "/usr/bin/bwrap",
                    "executable_sha256": "sha256:REPLACE_AFTER_REVIEW",
                    "model_sha256": "sha256:REPLACE_AFTER_REVIEW",
                },
            },
            indent=2,
        )
        + "\n",
        "plugin_sysinfo.json": json.dumps(
            {
                "id": "plugin_sysinfo",
                "manifest": "REPLACE_WITH_REVIEWED_TOOL_MANIFEST",
                "launch": {
                    "command": ["/absolute/path/to/plugin_sysinfo"],
                    "working_dir": "/absolute/path/to/private-working-dir",
                    "environment": {},
                    "sandbox": "required",
                    "sandbox_executable": "/usr/bin/bwrap",
                    "request_timeout_ms": 1000,
                    "max_frame_bytes": 65536,
                    "max_restart_attempts": 1,
                },
            },
            indent=2,
        )
        + "\n",
    }


def render_unit(paths_map: dict[str, Path], binary: Path) -> str:
    config = paths_map["config"]
    state = paths_map["state"]
    runtime = paths_map["runtime"]
    definitions = config / "trusted"
    return f"""[Unit]
Description=V.I.C.T.O.R. local daemon
After=default.target

[Service]
Type=simple
ExecStart={binary} --sqlite {state / 'state.sqlite'} --socket {runtime / 'victor.sock'} --runtime-dir {runtime} --trusted-definitions {definitions} --token-file {config / 'token'} --allowed-root {config} --local-model-definition {definitions / 'local-model.json'} --local-stt-definition {definitions / 'local-stt.json'} --local-tts-definition {definitions / 'local-tts.json'}
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths={state} {runtime}
Restart=on-failure
RestartSec=2s

[Install]
WantedBy=default.target
"""


def config_token(path: Path) -> str:
    if path.exists() or path.is_symlink():
        safe_file(path)
        value = path.read_text(encoding="utf-8").strip()
        if len(value) < 16:
            fail("existing token is shorter than 16 characters")
        return value
    value = secrets.token_urlsafe(32)
    write_private(path, (value + "\n").encode())
    return value


def install(args: argparse.Namespace) -> None:
    selected = paths(args)
    for key in ("config", "state", "runtime", "data"):
        ensure_directory(selected[key])
    trusted = selected["config"] / "trusted"
    ensure_directory(trusted)
    bin_dir = selected["data"] / "bin"
    ensure_directory(bin_dir)

    release_dir = Path(args.release_dir)
    reject_symlink(release_dir)
    release_dir = release_dir.resolve()
    validate_existing_directory(release_dir)
    installed: list[str] = []
    for name in BINARIES:
        source = release_dir / name
        reject_symlink(source)
        if not source.is_file():
            continue
        destination = bin_dir / name
        safe_file(destination)
        shutil.copyfile(source, destination)
        os.chmod(destination, 0o700)
        installed.append(name)
    if "victor-daemon" not in installed:
        fail(f"release directory does not contain {release_dir / 'victor-daemon'}")

    for filename, content in example_files().items():
        destination = trusted / filename
        if not destination.exists():
            write_private(destination, content.encode())

    token = config_token(selected["config"] / "token")
    write_private(selected["config"] / "daemon.env", b"# Reserved for non-secret daemon settings.\n")
    if args.systemd_user:
        unit = selected["systemd"]
        ensure_directory(unit.parent)
        write_private(unit, render_unit(selected, bin_dir / "victor-daemon").encode())

    print(f"installed {', '.join(installed)}")
    print(f"config={selected['config']}")
    print(f"state={selected['state']}")
    print(f"runtime={selected['runtime']}")
    print(f"token_sha256={hashlib.sha256(token.encode()).hexdigest()}")
    print("trusted definitions are placeholders and must be reviewed before starting")


def uninstall(args: argparse.Namespace) -> None:
    selected = paths(args)
    bin_dir = selected["data"] / "bin"
    if bin_dir.exists() or bin_dir.is_symlink():
        validate_existing_directory(bin_dir)
        for name in BINARIES:
            path = bin_dir / name
            if path.exists() or path.is_symlink():
                safe_file(path)
                path.unlink()
    if args.systemd_user and (selected["systemd"].exists() or selected["systemd"].is_symlink()):
        safe_file(selected["systemd"])
        selected["systemd"].unlink()
    print("removed installed binaries; configuration, token and state were preserved")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("install", "uninstall"))
    parser.add_argument("--release-dir", default="target/release", type=Path)
    parser.add_argument("--config-dir", type=Path)
    parser.add_argument("--state-dir", type=Path)
    parser.add_argument("--runtime-dir", type=Path)
    parser.add_argument("--data-dir", type=Path)
    parser.add_argument("--systemd-user", action="store_true")
    args = parser.parse_args()
    (install if args.command == "install" else uninstall)(args)


if __name__ == "__main__":
    main()
