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


def require_count(text: str, needle: str, minimum: int, source: str) -> None:
    count = text.count(needle)
    if count < minimum:
        raise SystemExit(
            f"{source}: expected at least {minimum} occurrences of {needle}, found {count}"
        )


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
    require(image_validation, "name: Image validation success", "validate-images.yml")
    for helper in (
        ".github/scripts/resolve-runnable-image-digest.sh",
        ".github/scripts/verify-registry-tag-absent.sh",
        ".github/scripts/verify-reviewed-image-release.sh",
    ):
        require(image_validation, helper, "validate-images.yml")
    reject(image_validation, "docker/login-action", "validate-images.yml")
    reject(image_validation, "packages: write", "validate-images.yml")

    image_publish = read("publish-images.yml")
    reject(image_publish, "pull_request:", "publish-images.yml")
    require(image_publish, "'reviewed-images-publish-*'", "publish-images.yml")
    require(image_publish, "group: publish-reviewed-centaur-images", "publish-images.yml")
    require(image_publish, "cancel-in-progress: false", "publish-images.yml")
    require(image_publish, "checks: read", "publish-images.yml")
    require(
        image_publish,
        "verify-reviewed-image-release.sh",
        "publish-images.yml",
    )
    require(
        image_publish,
        "verify-registry-tag-absent.sh",
        "publish-images.yml",
    )
    require_count(
        image_publish,
        "verify-registry-tag-absent.sh",
        2,
        "publish-images.yml",
    )
    require(image_publish, "REVIEWED_TAG: reviewed-${{ github.sha }}", "publish-images.yml")
    require(image_publish, 'tag="reviewed-${RELEASE_REVISION}"', "publish-images.yml")
    require(image_publish, 'needs: tag-absence-gate', "publish-images.yml")
    require(
        image_publish,
        'if [[ "${#root_digest_files[@]}" -ne 2 ]]',
        "publish-images.yml",
    )
    require(
        image_publish,
        "pattern: runnable-child-digests-*-linux-arm64",
        "publish-images.yml",
    )
    require(
        image_publish,
        'if [[ "$digest" != "$run_child_digest" ]]',
        "publish-images.yml",
    )
    reject(image_publish, 'tag="sha-${RELEASE_REVISION', "publish-images.yml")
    reject(image_publish, "type=semver", "publish-images.yml")
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
