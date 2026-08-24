#!/usr/bin/env python3
"""Build and verify the immutable osswitch release asset manifest."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path
from typing import Any


MANIFEST_NAME = "osswitch-release-manifest.json"
EXPECTED_ASSETS = frozenset(
    {
        "lan-mouse-gtk-linux-x86_64.tar.gz",
        "lan-mouse-gtk-linux-aarch64.tar.gz",
        "lan-mouse-gtk-windows-x86_64.zip",
        "lan-mouse-gtk-macos-x86_64.zip",
        "lan-mouse-gtk-macos-aarch64.zip",
        "lan-mouse-no-gtk-linux-x86_64.tar.gz",
        "lan-mouse-no-gtk-linux-aarch64.tar.gz",
        "lan-mouse-no-gtk-windows-x86_64.zip",
        "lan-mouse-no-gtk-macos-x86_64.zip",
        "lan-mouse-no-gtk-macos-aarch64.zip",
        "tv-multiview-linux-x86_64.tar.gz",
    }
)
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")


class ManifestError(ValueError):
    pass


def _require_object(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ManifestError(f"{label} must be a JSON object")
    return value


def _require_string(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value:
        raise ManifestError(f"{label} must be a non-empty string")
    return value


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _direct_files(directory: Path, expected: set[str]) -> dict[str, Path]:
    if not directory.is_dir():
        raise ManifestError(f"asset directory does not exist: {directory}")
    entries = list(directory.iterdir())
    non_files = sorted(entry.name for entry in entries if not entry.is_file())
    if non_files:
        raise ManifestError("asset directory contains non-files: " + ", ".join(non_files))
    files = {entry.name: entry for entry in entries}
    names = set(files)
    if names != expected:
        missing = sorted(expected - names)
        extra = sorted(names - expected)
        details = []
        if missing:
            details.append("missing: " + ", ".join(missing))
        if extra:
            details.append("unexpected: " + ", ".join(extra))
        raise ManifestError("release asset set mismatch: " + "; ".join(details))
    return files


def verify_local_assets(directory: Path) -> list[dict[str, Any]]:
    files = _direct_files(directory, set(EXPECTED_ASSETS))
    return [
        {
            "name": name,
            "size": files[name].stat().st_size,
            "sha256": _sha256(files[name]),
        }
        for name in sorted(files)
    ]


def build_manifest(
    directory: Path,
    repository: str,
    release_id: int,
    tag: str,
    commit: str,
) -> dict[str, Any]:
    if "/" not in repository or repository.startswith("/") or repository.endswith("/"):
        raise ManifestError("repository must use owner/repository form")
    if release_id <= 0:
        raise ManifestError("release_id must be positive")
    if not tag:
        raise ManifestError("tag must not be empty")
    if not COMMIT_RE.fullmatch(commit):
        raise ManifestError("commit must be a lowercase 40-digit SHA-1")
    return {
        "schema_version": 1,
        "repository": repository,
        "release_id": release_id,
        "tag": tag,
        "commit": commit,
        "assets": verify_local_assets(directory),
    }


def _unique_remote_assets(values: Any) -> dict[str, dict[str, Any]]:
    if not isinstance(values, list):
        raise ManifestError("release assets must be a JSON array")
    result: dict[str, dict[str, Any]] = {}
    for index, raw in enumerate(values):
        asset = _require_object(raw, f"release assets[{index}]")
        name = _require_string(asset.get("name"), f"release assets[{index}].name")
        if name in result:
            raise ManifestError(f"release contains duplicate asset {name!r}")
        result[name] = asset
    return result


def verify_remote_draft(
    release_path: Path,
    local_directory: Path,
    remote_directory: Path,
    manifest_path: Path,
) -> None:
    release = _require_object(json.loads(release_path.read_bytes()), "release")
    manifest = _require_object(json.loads(manifest_path.read_bytes()), "manifest")
    if release.get("draft") is not True:
        raise ManifestError("release must remain a draft during verification")
    if release.get("id") != manifest.get("release_id"):
        raise ManifestError("release ID does not match manifest")
    if release.get("tag_name") != manifest.get("tag"):
        raise ManifestError("release tag does not match manifest")
    if release.get("target_commitish") != manifest.get("commit"):
        raise ManifestError("release target commit does not match manifest")

    local_assets = {entry["name"]: entry for entry in verify_local_assets(local_directory)}
    manifest_assets = manifest.get("assets")
    if not isinstance(manifest_assets, list):
        raise ManifestError("manifest assets must be a JSON array")
    declared: dict[str, dict[str, Any]] = {}
    for raw in manifest_assets:
        entry = _require_object(raw, "manifest asset")
        name = _require_string(entry.get("name"), "manifest asset name")
        if name in declared:
            raise ManifestError(f"manifest contains duplicate asset {name!r}")
        if not SHA256_RE.fullmatch(str(entry.get("sha256", ""))):
            raise ManifestError(f"manifest digest for {name!r} is invalid")
        declared[name] = entry
    if declared != local_assets:
        raise ManifestError("manifest payload entries differ from local assets")

    remote_assets = _unique_remote_assets(release.get("assets"))
    expected_remote = set(EXPECTED_ASSETS) | {MANIFEST_NAME}
    if set(remote_assets) != expected_remote:
        raise ManifestError("remote release asset names differ from the complete matrix")

    remote_files = _direct_files(remote_directory, expected_remote)
    for name, local in local_assets.items():
        remote = remote_assets[name]
        if remote.get("size") != local["size"]:
            raise ManifestError(f"remote size differs for {name!r}")
        if _sha256(remote_files[name]) != local["sha256"]:
            raise ManifestError(f"remote digest differs for {name!r}")

    manifest_bytes = manifest_path.read_bytes()
    if remote_assets[MANIFEST_NAME].get("size") != len(manifest_bytes):
        raise ManifestError("remote manifest size differs")
    if remote_files[MANIFEST_NAME].read_bytes() != manifest_bytes:
        raise ManifestError("remote manifest bytes differ")


def main() -> int:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)

    verify = subparsers.add_parser("verify")
    verify.add_argument("--assets-dir", type=Path, required=True)

    create = subparsers.add_parser("create")
    create.add_argument("--assets-dir", type=Path, required=True)
    create.add_argument("--repository", required=True)
    create.add_argument("--release-id", type=int, required=True)
    create.add_argument("--tag", required=True)
    create.add_argument("--commit", required=True)
    create.add_argument("--output", type=Path, required=True)

    remote = subparsers.add_parser("verify-remote")
    remote.add_argument("--release-json", type=Path, required=True)
    remote.add_argument("--assets-dir", type=Path, required=True)
    remote.add_argument("--remote-dir", type=Path, required=True)
    remote.add_argument("--manifest", type=Path, required=True)

    args = parser.parse_args()
    try:
        if args.command == "verify":
            result = verify_local_assets(args.assets_dir)
            json.dump(result, sys.stdout, sort_keys=True, separators=(",", ":"))
            sys.stdout.write("\n")
        elif args.command == "create":
            manifest = build_manifest(
                args.assets_dir,
                args.repository,
                args.release_id,
                args.tag,
                args.commit,
            )
            args.output.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
        else:
            verify_remote_draft(
                args.release_json,
                args.assets_dir,
                args.remote_dir,
                args.manifest,
            )
    except (ManifestError, OSError, json.JSONDecodeError) as error:
        print(f"release manifest validation failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
