from __future__ import annotations

import importlib.util
import json
import unittest
from pathlib import Path


SCRIPT = Path(__file__).parents[1] / "scripts" / "resolve_github_release.py"
SPEC = importlib.util.spec_from_file_location("release_resolver", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
resolver = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(resolver)


REPOSITORY = "example/osswitch"
TAG = "v1.2.3"
COMMIT = "1" * 40


def valid_documents():
    assets = sorted(resolver.REQUIRED_DEPLOY_ASSETS | {"lan-mouse-gtk-linux-x86_64.tar.gz"})
    manifest = {
        "schema_version": 1,
        "repository": REPOSITORY,
        "release_id": 42,
        "tag": TAG,
        "commit": COMMIT,
        "assets": [{"name": name, "sha256": "a" * 64} for name in assets],
    }
    release_assets = [
        {
            "name": name,
            "browser_download_url": f"https://downloads.invalid/{name}",
        }
        for name in assets
    ]
    release_assets.append(
        {
            "name": resolver.MANIFEST_NAME,
            "browser_download_url": "https://downloads.invalid/manifest.json",
        }
    )
    release = {
        "id": 42,
        "tag_name": TAG,
        "draft": False,
        "assets": release_assets,
    }
    return release, {"sha": COMMIT}, manifest


class ValidateResolutionTests(unittest.TestCase):
    def validate(self, release, commit, manifest, selector="latest"):
        return resolver.validate_resolution(
            REPOSITORY,
            selector,
            release,
            commit,
            json.dumps(manifest, sort_keys=True).encode(),
        )

    def test_valid_release_freezes_canonical_urls_and_digests(self):
        release, commit, manifest = valid_documents()
        result = self.validate(release, commit, manifest)

        self.assertEqual(result["release_id"], 42)
        self.assertEqual(result["tag"], TAG)
        self.assertEqual(result["commit"], COMMIT)
        self.assertNotIn("latest/download", " ".join(result["asset_urls"].values()))
        self.assertEqual(set(result["asset_digests"]), set(result["asset_urls"]))

    def test_explicit_selector_must_match_release_tag(self):
        release, commit, manifest = valid_documents()
        with self.assertRaisesRegex(resolver.ResolutionError, "does not match selector"):
            self.validate(release, commit, manifest, selector="v9.9.9")

    def test_duplicate_manifest_asset_is_rejected(self):
        release, commit, manifest = valid_documents()
        manifest["assets"].append(dict(manifest["assets"][0]))
        with self.assertRaisesRegex(resolver.ResolutionError, "duplicate asset"):
            self.validate(release, commit, manifest)

    def test_missing_required_asset_is_rejected(self):
        release, commit, manifest = valid_documents()
        missing = next(iter(resolver.REQUIRED_DEPLOY_ASSETS))
        manifest["assets"] = [asset for asset in manifest["assets"] if asset["name"] != missing]
        release["assets"] = [asset for asset in release["assets"] if asset["name"] != missing]
        with self.assertRaisesRegex(resolver.ResolutionError, "missing required"):
            self.validate(release, commit, manifest)

    def test_undeclared_remote_asset_is_rejected(self):
        release, commit, manifest = valid_documents()
        release["assets"].append(
            {"name": "unexpected.zip", "browser_download_url": "https://invalid/unexpected"}
        )
        with self.assertRaisesRegex(resolver.ResolutionError, "undeclared remote"):
            self.validate(release, commit, manifest)

    def test_mixed_release_commit_is_rejected(self):
        release, commit, manifest = valid_documents()
        manifest["commit"] = "2" * 40
        with self.assertRaisesRegex(resolver.ResolutionError, "commit does not match"):
            self.validate(release, commit, manifest)

    def test_invalid_digest_is_rejected(self):
        release, commit, manifest = valid_documents()
        manifest["assets"][0]["sha256"] = "not-a-digest"
        with self.assertRaisesRegex(resolver.ResolutionError, "not lowercase SHA-256"):
            self.validate(release, commit, manifest)


class ResolveReleaseTests(unittest.TestCase):
    def test_latest_endpoint_is_queried_once_and_manifest_once(self):
        release, commit, manifest = valid_documents()
        json_urls = []
        byte_urls = []

        def fetch_json(url):
            json_urls.append(url)
            return release if url.endswith("/releases/latest") else commit

        def fetch_bytes(url):
            byte_urls.append(url)
            return json.dumps(manifest).encode()

        result = resolver.resolve_release(REPOSITORY, "latest", fetch_json, fetch_bytes)

        self.assertEqual(result["tag"], TAG)
        self.assertEqual(len(json_urls), 2)
        self.assertEqual(sum(url.endswith("/releases/latest") for url in json_urls), 1)
        self.assertEqual(len(byte_urls), 1)


if __name__ == "__main__":
    unittest.main()
