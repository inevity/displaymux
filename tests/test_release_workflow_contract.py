from __future__ import annotations

import re
import unittest
from pathlib import Path

from scripts import release_manifest


WORKFLOW_PATH = Path(__file__).parents[1] / ".github" / "workflows" / "release.yml"
WORKFLOW = WORKFLOW_PATH.read_text()


class ReleaseWorkflowContractTests(unittest.TestCase):
    def test_release_is_github_actions_only_and_not_main_push_deployment(self):
        self.assertNotIn("ansible", WORKFLOW.lower())
        self.assertNotIn("branches:", WORKFLOW)
        self.assertIn("tags:\n      - 'v*'", WORKFLOW)
        self.assertIn("workflow_dispatch:", WORKFLOW)

    def test_every_action_reference_is_an_immutable_commit(self):
        references = re.findall(r"^\s*uses:\s*([^\s#]+)", WORKFLOW, re.MULTILINE)
        self.assertTrue(references)
        for reference in references:
            self.assertRegex(reference, r"^[^/@]+/[^/@]+@[0-9a-f]{40}$")

    def test_complete_manifest_matrix_is_named_by_workflow(self):
        for asset in release_manifest.EXPECTED_ASSETS:
            self.assertIn(asset.removesuffix(".tar.gz").removesuffix(".zip"), WORKFLOW)

    def test_only_final_job_receives_write_permission(self):
        self.assertEqual(WORKFLOW.count("contents: write"), 1)
        self.assertIn("permissions:\n  contents: read", WORKFLOW)
        self.assertIn("environment: release", WORKFLOW)

    def test_draft_is_verified_before_publication(self):
        create = WORKFLOW.index("Create unpublished draft and manifest")
        verify = WORKFLOW.index("Download and verify every remote draft byte")
        publish = WORKFLOW.index("Publish verified draft")
        self.assertLess(create, verify)
        self.assertLess(verify, publish)
        self.assertIn("draft:true", WORKFLOW)
        self.assertIn("-F draft=false", WORKFLOW)


if __name__ == "__main__":
    unittest.main()
