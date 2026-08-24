from __future__ import annotations

import importlib.util
import unittest
from pathlib import Path

from scripts import release_manifest


ROOT = Path(__file__).parents[1]
RESOLVER_PATH = ROOT / "deploy" / "scripts" / "resolve_github_release.py"
SPEC = importlib.util.spec_from_file_location("deploy_release_resolver", RESOLVER_PATH)
assert SPEC is not None and SPEC.loader is not None
deploy_release_resolver = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(deploy_release_resolver)


class ReleaseContractAlignmentTests(unittest.TestCase):
    def test_deployment_assets_are_a_subset_of_github_release_matrix(self):
        self.assertEqual(
            release_manifest.MANIFEST_NAME,
            deploy_release_resolver.MANIFEST_NAME,
        )
        self.assertLessEqual(
            deploy_release_resolver.REQUIRED_DEPLOY_ASSETS,
            release_manifest.EXPECTED_ASSETS,
        )


if __name__ == "__main__":
    unittest.main()
