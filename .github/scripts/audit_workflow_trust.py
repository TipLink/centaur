#!/usr/bin/env python3
"""Fail closed when PR validation regains publication credentials or secrets."""

from __future__ import annotations

from pathlib import Path
import re


ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github" / "workflows"


def read(name: str) -> str:
    return (WORKFLOWS / name).read_text()


def require(text: str, needle: str, source: str) -> None:
    if needle not in text:
        raise SystemExit(f"{source}: missing required trust-boundary marker: {needle}")


def reject(text: str, needle: str, source: str) -> None:
    if needle in text:
        raise SystemExit(f"{source}: forbidden in this trust lane: {needle}")


def audit_pull_request_workflows() -> None:
    label_gated_secret_workflow = "pr-audit.yml"
    for path in sorted(WORKFLOWS.glob("*.yml")):
        text = path.read_text()
        if not re.search(r"^\s{2}pull_request:\s*$", text, re.MULTILINE):
            continue
        if path.name == label_gated_secret_workflow:
            require(text, "types: [labeled]", path.name)
            require(text, "github.event.label.name == 'cyclops'", path.name)
            reject(text, "actions/checkout", path.name)
            continue
        reject(text, "${{ secrets.", path.name)
        for permission in ("contents", "packages", "deployments", "actions"):
            if re.search(rf"^\s+{permission}:\s+write\s*$", text, re.MULTILINE):
                raise SystemExit(f"{path.name}: PR workflow grants {permission}: write")


def audit_split_publication_lanes() -> None:
    image_validation = read("validate-images.yml")
    require(image_validation, "pull_request:", "validate-images.yml")
    require(image_validation, "push: false", "validate-images.yml")
    reject(image_validation, "docker/login-action", "validate-images.yml")
    reject(image_validation, "packages: write", "validate-images.yml")

    image_publish = read("publish-images.yml")
    reject(image_publish, "pull_request:", "publish-images.yml")
    require(image_publish, "tags: [v*]", "publish-images.yml")
    reject(image_publish, "promote-fineas-infra", "publish-images.yml")

    for name in ("docs-deploy.yml", "release-chart-publish.yml"):
        text = read(name)
        reject(text, "pull_request:", name)
        reject(text, "push:", name)
        require(text, "workflow_dispatch:", name)
        require(text, "confirm_reviewed_main", name)
        require(text, "github.ref == 'refs/heads/main'", name)

    for name in ("docs.yml", "release-chart.yml"):
        text = read(name)
        require(text, "pull_request:", name)
        reject(text, "${{ secrets.", name)
        reject(text, "contents: write", name)


def audit_upstream_import_lane() -> None:
    audit = read("upstream-sync.yml")
    require(audit, 'UPSTREAM_OWNER: paradigmxyz', "upstream-sync.yml")
    require(audit, '"${UPSTREAM_OWNER}:${UPSTREAM_BRANCH}"', "upstream-sync.yml")
    require(audit, 'repos/${UPSTREAM_REPO}/commits/${commit}', "upstream-sync.yml")
    require(audit, "draft: true", "upstream-sync.yml")
    reject(audit, "SYNC_BRANCH", "upstream-sync.yml")
    reject(audit, "git push", "upstream-sync.yml")
    reject(audit, "permission-contents: write", "upstream-sync.yml")

    verifier = read("upstream-pr-verify.yml")
    require(verifier, "moving upstream head", "upstream-pr-verify.yml")
    require(verifier, "rev-list --reverse", "upstream-pr-verify.yml")
    require(verifier, "repos/paradigmxyz/centaur/commits/${commit}", "upstream-pr-verify.yml")
    reject(verifier, "${{ secrets.", "upstream-pr-verify.yml")


def main() -> None:
    audit_pull_request_workflows()
    audit_split_publication_lanes()
    audit_upstream_import_lane()
    print("workflow trust-boundary audit passed")


if __name__ == "__main__":
    main()
