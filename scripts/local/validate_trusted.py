#!/usr/bin/env python3
"""Read-only validation and hashing for V.I.C.T.O.R. trusted definitions.

This tool deliberately never edits a definition, starts a process, or prints
environment values.  It is an operator pre-flight check; the daemon remains
the final authority and validates the same files again at startup.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import stat
import sys
from pathlib import Path
from typing import Any

SECRET_MARKERS = ("secret", "token", "password", "api_key", "private_key")
DEFINITION_NAMES = ("local-model.json", "local-stt.json", "local-tts.json")


class ValidationError(Exception):
    pass


def sha256(path: Path) -> str:
    check_regular(path, "file")
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return f"sha256:{digest.hexdigest()}"


def spec_sha256(spec: dict[str, Any]) -> str:
    encoded = json.dumps(spec, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    return "sha256:" + hashlib.sha256(encoded).hexdigest()


def check_regular(path: Path, label: str) -> os.stat_result:
    try:
        info = path.lstat()
    except FileNotFoundError as error:
        raise ValidationError(f"{label} does not exist: {path}") from error
    if stat.S_ISLNK(info.st_mode):
        raise ValidationError(f"{label} is a symlink: {path}")
    if not stat.S_ISREG(info.st_mode):
        raise ValidationError(f"{label} is not a regular file: {path}")
    if info.st_uid != os.getuid() or info.st_mode & 0o022:
        raise ValidationError(f"{label} has unsafe owner or permissions: {path}")
    return info


def check_directory(path: Path, label: str) -> None:
    try:
        info = path.lstat()
    except FileNotFoundError as error:
        raise ValidationError(f"{label} does not exist: {path}") from error
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
        raise ValidationError(f"{label} is not a real directory: {path}")
    if info.st_uid != os.getuid() or info.st_mode & 0o022:
        raise ValidationError(f"{label} has unsafe owner or permissions: {path}")


def absolute_path(value: Any, field: str) -> Path:
    if not isinstance(value, str) or not value or not Path(value).is_absolute():
        raise ValidationError(f"{field} must be an absolute path")
    return Path(value)


def check_environment(value: Any) -> list[str]:
    if value is None:
        return []
    if not isinstance(value, dict):
        raise ValidationError("environment must be an object")
    keys: list[str] = []
    for key in value:
        if not isinstance(key, str) or not key or any(marker in key.lower() for marker in SECRET_MARKERS):
            raise ValidationError("environment contains a secret-like key")
        keys.append(key)
    return sorted(keys)


def checked_executable(value: Any, field: str) -> str:
    path = absolute_path(value, field)
    info = check_regular(path, field)
    if not info.st_mode & 0o111:
        raise ValidationError(f"{field} is not executable: {path}")
    return sha256(path)


def checked_model(value: Any, field: str) -> str:
    path = absolute_path(value, field)
    check_regular(path, field)
    return sha256(path)


def validate_definition(path: Path) -> dict[str, Any]:
    check_regular(path, "definition")
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ValidationError(f"invalid JSON: {path}") from error
    if not isinstance(document, dict) or not isinstance(document.get("spec"), dict):
        raise ValidationError("definition must contain an object named spec")

    spec = document["spec"]
    env_keys = check_environment(spec.get("environment", {}))
    result: dict[str, Any] = {"definition": path.name, "environment_keys": env_keys}
    if path.name == "local-stt.json":
        result["executable_sha256"] = checked_executable(spec.get("executable"), "executable")
        result["model_sha256"] = checked_model(spec.get("model"), "model")
        if not isinstance(spec.get("timeout_ms"), int) or spec["timeout_ms"] <= 0:
            raise ValidationError("timeout_ms must be positive")
    else:
        command = spec.get("command")
        if not isinstance(command, list) or not command:
            raise ValidationError("command must be a non-empty array")
        result["executable_sha256"] = checked_executable(command[0], "command[0]")
        if path.name == "local-tts.json":
            result["model_sha256"] = checked_model(spec.get("model"), "model")
        if not isinstance(spec.get("timeout_ms"), int) or spec["timeout_ms"] <= 0:
            raise ValidationError("timeout_ms must be positive")

    check_directory(absolute_path(spec.get("working_dir"), "working_dir"), "working_dir")
    if spec.get("sandbox_required"):
        sandbox = absolute_path(spec.get("sandbox_executable"), "sandbox_executable")
        checked_executable(sandbox, "sandbox_executable")

    config_hash = document.get("config_sha256")
    if not isinstance(config_hash, str) or not config_hash.startswith("sha256:"):
        raise ValidationError("config_sha256 is missing or malformed")
    if config_hash != spec_sha256(spec):
        raise ValidationError("config_sha256 does not match spec")
    result["config_sha256"] = config_hash
    return result


def command_hash(paths: list[Path]) -> int:
    if not paths:
        raise SystemExit("hash requires at least one file")
    for path in paths:
        print(f"{sha256(path)}  {path}")
    return 0


def command_validate(directory: Path) -> int:
    try:
        check_directory(directory, "trusted definitions directory")
    except ValidationError as error:
        print(json.dumps({"status": "NotConfigured", "reason": str(error)}, sort_keys=True))
        return 2

    found = 0
    failures: list[dict[str, str]] = []
    valid: list[dict[str, Any]] = []
    for name in DEFINITION_NAMES:
        path = directory / name
        if not path.exists() and not path.is_symlink():
            continue
        found += 1
        try:
            valid.append(validate_definition(path))
        except (OSError, ValidationError) as error:
            failures.append({"definition": name, "reason": str(error)})
    if not found:
        print(json.dumps({"status": "NotConfigured", "directory": str(directory)}, sort_keys=True))
        return 2
    result = {"status": "Invalid" if failures else "Valid", "definitions": valid, "failures": failures}
    print(json.dumps(result, sort_keys=True, separators=(",", ":")))
    return 1 if failures else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    hash_parser = subparsers.add_parser("hash", help="print SHA-256 without modifying files")
    hash_parser.add_argument("files", nargs="+", type=Path)
    validate_parser = subparsers.add_parser("validate", help="validate a trusted definition directory")
    validate_parser.add_argument("--directory", required=True, type=Path)
    args = parser.parse_args()
    try:
        if args.command == "hash":
            return command_hash(args.files)
        return command_validate(args.directory)
    except (OSError, ValidationError) as error:
        print(json.dumps({"status": "Invalid", "reason": str(error)}, sort_keys=True))
        return 1


if __name__ == "__main__":
    sys.exit(main())
