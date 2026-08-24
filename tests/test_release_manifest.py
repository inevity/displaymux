from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from scripts import release_manifest


REPOSITORY = "example/osswitch"
TAG = "v1.2.3"
COMMIT = "1" * 40


class ReleaseManifestTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.assets = self.root / "assets"
        self.assets.mkdir()
        for index, name in enumerate(sorted(release_manifest.EXPECTED_ASSETS)):
            (self.assets / name).write_bytes(f"asset-{index}".encode())

    def tearDown(self):
        self.temporary.cleanup()

    def test_manifest_binds_complete_sorted_asset_set(self):
        manifest = release_manifest.build_manifest(
            self.assets, REPOSITORY, 42, TAG, COMMIT
        )

        self.assertEqual(manifest["release_id"], 42)
        self.assertEqual(manifest["tag"], TAG)
        self.assertEqual(manifest["commit"], COMMIT)
        self.assertEqual(
            [asset["name"] for asset in manifest["assets"]],
            sorted(release_manifest.EXPECTED_ASSETS),
        )

    def test_missing_or_extra_asset_is_rejected(self):
        missing = next(iter(release_manifest.EXPECTED_ASSETS))
        (self.assets / missing).unlink()
        with self.assertRaisesRegex(release_manifest.ManifestError, "missing"):
            release_manifest.verify_local_assets(self.assets)

        (self.assets / missing).write_bytes(b"restored")
        (self.assets / "unexpected.zip").write_bytes(b"unexpected")
        with self.assertRaisesRegex(release_manifest.ManifestError, "unexpected"):
            release_manifest.verify_local_assets(self.assets)

    def test_remote_verification_rejects_changed_bytes(self):
        manifest = release_manifest.build_manifest(
            self.assets, REPOSITORY, 42, TAG, COMMIT
        )
        manifest_path = self.root / release_manifest.MANIFEST_NAME
        manifest_path.write_text(json.dumps(manifest, sort_keys=True))

        remote = self.root / "remote"
        remote.mkdir()
        for name in release_manifest.EXPECTED_ASSETS:
            (remote / name).write_bytes((self.assets / name).read_bytes())
        (remote / release_manifest.MANIFEST_NAME).write_bytes(manifest_path.read_bytes())

        release = {
            "id": 42,
            "tag_name": TAG,
            "target_commitish": COMMIT,
            "draft": True,
            "assets": [
                {"name": path.name, "size": path.stat().st_size}
                for path in sorted(remote.iterdir())
            ],
        }
        release_path = self.root / "release.json"
        release_path.write_text(json.dumps(release))

        release_manifest.verify_remote_draft(
            release_path, self.assets, remote, manifest_path
        )
        changed = next(iter(release_manifest.EXPECTED_ASSETS))
        (remote / changed).write_bytes(b"changed")
        with self.assertRaisesRegex(release_manifest.ManifestError, "digest differs"):
            release_manifest.verify_remote_draft(
                release_path, self.assets, remote, manifest_path
            )


if __name__ == "__main__":
    unittest.main()
