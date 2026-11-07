#!/usr/bin/env python3
"""Deterministic repository boundary and license checks for CI."""

from __future__ import annotations

import sys
import tomllib
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
EXPECTED_LICENSE = "BUSL-1.1"
VENDORED_DIR = (ROOT / "vendor" / "bse-fetch").resolve()


def fail(message: str) -> None:
    print(f"repository check failed: {message}", file=sys.stderr)
    raise SystemExit(1)


def load(path: Path) -> dict:
    try:
        return tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        fail(f"cannot read {path.relative_to(ROOT)}: {error}")


def dependency_tables(document: dict):
    for key, value in document.items():
        if key == "dependencies" and isinstance(value, dict):
            yield value
        if isinstance(value, dict):
            yield from dependency_tables(value)


def check_path_dependencies(path: Path, document: dict) -> None:
    for table in dependency_tables(document):
        for dependency, specification in table.items():
            if not isinstance(specification, dict) or "path" not in specification:
                continue
            dependency_path = (path.parent / specification["path"]).resolve()
            inside_workspace = dependency_path == ROOT or ROOT in dependency_path.parents
            if not inside_workspace:
                fail(
                    f"path dependency {dependency!r} in {path.relative_to(ROOT)} "
                    f"escapes the workspace: {specification['path']!r}"
                )


def main() -> None:
    root_manifest = ROOT / "Cargo.toml"
    root = load(root_manifest)
    workspace = root.get("workspace")
    package = workspace.get("package") if isinstance(workspace, dict) else None
    if not isinstance(workspace, dict) or not isinstance(package, dict):
        fail("root Cargo.toml must define a workspace package")
    source = root_manifest.read_text(encoding="utf-8")
    if package.get("license") != EXPECTED_LICENSE:
        fail("workspace license must be BUSL-1.1")
    if not (ROOT / "LICENSE").is_file():
        fail("root LICENSE is missing")

    members = workspace.get("members", [])
    if not isinstance(members, list):
        fail("workspace.members must be a list")
    for member in members:
        manifest_path = (ROOT / member / "Cargo.toml").resolve()
        if not manifest_path.is_file():
            fail(f"workspace member manifest is missing: {member}")
        document = load(manifest_path)
        if document.get("package", {}).get("license", {}).get("workspace") is not True:
            fail(f"workspace member does not inherit BUSL-1.1: {member}")
        check_path_dependencies(manifest_path, document)

    vendor_manifest = VENDORED_DIR / "Cargo.toml"
    vendor = load(vendor_manifest)
    if vendor.get("package", {}).get("license") != "MIT":
        fail("vendor/bse-fetch must remain MIT")
    for filename in ("LICENSE", "ORIGIN.md"):
        if not (VENDORED_DIR / filename).is_file():
            fail(f"vendor/bse-fetch/{filename} is missing")
    check_path_dependencies(vendor_manifest, vendor)
    print("repository boundaries and licenses: ok")


if __name__ == "__main__":
    main()
