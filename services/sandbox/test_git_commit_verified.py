from __future__ import annotations

import base64
import pathlib
import subprocess

import pytest

from services.sandbox import git_commit_verified as workflow


def git(repo: pathlib.Path, *args: str) -> str:
    result = subprocess.run(
        ["git", *args], cwd=repo, text=True, capture_output=True, check=True
    )
    return result.stdout.strip()


def repository(tmp_path: pathlib.Path) -> pathlib.Path:
    git(tmp_path, "init", "-b", "main")
    git(tmp_path, "config", "user.name", "Test")
    git(tmp_path, "config", "user.email", "test@example.com")
    (tmp_path / "keep.txt").write_text("before\n")
    (tmp_path / "delete.txt").write_text("delete me\n")
    (tmp_path / "old.txt").write_text("rename me\n")
    git(tmp_path, "add", ".")
    git(tmp_path, "commit", "-m", "base")
    head = git(tmp_path, "rev-parse", "HEAD")
    git(tmp_path, "update-ref", "refs/remotes/origin/main", head)
    git(tmp_path, "switch", "-c", "feature/verified")
    return tmp_path


def test_collect_staged_changes_uses_index_and_supports_deletes_and_renames(
    tmp_path: pathlib.Path,
) -> None:
    repo = repository(tmp_path)
    (repo / "keep.txt").write_text("staged\n")
    git(repo, "rm", "delete.txt")
    git(repo, "mv", "old.txt", "new.txt")
    git(repo, "add", "keep.txt")
    (repo / "keep.txt").write_text("unstaged\n")

    changes = workflow.collect_staged_changes(repo, git(repo, "rev-parse", "HEAD"))

    additions = {
        change["path"]: base64.b64decode(change["contents"])
        for change in changes.additions
    }
    assert additions == {"keep.txt": b"staged\n", "new.txt": b"rename me\n"}
    assert changes.deletions == [{"path": "delete.txt"}, {"path": "old.txt"}]
    assert changes.tree_oid == git(repo, "write-tree")


@pytest.mark.parametrize("mode", ["symlink", "executable"])
def test_collect_staged_changes_rejects_unsupported_new_modes(
    tmp_path: pathlib.Path, mode: str
) -> None:
    repo = repository(tmp_path)
    path = repo / "new-file"
    if mode == "symlink":
        path.symlink_to("keep.txt")
    else:
        path.write_text("#!/bin/sh\n")
        path.chmod(0o755)
    git(repo, "add", "new-file")

    with pytest.raises(workflow.VerifiedCommitError, match="unsupported Git mode"):
        workflow.collect_staged_changes(repo, git(repo, "rev-parse", "HEAD"))


def test_repository_from_remote_supports_https_and_ssh() -> None:
    assert (
        workflow.repository_from_remote("https://github.com/TipLink/centaur.git")
        == "TipLink/centaur"
    )
    assert (
        workflow.repository_from_remote("git@github.com:TipLink/centaur.git")
        == "TipLink/centaur"
    )


def test_publish_creates_remote_branch_and_verifies_exact_tree(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    repo = repository(tmp_path)
    (repo / "new.txt").write_text("new\n")
    git(repo, "add", "new.txt")
    head = git(repo, "rev-parse", "HEAD")
    tree = git(repo, "write-tree")
    new_oid = "b" * 40
    heads = iter([None, new_oid])
    api_calls: list[tuple[str, str, dict | None]] = []

    monkeypatch.setattr(workflow, "branch_head", lambda repository, branch: next(heads))

    def fake_gh_api(endpoint, *, method="GET", payload=None, allow_not_found=False):
        api_calls.append((endpoint, method, payload))
        if endpoint == "graphql":
            return {
                "data": {
                    "createCommitOnBranch": {
                        "commit": {"oid": new_oid, "url": "https://example.test/commit"}
                    }
                }
            }
        if endpoint.endswith(f"commits/{new_oid}"):
            return {
                "commit": {
                    "tree": {"sha": tree},
                    "verification": {"verified": True, "reason": "valid"},
                }
            }
        return {"ref": "refs/heads/feature/verified"}

    monkeypatch.setattr(workflow, "gh_api", fake_gh_api)
    monkeypatch.setattr(workflow, "sync_local_branch", lambda *args: (True, None))

    result = workflow.publish_verified_commit(
        repo,
        repository="TipLink/centaur",
        branch="feature/verified",
        remote="origin",
        message="test: verified",
        body="",
    )

    assert result["oid"] == new_oid
    assert result["verified"] is True
    assert result["local_synced"] is True
    assert api_calls[0] == (
        "repos/TipLink/centaur/git/refs",
        "POST",
        {"ref": "refs/heads/feature/verified", "sha": head},
    )
    graphql_payload = api_calls[1][2]
    assert graphql_payload is not None
    commit_input = graphql_payload["variables"]["input"]
    assert commit_input["expectedHeadOid"] == head
    assert commit_input["fileChanges"]["additions"][0]["path"] == "new.txt"


def test_publish_rejects_a_stale_remote_head(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    repo = repository(tmp_path)
    (repo / "new.txt").write_text("new\n")
    git(repo, "add", "new.txt")
    monkeypatch.setattr(workflow, "branch_head", lambda repository, branch: "c" * 40)

    with pytest.raises(
        workflow.VerifiedCommitError, match="remote branch head changed"
    ):
        workflow.publish_verified_commit(
            repo,
            repository="TipLink/centaur",
            branch="feature/verified",
            remote="origin",
            message="test: verified",
            body="",
        )


def test_verify_commit_rejects_unverified_or_mismatched_commits(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(
        workflow,
        "gh_api",
        lambda endpoint: {
            "commit": {
                "tree": {"sha": "tree"},
                "verification": {"verified": False, "reason": "unsigned"},
            }
        },
    )
    with pytest.raises(workflow.VerifiedCommitError, match="unsigned"):
        workflow.verify_commit("TipLink/centaur", "a" * 40, "tree")

    monkeypatch.setattr(
        workflow,
        "gh_api",
        lambda endpoint: {
            "commit": {
                "tree": {"sha": "different"},
                "verification": {"verified": True, "reason": "valid"},
            }
        },
    )
    with pytest.raises(workflow.VerifiedCommitError, match="staged Git index"):
        workflow.verify_commit("TipLink/centaur", "a" * 40, "tree")


def test_new_branch_rejects_local_only_history(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    repo = repository(tmp_path)
    (repo / "local.txt").write_text("local commit\n")
    git(repo, "add", "local.txt")
    git(repo, "commit", "-m", "local only")
    (repo / "new.txt").write_text("new\n")
    git(repo, "add", "new.txt")
    monkeypatch.setattr(workflow, "branch_head", lambda repository, branch: None)

    with pytest.raises(workflow.VerifiedCommitError, match="local-only commits"):
        workflow.publish_verified_commit(
            repo,
            repository="TipLink/centaur",
            branch="feature/verified",
            remote="origin",
            message="test: verified",
            body="",
        )
