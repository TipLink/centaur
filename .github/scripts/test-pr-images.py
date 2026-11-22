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
        self.services = ("agent", "codemode", "slackbotv2", "linearbot")
        self.all = {("agent", "linux/amd64"), ("agent", "linux/arm64"),
                    ("slackbotv2", "linux/amd64"), ("linearbot", "linux/amd64"),
                    ("codemode", "linux/amd64"), ("codemode", "linux/arm64")}
        for service, platform in self.all:
            if service in ("codemode", "agent"):
                sources = ["Cargo.toml", "Cargo.lock", "crates", "connectors"]
                if service == "agent":
                    sources.append("harness")
            else:
                sources = [f"services/{service}", "packages", "pnpm-lock.yaml"]
            filename = f"{service}-{platform.split('/')[1]}.json"
            (self.directory / filename).write_text(json.dumps({"inputs": sources, "materials": [["base", "digest"]]}))

    def select(self, paths, digest="digest", baseline="base"):
        with patch.object(planner.images, "SERVICES", self.services), \
             patch.object(planner.images, "changed_paths", return_value=set(paths)), \
             patch.object(planner.images, "manifest", return_value=(digest, {})):
            return {(row["service"], row["platform"]) for row in planner.matrix(self.directory, baseline, "HEAD")}

    def test_isolated_shared_and_probe_changes(self):
        self.assertEqual(self.select(["services/slackbotv2/src/user-input.ts"]), {("slackbotv2", "linux/amd64")})
        self.assertEqual(self.select(["packages/shared/file"]), {p for p in self.all if p[0] in ("slackbotv2", "linearbot")})
        self.assertEqual(self.select([planner.images.AGENT_PROBE]), {p for p in self.all if p[0] == "agent"})
        self.assertEqual(self.select(["docs/intro.md"]), set())

    def test_codemode_inputs_on_both_platforms(self):
        for path in ("Cargo.toml", "Cargo.lock", "crates/centaur-codemode/src/lib.rs",
                     "crates/centaur-sandbox-core/src/lib.rs", "connectors/example/index.ts"):
            with self.subTest(path=path):
                self.assertEqual(self.select([path]), {p for p in self.all if p[0] in ("codemode", "agent")})
        self.assertEqual(self.select(["services/codemode/Dockerfile"]),
                         {p for p in self.all if p[0] == "codemode"})

    def test_missing_proof_moved_base_and_ignores(self):
        self.assertEqual(self.select([".dockerignore"]), self.all)
        self.assertEqual(self.select([], digest="moved"), self.all)
        self.assertEqual(self.select([], baseline=None), self.all)
        (self.directory / "agent-arm64.json").unlink()
        self.assertEqual(self.select([]), {("agent", "linux/arm64")})

    def test_only_successful_base_push_runs_can_seed_other_prs(self):
        calls = []
        def result(*args):
            calls.append(args)
            if args[:3] == ("gh", "run", "list"):
                return b'[{"databaseId": 1, "headSha": "landed", "url": "example"}]'
            if args[:3] == ("gh", "run", "download"):
                (self.directory / "revision").write_text("landed")
            return b""
        with patch.dict(os.environ, GITHUB_REPOSITORY="example/project", BASE_BRANCH="main", BASE_SHA="base"), \
             patch.object(planner.images, "run", side_effect=result):
            self.assertEqual(planner.base_snapshot(self.directory), "landed")
        lookup = calls[0]
        for option, value in (("--event", "push"), ("--status", "success"), ("--branch", "main")):
            self.assertEqual(lookup[lookup.index(option) + 1], value)
        self.assertEqual(calls[1], ("git", "merge-base", "--is-ancestor", "landed", "base"))


if __name__ == "__main__":
    unittest.main()
