#!/usr/bin/env python3
"""Resolve one GitHub Release into an immutable deployment manifest."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sys
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Callable
from typing import Any


MANIFEST_NAME = "osswitch-release-manifest.json"
REQUIRED_DEPLOY_ASSETS = frozenset(
    {
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
REPOSITORY_RE = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")


class ResolutionError(ValueError):
    pass


def _require_dict(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ResolutionError(f"{label} must be a JSON object")
    return value


def _require_string(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value:
        raise ResolutionError(f"{label} must be a non-empty string")
    return value


def _unique_named_objects(values: Any, label: str) -> dict[str, dict[str, Any]]:
    if not isinstance(values, list):
        raise ResolutionError(f"{label} must be a JSON array")

    result: dict[str, dict[str, Any]] = {}
    for index, raw_value in enumerate(values):
        value = _require_dict(raw_value, f"{label}[{index}]")
        name = _require_string(value.get("name"), f"{label}[{index}].name")
        if name in result:
            raise ResolutionError(f"{label} contains duplicate asset {name!r}")
        result[name] = value
    return result


def validate_resolution(
    repository: str,
    selector: str,
    release: Any,
    commit: Any,
    manifest_bytes: bytes,
) -> dict[str, Any]:
    if not REPOSITORY_RE.fullmatch(repository):
        raise ResolutionError("repository must use owner/repository form")
    if not selector:
        raise ResolutionError("release selector must not be empty")

    release_object = _require_dict(release, "release response")
    if release_object.get("draft") is not False:
        raise ResolutionError("draft releases cannot be deployed")

    release_id = release_object.get("id")
    if not isinstance(release_id, int) or release_id <= 0:
        raise ResolutionError("release response id must be a positive integer")

    tag = _require_string(release_object.get("tag_name"), "release response tag_name")
    if selector != "latest" and tag != selector:
        raise ResolutionError(
            f"release response tag {tag!r} does not match selector {selector!r}"
        )

    commit_object = _require_dict(commit, "commit response")
    commit_sha = _require_string(commit_object.get("sha"), "commit response sha")
    if not COMMIT_RE.fullmatch(commit_sha):
        raise ResolutionError("commit response sha must be a lowercase 40-digit SHA-1")

    release_assets = _unique_named_objects(release_object.get("assets"), "release assets")
    if MANIFEST_NAME not in release_assets:
        raise ResolutionError(f"release is missing {MANIFEST_NAME}")

    try:
        manifest = _require_dict(json.loads(manifest_bytes), "release manifest")
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ResolutionError(f"release manifest is not valid UTF-8 JSON: {error}") from error

    if manifest.get("schema_version") != 1:
        raise ResolutionError("release manifest schema_version must equal 1")
    if manifest.get("repository") != repository:
        raise ResolutionError("release manifest repository does not match deployment repository")
    if manifest.get("release_id") != release_id:
        raise ResolutionError("release manifest release_id does not match release response")
    if manifest.get("tag") != tag:
        raise ResolutionError("release manifest tag does not match release response")
    if manifest.get("commit") != commit_sha:
        raise ResolutionError("release manifest commit does not match resolved tag commit")

    manifest_assets = _unique_named_objects(manifest.get("assets"), "manifest assets")
    if MANIFEST_NAME in manifest_assets:
        raise ResolutionError("release manifest must not list itself as a payload asset")

    missing_required = sorted(REQUIRED_DEPLOY_ASSETS - manifest_assets.keys())
    if missing_required:
        raise ResolutionError(
            "release manifest is missing required deployment assets: "
            + ", ".join(missing_required)
        )

    remote_payload_names = set(release_assets) - {MANIFEST_NAME}
    manifest_names = set(manifest_assets)
    if remote_payload_names != manifest_names:
        missing_remote = sorted(manifest_names - remote_payload_names)
        undeclared_remote = sorted(remote_payload_names - manifest_names)
        details = []
        if missing_remote:
            details.append("missing remote assets: " + ", ".join(missing_remote))
        if undeclared_remote:
            details.append("undeclared remote assets: " + ", ".join(undeclared_remote))
        raise ResolutionError("release asset set differs from manifest: " + "; ".join(details))

    asset_digests: dict[str, str] = {}
    for name, asset in manifest_assets.items():
        digest = _require_string(asset.get("sha256"), f"manifest digest for {name}")
        if not SHA256_RE.fullmatch(digest):
            raise ResolutionError(f"manifest digest for {name!r} is not lowercase SHA-256")
        asset_digests[name] = digest

    encoded_tag = urllib.parse.quote(tag, safe="")
    asset_urls = {
        name: f"https://github.com/{repository}/releases/download/{encoded_tag}/{name}"
        for name in sorted(asset_digests)
    }

    return {
        "schema_version": 1,
        "repository": repository,
        "release_id": release_id,
        "tag": tag,
        "commit": commit_sha,
        "manifest_sha256": hashlib.sha256(manifest_bytes).hexdigest(),
        "asset_digests": dict(sorted(asset_digests.items())),
        "asset_urls": asset_urls,
    }


def resolve_release(
    repository: str,
    selector: str,
    fetch_json: Callable[[str], Any],
    fetch_bytes: Callable[[str], bytes],
) -> dict[str, Any]:
    if not REPOSITORY_RE.fullmatch(repository):
        raise ResolutionError("repository must use owner/repository form")

    encoded_repository = "/".join(
        urllib.parse.quote(part, safe="") for part in repository.split("/", 1)
    )
    if selector == "latest":
        release_url = f"https://api.github.com/repos/{encoded_repository}/releases/latest"
    else:
        encoded_selector = urllib.parse.quote(selector, safe="")
        release_url = (
            f"https://api.github.com/repos/{encoded_repository}/releases/tags/"
            f"{encoded_selector}"
        )

    release = fetch_json(release_url)
    release_object = _require_dict(release, "release response")
    tag = _require_string(release_object.get("tag_name"), "release response tag_name")
    encoded_tag = urllib.parse.quote(tag, safe="")
    commit = fetch_json(
        f"https://api.github.com/repos/{encoded_repository}/commits/{encoded_tag}"
    )

    release_assets = _unique_named_objects(release_object.get("assets"), "release assets")
    manifest_asset = release_assets.get(MANIFEST_NAME)
    if manifest_asset is None:
        raise ResolutionError(f"release is missing {MANIFEST_NAME}")
    manifest_url = _require_string(
        manifest_asset.get("browser_download_url"),
        f"{MANIFEST_NAME} browser_download_url",
    )
    manifest_bytes = fetch_bytes(manifest_url)
    return validate_resolution(repository, selector, release, commit, manifest_bytes)


def _request(url: str) -> bytes:
    headers = {
        "Accept": "application/vnd.github+json",
        "User-Agent": "osswitch-ansible-release-resolver/1",
        "X-GitHub-Api-Version": "2022-11-28",
    }
    token = os.environ.get("GITHUB_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(url, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return response.read()
    except urllib.error.HTTPError as error:
        raise ResolutionError(f"GitHub returned HTTP {error.code} for {url}") from error
    except urllib.error.URLError as error:
        raise ResolutionError(f"unable to fetch {url}: {error.reason}") from error


def _fetch_json(url: str) -> Any:
    try:
        return json.loads(_request(url))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ResolutionError(f"GitHub returned invalid JSON for {url}: {error}") from error


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repository", required=True)
    parser.add_argument("--selector", required=True)
    args = parser.parse_args()

    try:
        resolution = resolve_release(
            args.repository,
            args.selector,
            fetch_json=_fetch_json,
            fetch_bytes=_request,
        )
    except ResolutionError as error:
        print(f"release resolution failed: {error}", file=sys.stderr)
        return 1

    json.dump(resolution, sys.stdout, sort_keys=True, separators=(",", ":"))
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
