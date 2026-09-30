"""Exercise the release admission script without credentials or GitHub writes."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("verify-reviewed-image-release.sh")
SHA = "a" * 40
REPOSITORY = "example/centaur"
CHECKS = [
    ("CI success", "ci.yml"),
    ("Console CI success", "console-ci.yml"),
    ("Validate CLI pyproject packaging", "validate-cli-packaging.yml"),
    ("Image validation success", "validate-images.yml"),
]


class AdmissionTest(unittest.TestCase):
    def run_gate(self, *, base="main", verified=True, draft=False, conclusion="success", run_sha=SHA):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            commit = {"sha": SHA, "verification": {"verified": verified, "reason": "valid"}}
            pull = {"number": 1, "base": {"ref": base, "repo": {"full_name": REPOSITORY}},
                    "head": {"sha": SHA, "ref": "candidate", "repo": {"full_name": REPOSITORY}},
                    "draft": draft, "state": "open"}
            prefix = f"https://api.github.com/repos/{REPOSITORY}"
            replies = {f"{prefix}/git/commits/{SHA}": commit, f"{prefix}/commits/{SHA}/pulls": [pull]}
            checks = []
            for number, (name, workflow) in enumerate(CHECKS, 1):
                checks.append({"name": name, "head_sha": SHA, "app": {"slug": "github-actions"},
                               "status": "completed", "conclusion": conclusion,
                               "details_url": f"https://github.com/{REPOSITORY}/actions/runs/{number}/job/1"})
                replies[f"{prefix}/actions/runs/{number}"] = {
                    "head_sha": run_sha, "event": "pull_request", "status": "completed",
                    "conclusion": conclusion, "path": ".github/workflows/" + workflow,
                    "head_repository": {"full_name": REPOSITORY}, "head_branch": "candidate"}
            replies[f"{prefix}/commits/{SHA}/check-runs?filter=latest&per_page=100"] = {"check_runs": checks}
            (root / "replies.json").write_text(json.dumps(replies))
            curl = root / "curl"
            curl.write_text(f"#!{sys.executable}\nimport json, pathlib, sys\n"
                            "print(json.dumps(json.loads(pathlib.Path(__file__).with_name('replies.json').read_text())[sys.argv[-1]]))\n")
            git = root / "git"
            git.write_text(f"#!/bin/sh\nprintf '%s\\n' '{SHA}'\n")
            curl.chmod(0o755)
            git.chmod(0o755)
            env = dict(os.environ, PATH=str(root) + os.pathsep + os.environ["PATH"],
                       GITHUB_API_TOKEN="synthetic-fixture", TRIGGER_API_URL="https://api.github.com",
                       TRIGGER_EVENT_NAME="workflow_dispatch", TRIGGER_REF="refs/heads/candidate",
                       TRIGGER_REF_NAME="candidate", TRIGGER_REF_TYPE="branch", TRIGGER_REPOSITORY=REPOSITORY,
                       TRIGGER_SHA=SHA, DISPATCH_REVIEWED_COMMIT=SHA)
            return subprocess.run(["bash", str(SCRIPT)], env=env, capture_output=True, text=True)

    def test_reviewed_main_and_aws_release_branches(self):
        for branch in ("main", "release/aws-core"):
            with self.subTest(branch=branch):
                result = self.run_gate(base=branch)
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_unreviewed_base_branch_fails(self):
        self.assertNotEqual(self.run_gate(base="unreviewed").returncode, 0)

    def test_unsigned_head_fails(self):
        self.assertNotEqual(self.run_gate(verified=False).returncode, 0)

    def test_draft_fails(self):
        self.assertNotEqual(self.run_gate(draft=True).returncode, 0)

    def test_failed_checks_fail(self):
        self.assertNotEqual(self.run_gate(conclusion="failure").returncode, 0)

    def test_checks_on_different_commit_fail(self):
        self.assertNotEqual(self.run_gate(run_sha="b" * 40).returncode, 0)


if __name__ == "__main__":
    unittest.main()
