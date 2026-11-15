"""Exercise validation selection independently from release builds and GitHub credentials."""
import importlib
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

planner = importlib.import_module("plan-pr-images")


class ValidationTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.directory = Path(temp.name)
        self.services = ("agent", "slackbotv2", "linearbot")
        self.all = {("agent", "linux/amd64"), ("agent", "linux/arm64"),
                    ("slackbotv2", "linux/amd64"), ("linearbot", "linux/amd64")}
        for service, platform in self.all:
            sources = ["harness"] if service == "agent" else [f"services/{service}", "packages", "pnpm-lock.yaml"]
            filename = f"{service}-{platform.split('/')[1]}.json"
            (self.directory / filename).write_text(json.dumps({"inputs": sources, "materials": [["base", "digest"]]}))

    def select(self, paths, digest="digest", baseline="base"):
        with patch.object(planner.images, "SERVICES", self.services), \
             patch.object(planner.images, "changed_paths", return_value=set(paths)), \
             patch.object(planner.images, "manifest", return_value=(digest, {})):
            return {(row["service"], row["platform"]) for row in planner.matrix(self.directory, baseline, "HEAD")}

    def test_isolated_shared_and_probe_changes(self):
        self.assertEqual(self.select(["services/slackbotv2/src/user-input.ts"]), {("slackbotv2", "linux/amd64")})
        self.assertEqual(self.select(["packages/shared/file"]), {p for p in self.all if p[0] != "agent"})
        self.assertEqual(self.select([planner.PROBE]), {p for p in self.all if p[0] == "agent"})
        self.assertEqual(self.select(["docs/intro.md"]), set())

    def test_missing_proof_moved_base_and_ignores(self):
        self.assertEqual(self.select([".dockerignore"]), self.all)
        self.assertEqual(self.select([], digest="moved"), self.all)
        self.assertEqual(self.select([], baseline=None), self.all)
        (self.directory / "agent-arm64.json").unlink()
        self.assertEqual(self.select([]), {("agent", "linux/arm64")})

    def test_fork_or_unmerged_workflow_cannot_supply_inputs(self):
        repo = "example/project"
        artifact = {"expired": False, "workflow_run": {"id": 1}}
        run = {"event": "pull_request", "conclusion": "success", "path": planner.WORKFLOW,
               "head_repository": {"full_name": repo}, "head_sha": "abc"}
        pull = {"merged_at": None, "head": {"sha": "abc", "repo": {"full_name": repo}},
                "base": {"repo": {"full_name": repo}, "ref": "main"}}
        for head_repo in (repo, "fork/project"):
            run["head_repository"]["full_name"] = head_repo
            with patch.dict(os.environ, GITHUB_REPOSITORY=repo, GITHUB_BASE_REF="main", PR_BASE_SHA="base"), \
                 patch.object(planner, "gh", side_effect=[{"artifacts": [artifact]}, run, [pull]]), \
                 patch.object(planner.images, "run") as download, self.assertRaises(ValueError):
                planner.merged_snapshot(self.directory)
            download.assert_not_called()


if __name__ == "__main__":
    unittest.main()
