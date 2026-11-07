#!/usr/bin/env python3
"""Create a deterministic SHA-256 manifest for release artifacts."""

from __future__ import annotations

import argparse
import hashlib
import os
import stat
import sys
from pathlib import Path


def fail(message: str) -> None:
    raise SystemExit(f"sha256 manifest failed: {message}")


def regular_file(path: Path) -> None:
    try:
        info = path.lstat()
    except FileNotFoundError:
        fail(f"file does not exist: {path}")
    except OSError as error:
        fail(f"cannot inspect {path}: {error}")
    if stat.S_ISLNK(info.st_mode):
        fail(f"symlink is not an artifact: {path}")
    if not stat.S_ISREG(info.st_mode):
        fail(f"artifact is not a regular file: {path}")


def digest(path: Path) -> str:
    regular_file(path)
    hasher = hashlib.sha256()
    try:
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                hasher.update(chunk)
    except OSError as error:
        fail(f"cannot read {path}: {error}")
    return hasher.hexdigest()


def display_name(path: Path, base: Path) -> str:
    try:
        return path.relative_to(base).as_posix()
    except ValueError:
        return path.name


def write_manifest(output: Path, content: str) -> None:
    if output.exists() or output.is_symlink():
        regular_file(output)
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = output.with_name(f".{output.name}.tmp-{os.getpid()}")
    if temporary.exists() or temporary.is_symlink():
        fail(f"temporary output already exists: {temporary}")
    try:
        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
        if hasattr(os, "O_NOFOLLOW"):
            flags |= os.O_NOFOLLOW
        fd = os.open(temporary, flags, 0o644)
        with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as handle:
            handle.write(content)
        os.replace(temporary, output)
    except OSError as error:
        try:
            temporary.unlink(missing_ok=True)
        except OSError:
            pass
        fail(f"cannot write {output}: {error}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("artifacts", nargs="+", type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    artifacts = [path.absolute() for path in args.artifacts]
    output = args.output.absolute()
    if len({str(path) for path in artifacts}) != len(artifacts):
        fail("an artifact was listed more than once")
    if output in artifacts:
        fail("manifest output cannot also be an artifact")
    base = Path(os.path.commonpath([str(path.parent) for path in artifacts]))
    rows = sorted(
        (f"{digest(path)}  {display_name(path, base)}" for path in artifacts),
        key=lambda row: row.rsplit("  ", 1)[1],
    )
    write_manifest(output, "\n".join(rows) + "\n")
    print(f"wrote {output} ({len(rows)} artifacts)")


if __name__ == "__main__":
    main()
