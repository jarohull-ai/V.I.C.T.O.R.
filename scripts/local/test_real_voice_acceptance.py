#!/usr/bin/env python3
"""Run the operator-side acceptance check against real local voice executables.

The daemon must already be running with trusted definitions. Missing local
executables/models are reported as NotConfigured; this script never downloads
or falls back to a network provider.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import socket
import sqlite3
import stat
import sys
import time
from pathlib import Path


def fail(message: str, code: int = 1) -> int:
    print(f"{message}", file=sys.stderr)
    return code


def private_regular(path: Path, label: str) -> None:
    try:
        info = path.lstat()
    except FileNotFoundError:
        raise RuntimeError(f"NotConfigured: missing {label}: {path}") from None
    if not stat.S_ISREG(info.st_mode) or path.is_symlink():
        raise RuntimeError(f"NotConfigured: {label} is not a regular file: {path}")
    if info.st_uid != os.geteuid() or stat.S_IMODE(info.st_mode) & 0o077:
        raise RuntimeError(f"NotConfigured: {label} is not private and user-owned: {path}")


def digest(path: Path) -> str:
    return "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest()


def verify_hash(path: Path, expected: str, label: str) -> None:
    if digest(path) != expected:
        raise RuntimeError(f"NotConfigured: {label} hash does not match trusted definition")


def trusted_definition(definitions: Path, name: str) -> dict:
    path = definitions / name
    private_regular(path, f"trusted definition {name}")
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise RuntimeError(f"NotConfigured: invalid trusted definition {name}: {error}") from error


def preflight(args: argparse.Namespace) -> None:
    definitions = args.definitions
    if not definitions.is_dir() or definitions.is_symlink():
        raise RuntimeError(f"NotConfigured: invalid trusted definitions directory: {definitions}")

    stt = trusted_definition(definitions, "local-stt.json")["spec"]
    tts = trusted_definition(definitions, "local-tts.json")["spec"]
    for spec, executable_key, model_key, label in [
        (stt, "executable", "model", "Whisper"),
        (tts, "command", "model", "TTS"),
    ]:
        executable = Path(spec[executable_key][0] if executable_key == "command" else spec[executable_key])
        model = Path(spec[model_key])
        private_regular(executable, f"{label} executable")
        private_regular(model, f"{label} model")
        expected_executable = spec["executable_sha256"]
        expected_model = spec["model_sha256"]
        verify_hash(executable, expected_executable, f"{label} executable")
        verify_hash(model, expected_model, f"{label} model")
        if spec.get("sandbox_required") and not spec.get("sandbox_executable"):
            raise RuntimeError(f"NotConfigured: {label} requires a sandbox executable")
        if spec.get("sandbox_required") and not Path(spec["sandbox_executable"]).is_file():
            raise RuntimeError(f"NotConfigured: sandbox executable is unavailable for {label}")

    private_regular(args.token_file, "daemon token")
    token = args.token_file.read_text(encoding="utf-8").strip()
    if len(token) < 16:
        raise RuntimeError("NotConfigured: daemon token is too short")


def call_voice(args: argparse.Namespace) -> dict:
    audio = args.audio.read_bytes()
    if len(audio) > args.max_audio_bytes:
        raise RuntimeError("NotConfigured: input WAV exceeds the configured acceptance limit")
    request = {
        "token": args.token_file.read_text(encoding="utf-8").strip(),
        "scope": args.scope,
        "idempotency_key": f"real-voice-{time.time_ns()}",
        "operation": {
            "operation": "voice",
            "conversation_id": "real-voice-acceptance",
            "audio": list(audio),
            "at_unix_ms": int(time.time() * 1000),
        },
    }
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.settimeout(args.timeout)
        connection.connect(args.socket)
        connection.sendall(json.dumps(request, separators=(",", ":")).encode() + b"\n")
        data = b""
        while not data.endswith(b"\n"):
            chunk = connection.recv(65536)
            if not chunk:
                raise RuntimeError("daemon closed the socket before returning a response")
            data += chunk
            if len(data) > args.max_response_bytes:
                raise RuntimeError("daemon response exceeds the acceptance limit")
    response = json.loads(data)
    if response.get("ok") is not True:
        raise RuntimeError(f"voice request denied or failed: {response}")
    result = response.get("result", {})
    if result.get("provider") != "whisper.cpp" or result.get("location") != "local":
        raise RuntimeError("voice response did not contain local Whisper provenance")
    if result.get("tts", {}).get("status") != "synthesized":
        raise RuntimeError("voice response did not contain a synthesized local TTS result")
    return response


def verify_sqlite(path: Path) -> None:
    if not path.is_file() or path.is_symlink():
        raise RuntimeError(f"SQLite state is missing: {path}")
    with sqlite3.connect(path) as database:
        rows = database.execute(
            "SELECT content_kind, content_text, content_uri FROM context_items"
        ).fetchall()
        for kind, text, uri in rows:
            if kind == "inline" and ("RIFF" in (text or "") or "WAVE" in (text or "")):
                raise RuntimeError("raw audio marker found in SQLite context")
            if uri and "audio/" in uri:
                raise RuntimeError("raw audio URI found in SQLite context")
        audit_count = database.execute("SELECT COUNT(*) FROM tool_audit").fetchone()[0]
        if audit_count < 1:
            raise RuntimeError("SQLite audit contains no tool execution")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--definitions", type=Path, required=True)
    parser.add_argument("--token-file", type=Path, required=True)
    parser.add_argument("--socket", type=Path, required=True)
    parser.add_argument("--sqlite", type=Path, required=True)
    parser.add_argument("--audio", type=Path, required=True)
    parser.add_argument("--scope", default="local")
    parser.add_argument("--timeout", type=float, default=30.0)
    parser.add_argument("--max-audio-bytes", type=int, default=16 * 1024 * 1024)
    parser.add_argument("--max-response-bytes", type=int, default=4 * 1024 * 1024)
    args = parser.parse_args()
    try:
        private_regular(args.audio, "input WAV")
        preflight(args)
        response = call_voice(args)
        verify_sqlite(args.sqlite)
    except (OSError, RuntimeError, json.JSONDecodeError, sqlite3.Error) as error:
        return fail(str(error))
    print(json.dumps({"ok": True, "voice": response["result"], "raw_audio_persisted": False}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
