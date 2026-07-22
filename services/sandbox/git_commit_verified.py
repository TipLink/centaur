#!/usr/bin/env python3
"""Publish the staged Git diff as a GitHub-authored, verified commit."""

from __future__ import annotations

import argparse
import base64
import json
import pathlib
import re
import subprocess
import sys
import urllib.parse
from dataclasses import dataclass
from typing import Any


CREATE_COMMIT_MUTATION = """
mutation($input: CreateCommitOnBranchInput!) {
  createCommitOnBranch(input: $input) {
    commit { oid url }
  }
}
"""

GITHUB_REMOTE_RE = re.compile(
    r"^(?:https?://github\.com/|ssh://git@github\.com/|git@github\.com:)"
    r"(?P<repository>[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+?)(?:\.git)?/?$"
)
REPOSITORY_RE = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
SUPPORTED_EXISTING_MODES = {"100644", "100755"}


class VerifiedCommitError(RuntimeError):
    """A failure that makes the verified commit workflow unsafe to continue."""


@dataclass(frozen=True)
class StagedChanges:
    additions: list[dict[str, str]]
    deletions: list[dict[str, str]]
    tree_oid: str


def run_command(
    command: list[str],
    *,
    cwd: pathlib.Path | None = None,
    stdin: bytes | None = None,
) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(
        command,
        cwd=cwd,
        input=stdin,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )


def display_output(value: bytes) -> str:
    return value.decode("utf-8", errors="replace").strip()


def require_success(
    result: subprocess.CompletedProcess[bytes], description: str
) -> bytes:
    if result.returncode != 0:
        detail = (
            display_output(result.stderr)
            or display_output(result.stdout)
            or description
        )
        raise VerifiedCommitError(f"{description}: {detail}")
    return result.stdout


def git(repo_root: pathlib.Path, *args: str) -> bytes:
    result = run_command(["git", *args], cwd=repo_root)
    return require_success(result, f"git {' '.join(args)} failed")


def parse_json_output(result: subprocess.CompletedProcess[bytes]) -> dict[str, Any]:
    require_success(result, "GitHub API request failed")
    try:
        value = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise VerifiedCommitError("GitHub API returned invalid JSON") from error
    if not isinstance(value, dict):
        raise VerifiedCommitError("GitHub API returned an unexpected JSON value")
    return value


def gh_api(
    endpoint: str,
    *,
    method: str = "GET",
    payload: dict[str, Any] | None = None,
    allow_not_found: bool = False,
) -> dict[str, Any] | None:
    command = ["gh", "api"]
    if method != "GET":
        command.extend(["--method", method])
    command.append(endpoint)
    stdin = None
    if payload is not None:
        command.extend(["--input", "-"])
        stdin = json.dumps(payload, separators=(",", ":")).encode()
    result = run_command(command, stdin=stdin)
    if (
        allow_not_found
        and result.returncode != 0
        and "HTTP 404" in display_output(result.stderr)
    ):
        return None
    return parse_json_output(result)


def repository_from_remote(remote_url: str) -> str:
    match = GITHUB_REMOTE_RE.fullmatch(remote_url.strip())
    if not match:
        raise VerifiedCommitError(
            "cannot derive an owner/repository name from the GitHub remote; "
            "pass --repository"
        )
    return match.group("repository")


def validate_repository(repository: str) -> None:
    if not REPOSITORY_RE.fullmatch(repository):
        raise VerifiedCommitError("repository must use the owner/name form")


def nul_paths(value: bytes) -> list[str]:
    raw_paths = [entry for entry in value.split(b"\0") if entry]
    try:
        return [entry.decode("utf-8") for entry in raw_paths]
    except UnicodeDecodeError as error:
        raise VerifiedCommitError("GitHub commit paths must be valid UTF-8") from error


def tree_entry(value: bytes, *, source: str) -> tuple[str, str] | None:
    entries = [entry for entry in value.split(b"\0") if entry]
    if not entries:
        return None
    if len(entries) != 1:
        raise VerifiedCommitError(f"{source} returned more than one tree entry")
    metadata, separator, _path = entries[0].partition(b"\t")
    fields = metadata.split()
    if not separator or len(fields) < 2:
        raise VerifiedCommitError(f"{source} returned an invalid tree entry")
    return fields[0].decode("ascii"), fields[1].decode("ascii")


def staged_index_entry(repo_root: pathlib.Path, path: str) -> tuple[str, str]:
    entry = tree_entry(
        git(repo_root, "ls-files", "--stage", "-z", "--", path),
        source=f"git ls-files for {path}",
    )
    if entry is None:
        raise VerifiedCommitError(f"staged path is missing from the Git index: {path}")
    return entry


def head_tree_entry(
    repo_root: pathlib.Path, head: str, path: str
) -> tuple[str, str] | None:
    return tree_entry(
        git(repo_root, "ls-tree", "-z", head, "--", path),
        source=f"git ls-tree for {path}",
    )


def validate_file_mode(
    path: str,
    *,
    old_entry: tuple[str, str] | None,
    new_entry: tuple[str, str],
) -> None:
    new_mode, _new_oid = new_entry
    if old_entry is None:
        if new_mode != "100644":
            raise VerifiedCommitError(
                f"new path has an unsupported Git mode ({new_mode}): {path}"
            )
        return
    old_mode, _old_oid = old_entry
    if old_mode != new_mode:
        raise VerifiedCommitError(
            f"staged file mode changes are not supported ({old_mode} -> {new_mode}): {path}"
        )
    if new_mode not in SUPPORTED_EXISTING_MODES:
        raise VerifiedCommitError(
            f"path has an unsupported Git mode ({new_mode}): {path}"
        )


def collect_staged_changes(repo_root: pathlib.Path, head: str) -> StagedChanges:
    unmerged = git(repo_root, "ls-files", "--unmerged", "-z")
    if unmerged:
        raise VerifiedCommitError("the Git index contains unresolved merge conflicts")

    additions = nul_paths(
        git(
            repo_root,
            "diff",
            "--cached",
            "--name-only",
            "-z",
            "--no-renames",
            "--diff-filter=ACMRTUXB",
            head,
            "--",
        )
    )
    deletions = nul_paths(
        git(
            repo_root,
            "diff",
            "--cached",
            "--name-only",
            "-z",
            "--no-renames",
            "--diff-filter=D",
            head,
            "--",
        )
    )
    if not additions and not deletions:
        raise VerifiedCommitError("there are no staged changes to publish")

    encoded_additions: list[dict[str, str]] = []
    for path in additions:
        new_entry = staged_index_entry(repo_root, path)
        validate_file_mode(
            path,
            old_entry=head_tree_entry(repo_root, head, path),
            new_entry=new_entry,
        )
        _mode, blob_oid = new_entry
        contents = git(repo_root, "cat-file", "blob", blob_oid)
        encoded_additions.append(
            {
                "path": path,
                "contents": base64.b64encode(contents).decode("ascii"),
            }
        )

    tree_oid = display_output(git(repo_root, "write-tree"))
    return StagedChanges(
        additions=encoded_additions,
        deletions=[{"path": path} for path in deletions],
        tree_oid=tree_oid,
    )


def branch_head(repository: str, branch: str) -> str | None:
    encoded_branch = urllib.parse.quote(branch, safe="/")
    response = gh_api(
        f"repos/{repository}/git/ref/heads/{encoded_branch}", allow_not_found=True
    )
    if response is None:
        return None
    sha = response.get("object", {}).get("sha")
    if not isinstance(sha, str):
        raise VerifiedCommitError("GitHub branch response did not contain a commit SHA")
    return sha


def local_head_is_remote(repo_root: pathlib.Path, remote: str, head: str) -> bool:
    result = run_command(
        [
            "git",
            "for-each-ref",
            "--format=%(refname)",
            "--contains",
            head,
            f"refs/remotes/{remote}/",
        ],
        cwd=repo_root,
    )
    refs = require_success(result, "could not inspect remote-tracking refs")
    return bool(refs.strip())


def ensure_remote_branch(
    repo_root: pathlib.Path,
    repository: str,
    branch: str,
    expected_head: str,
    remote: str,
) -> None:
    current_head = branch_head(repository, branch)
    if current_head is None:
        if not local_head_is_remote(repo_root, remote, expected_head):
            raise VerifiedCommitError(
                "the local HEAD is not present on a remote-tracking ref; "
                "do not publish local-only commits"
            )
        gh_api(
            f"repos/{repository}/git/refs",
            method="POST",
            payload={"ref": f"refs/heads/{branch}", "sha": expected_head},
        )
        return
    if current_head != expected_head:
        raise VerifiedCommitError(
            f"remote branch head changed: expected {expected_head}, found {current_head}"
        )


def create_verified_commit(
    repository: str,
    branch: str,
    expected_head: str,
    message: str,
    body: str,
    changes: StagedChanges,
) -> dict[str, Any]:
    file_changes: dict[str, list[dict[str, str]]] = {}
    if changes.additions:
        file_changes["additions"] = changes.additions
    if changes.deletions:
        file_changes["deletions"] = changes.deletions
    response = gh_api(
        "graphql",
        method="POST",
        payload={
            "query": CREATE_COMMIT_MUTATION,
            "variables": {
                "input": {
                    "branch": {
                        "repositoryNameWithOwner": repository,
                        "branchName": branch,
                    },
                    "message": {"headline": message, "body": body},
                    "expectedHeadOid": expected_head,
                    "fileChanges": file_changes,
                }
            },
        },
    )
    if response is None:
        raise VerifiedCommitError("GitHub returned an empty commit response")
    errors = response.get("errors")
    if errors:
        raise VerifiedCommitError(f"GitHub rejected the commit: {errors}")
    commit = response.get("data", {}).get("createCommitOnBranch", {}).get("commit", {})
    oid = commit.get("oid")
    if not isinstance(oid, str):
        raise VerifiedCommitError("GitHub response did not contain the new commit SHA")
    return {"oid": oid, "url": commit.get("url")}


def verify_commit(repository: str, oid: str, expected_tree: str) -> dict[str, Any]:
    response = gh_api(f"repos/{repository}/commits/{oid}")
    if response is None:
        raise VerifiedCommitError("GitHub returned an empty verification response")
    verification = response.get("commit", {}).get("verification", {})
    if verification.get("verified") is not True:
        reason = verification.get("reason", "unknown")
        raise VerifiedCommitError(f"GitHub did not verify commit {oid}: {reason}")
    remote_tree = response.get("commit", {}).get("tree", {}).get("sha")
    if remote_tree != expected_tree:
        raise VerifiedCommitError(
            f"GitHub created tree {remote_tree}, but the staged Git index is {expected_tree}"
        )
    return {
        "verified": True,
        "verification_reason": verification.get("reason"),
        "tree_oid": remote_tree,
    }


def sync_local_branch(
    repo_root: pathlib.Path,
    remote: str,
    branch: str,
    expected_head: str,
    oid: str,
) -> tuple[bool, str | None]:
    remote_ref = f"refs/remotes/{remote}/{branch}"
    fetch = run_command(
        ["git", "fetch", remote, f"refs/heads/{branch}:{remote_ref}"], cwd=repo_root
    )
    if fetch.returncode != 0:
        return False, display_output(fetch.stderr) or "git fetch failed"
    local_ref = display_output(git(repo_root, "symbolic-ref", "--quiet", "HEAD"))
    update = run_command(
        ["git", "update-ref", local_ref, oid, expected_head], cwd=repo_root
    )
    if update.returncode != 0:
        return False, display_output(update.stderr) or "git update-ref failed"
    return True, None


def publish_verified_commit(
    repo_root: pathlib.Path,
    *,
    repository: str,
    branch: str,
    remote: str,
    message: str,
    body: str,
) -> dict[str, Any]:
    validate_repository(repository)
    require_success(
        run_command(["git", "check-ref-format", "--branch", branch], cwd=repo_root),
        "branch is not a valid Git ref name",
    )
    expected_head = display_output(git(repo_root, "rev-parse", "HEAD"))
    changes = collect_staged_changes(repo_root, expected_head)
    ensure_remote_branch(repo_root, repository, branch, expected_head, remote)
    created = create_verified_commit(
        repository,
        branch,
        expected_head,
        message,
        body,
        changes,
    )
    oid = created["oid"]
    verification = verify_commit(repository, oid, changes.tree_oid)
    final_head = branch_head(repository, branch)
    if final_head != oid:
        return {
            "repository": repository,
            "branch": branch,
            "oid": oid,
            "url": created.get("url"),
            **verification,
            "local_synced": False,
            "local_sync_error": f"remote branch advanced to {final_head}",
        }
    local_synced, local_sync_error = sync_local_branch(
        repo_root, remote, branch, expected_head, oid
    )
    return {
        "repository": repository,
        "branch": branch,
        "oid": oid,
        "url": created.get("url"),
        **verification,
        "local_synced": local_synced,
        "local_sync_error": local_sync_error,
    }


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Publish staged changes as a GitHub-authored, verified commit."
    )
    parser.add_argument(
        "-m", "--message", required=True, help="commit message headline"
    )
    parser.add_argument("--body", default="", help="optional commit message body")
    parser.add_argument(
        "--repository", help="GitHub owner/name; derived from the remote"
    )
    parser.add_argument(
        "--branch", help="remote branch; defaults to the current branch"
    )
    parser.add_argument(
        "--remote", default="origin", help="Git remote name (default: origin)"
    )
    parser.add_argument(
        "--repo-root", default=".", help="working tree path (default: current)"
    )
    return parser


def main() -> int:
    args = build_parser().parse_args()
    try:
        initial_root = pathlib.Path(args.repo_root).resolve()
        repo_root = pathlib.Path(
            display_output(git(initial_root, "rev-parse", "--show-toplevel"))
        )
        branch = args.branch or display_output(
            git(repo_root, "symbolic-ref", "--quiet", "--short", "HEAD")
        )
        repository = args.repository
        if repository is None:
            remote_url = display_output(
                git(repo_root, "config", "--get", f"remote.{args.remote}.url")
            )
            repository = repository_from_remote(remote_url)
        result = publish_verified_commit(
            repo_root,
            repository=repository,
            branch=branch,
            remote=args.remote,
            message=args.message,
            body=args.body,
        )
    except VerifiedCommitError as error:
        print(f"verified commit failed: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
