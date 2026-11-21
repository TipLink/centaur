#!/usr/bin/env python3
"""Keep configured GitHub repositories synced into the Centaur repo cache."""

from __future__ import annotations

import fcntl
import glob
import os
import shlex
import shutil
import signal
import stat
import subprocess
import sys
import time
from contextlib import contextmanager, suppress
from datetime import datetime, timezone
from itertools import chain
from pathlib import Path

REPOSITORY_VISIBILITIES = {"private", "public"}
PUBLIC_REPOSITORY_VISIBILITY = "public"
PRIVATE_REPOSITORY_VISIBILITY = "private"
GIT_LOCK_NAMES = (
    "index.lock",
    "HEAD.lock",
    "packed-refs.lock",
    "shallow.lock",
    "config.lock",
    "ORIG_HEAD.lock",
    "FETCH_HEAD.lock",
)
# Leave time for kubelet's default 30-second termination grace.
GIT_SHUTDOWN_GRACE_SECONDS = 5
STOP_POLL_SECONDS = 0.25


class ShutdownRequested(Exception):
    pass


class CacheWriterBusy(Exception):
    pass


class GitError(RuntimeError):
    def __init__(self, label: str, stderr: str) -> None:
        self.stderr = stderr.strip()
        super().__init__(
            f"{label} failed: {self.stderr}" if self.stderr else f"{label} failed"
        )

    @property
    def corrupt_index(self) -> bool:
        return any(
            message in self.stderr
            for message in (
                "index file smaller than expected",
                "index file corrupt",
                "index file sha1 mismatch",
            )
        )


def _split_words(value: str) -> list[str]:
    return [part for part in value.split() if part]


def _repository_refs(value: str) -> dict[str, str]:
    refs = {}
    for entry in _split_words(value):
        if "=" not in entry:
            continue
        repo, ref = entry.split("=", 1)
        if repo and ref:
            refs[repo] = ref
    return refs


def _normalize_repository_visibility(value: str | None) -> str:
    visibility = (value or "").strip().lower()
    return visibility if visibility in REPOSITORY_VISIBILITIES else "private"


def _repository_visibilities(value: str, repositories: list[str]) -> dict[str, str]:
    visibilities = {repo: "private" for repo in repositories}
    for entry in _split_words(value):
        if "=" not in entry:
            continue
        repo, visibility = entry.split("=", 1)
        if repo:
            visibilities[repo] = _normalize_repository_visibility(visibility)
    return visibilities


def _atomic_write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(f"{path.name}.tmp")
    tmp.write_text(content)
    tmp.replace(path)


def _remove_path(path: Path) -> None:
    if path.is_dir() and not path.is_symlink():
        shutil.rmtree(path)
    elif path.exists() or path.is_symlink():
        path.unlink()


def _relative_symlink_target(source: Path, link: Path) -> str:
    return os.path.relpath(source, start=link.parent)


class RepoCacheSync:
    def __init__(
        self,
        *,
        cache_dir: Path,
        repositories: list[str],
        repository_refs: dict[str, str],
        repository_visibilities: dict[str, str],
        sync_interval_seconds: float,
        github_token_file: Path,
    ) -> None:
        self.cache_dir = cache_dir
        self.repositories = repositories
        self.repository_refs = repository_refs
        self.repository_visibilities = repository_visibilities
        self.sync_interval_seconds = sync_interval_seconds
        self.github_token_file = github_token_file
        self.git_env: dict[str, str] | None = None
        self.ready_file = self.cache_dir / ".repo-cache-ready"
        self.shutdown_requested = False
        self.writer_fd: int | None = None

    def request_shutdown(self, _signum: int, _frame: object) -> None:
        self.shutdown_requested = True

    def _check_shutdown(self) -> None:
        if self.shutdown_requested:
            raise ShutdownRequested()

    @contextmanager
    def _cache_writer(self):
        # Visibility migrations and the readiness marker span multiple checkouts.
        # All writers therefore lock the same stable inode, outside every clone.
        self.cache_dir.mkdir(parents=True, exist_ok=True)
        path = self.cache_dir / ".repo-cache-sync.lock"
        fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
        try:
            if not stat.S_ISREG(os.fstat(fd).st_mode):
                raise RuntimeError(f"Expected a regular repo-cache writer lock: {path}")
            # A read-only cache mount still permits flock on a readable file.
            os.fchmod(fd, 0o600)
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError as exc:
                raise CacheWriterBusy("repo-cache writer already active") from exc
            self.writer_fd = fd
            try:
                yield
            finally:
                self.writer_fd = None
        finally:
            os.close(fd)

    def _recover_git_locks(self, target: Path) -> None:
        git_dir = target / ".git"
        if target.is_symlink() or git_dir.is_symlink():
            raise RuntimeError(
                f"Refusing symlinked cache checkout or Git directory: {target}"
            )
        if not git_dir.exists():
            return
        if not git_dir.is_dir():
            raise RuntimeError(f"Expected a cache-owned Git directory: {git_dir}")
        refs = git_dir / "refs"
        if refs.is_symlink():
            raise RuntimeError(f"Refusing symlinked Git refs: {refs}")
        # rglob does not descend into symlinked subdirectories of refs.
        for lock in chain(
            (git_dir / name for name in GIT_LOCK_NAMES), refs.rglob("*.lock")
        ):
            self._check_shutdown()
            try:
                mode = lock.lstat().st_mode
            except FileNotFoundError:
                continue
            if not stat.S_ISREG(mode):
                raise RuntimeError(f"Refusing to remove non-regular Git lock: {lock}")
            lock.unlink()
            print(f"Recovered stale Git lock: {lock}", file=sys.stderr, flush=True)

    def repository_visibility(self, repo: str) -> str:
        return _normalize_repository_visibility(self.repository_visibilities.get(repo))

    def repository_target(self, repo: str) -> Path:
        return self.cache_dir / self.repository_visibility(repo) / repo

    def legacy_repository_path(self, repo: str) -> Path:
        return self.cache_dir / repo

    def alternate_repository_target(self, repo: str) -> Path:
        visibility = self.repository_visibility(repo)
        alternate = (
            PRIVATE_REPOSITORY_VISIBILITY
            if visibility == PUBLIC_REPOSITORY_VISIBILITY
            else PUBLIC_REPOSITORY_VISIBILITY
        )
        return self.cache_dir / alternate / repo

    def migrate_existing_checkout(self, repo: str, target: Path) -> None:
        if (target / ".git").is_dir():
            return
        for candidate in [
            self.legacy_repository_path(repo),
            self.alternate_repository_target(repo),
        ]:
            if candidate == target or candidate.is_symlink() or not (candidate / ".git").is_dir():
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            _remove_path(target)
            candidate.replace(target)
            return

    def update_legacy_link(self, repo: str, target: Path) -> None:
        link = self.legacy_repository_path(repo)
        if link.is_symlink():
            current = os.readlink(link)
            expected = _relative_symlink_target(target, link)
            if current == expected:
                return
            link.unlink()
        elif link.exists():
            _remove_path(link)
        link.parent.mkdir(parents=True, exist_ok=True)
        link.symlink_to(_relative_symlink_target(target, link))

    def remove_stale_visibility_target(self, repo: str) -> None:
        stale = self.alternate_repository_target(repo)
        if stale.exists() or stale.is_symlink():
            _remove_path(stale)

    @classmethod
    def from_env(cls) -> RepoCacheSync:
        interval = os.environ.get("SYNC_INTERVAL_SECONDS", "").strip()
        try:
            sync_interval_seconds = float(interval) if interval else 30.0
        except ValueError:
            sync_interval_seconds = 30.0
        if sync_interval_seconds <= 0:
            sync_interval_seconds = 30.0
        repositories = _split_words(os.environ.get("REPOSITORIES", ""))

        return cls(
            cache_dir=Path(os.environ.get("REPO_CACHE_DIR", "/cache")),
            repositories=repositories,
            repository_refs=_repository_refs(os.environ.get("REPOSITORY_REFS", "")),
            repository_visibilities=_repository_visibilities(
                os.environ.get("REPOSITORY_VISIBILITIES", ""), repositories
            ),
            sync_interval_seconds=sync_interval_seconds,
            github_token_file=Path(
                os.environ.get("GITHUB_TOKEN_FILE", "/github-token/token")
            ),
        )

    def _git_env(self) -> dict[str, str]:
        env = os.environ.copy()
        env["GIT_TERMINAL_PROMPT"] = "0"
        env["LC_ALL"] = "C"
        # The env is built once per process, so the token check lives in the
        # script: a token Secret that is filled or rotated after start is used
        # on the next fetch without restarting repo-cache.
        token_file = shlex.quote(str(self.github_token_file))
        askpass = Path("/tmp/git-askpass")
        askpass.write_text(
            "#!/bin/sh\n"
            'case "$1" in\n'
            "  *Username*) printf '%s\\n' x-access-token ;;\n"
            f"  *Password*) if [ -s {token_file} ]; then cat {token_file}; "
            "else printf '\\n'; fi ;;\n"
            "  *) printf '\\n' ;;\n"
            "esac\n"
        )
        askpass.chmod(0o700)
        env["GIT_ASKPASS"] = str(askpass)
        return env

    def configure_git(self) -> None:
        self._run_git(
            ["config", "--global", "--add", "safe.directory", "*"], "git safe.directory"
        )
        self._run_git(
            ["config", "--global", "init.defaultBranch", "main"],
            "git init.defaultBranch",
        )

    def _run_git(self, args: list[str], label: str) -> subprocess.CompletedProcess[str]:
        self._check_shutdown()
        if self.git_env is None:
            self.git_env = self._git_env()
        with subprocess.Popen(
            ["git", *args],
            text=True,
            env=self.git_env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
            # If PID 1 dies mid-write, the surviving Git writer still holds the flock.
            pass_fds=() if self.writer_fd is None else (self.writer_fd,),
        ) as process:
            while not self.shutdown_requested:
                try:
                    stdout, stderr = process.communicate(timeout=STOP_POLL_SECONDS)
                    if self.shutdown_requested:
                        break
                    if process.returncode:
                        raise GitError(label, stderr)
                    return subprocess.CompletedProcess(process.args, 0, stdout, stderr)
                except subprocess.TimeoutExpired:
                    continue
            # Git can spawn helpers which keep writing or hold its output pipes.
            with suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGTERM)
            with suppress(subprocess.TimeoutExpired):
                process.communicate(timeout=GIT_SHUTDOWN_GRACE_SECONDS)
            with suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGKILL)
            # A detached helper can keep a pipe open beyond the pod grace period.
            # Reap Git itself; Popen closes our pipe ends on exit.
            process.wait()
            raise ShutdownRequested()

    def _git_output(self, repo_path: Path, *args: str) -> str | None:
        try:
            result = self._run_git(["-C", str(repo_path), *args], "git")
        except RuntimeError:
            return None
        return result.stdout.strip() or None

    def _git_ok(self, repo_path: Path, *args: str) -> bool:
        try:
            self._run_git(["-C", str(repo_path), *args], "git")
        except RuntimeError:
            return False
        return True

    def checkout_repo(self, repo: str, target: Path) -> None:
        requested_ref = self.repository_refs.get(repo)
        if requested_ref:
            for ref in (f"origin/{requested_ref}", requested_ref):
                if self._git_ok(
                    target, "rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}"
                ):
                    break
            else:
                self._run_git(
                    [
                        "-C",
                        str(target),
                        "-c",
                        "gc.auto=0",
                        "fetch",
                        "--force",
                        "--prune",
                        "--tags",
                        "origin",
                        requested_ref,
                    ],
                    f"fetch {repo}@{requested_ref}",
                )
                ref = "FETCH_HEAD"
            checkout = ["--detach", ref]
        else:
            default_branch = self._git_output(
                target, "symbolic-ref", "--short", "refs/remotes/origin/HEAD"
            )
            if not default_branch or default_branch == "(unknown)":
                default_branch = "main"
            default_branch = default_branch.removeprefix("origin/")
            ref = f"origin/{default_branch}"
            checkout = ["-B", default_branch, ref]

        self._run_git(
            ["-C", str(target), "checkout", "-q", "-f", *checkout],
            f"checkout {repo}@{ref}",
        )

    def _refresh_checkout(self, repo: str, target: Path) -> None:
        self._run_git(
            [
                "-C",
                str(target),
                "-c",
                "gc.auto=0",
                "fetch",
                "--force",
                "--prune",
                "--tags",
                "origin",
            ],
            f"fetch {repo}",
        )
        self._git_ok(target, "remote", "set-head", "origin", "-a")
        self.checkout_repo(repo, target)
        self._run_git(["-C", str(target), "clean", "-fd"], f"clean {repo}")

    def sync_repo(self, repo: str) -> None:
        with self._cache_writer():
            _remove_path(self.ready_file)
            self._sync_repo(repo)

    def _sync_repo(self, repo: str) -> None:
        self._check_shutdown()
        repo_url = f"https://github.com/{repo}.git"
        target = self.repository_target(repo)
        tmp = target.with_name(f"{target.name}.tmp")
        target.parent.mkdir(parents=True, exist_ok=True)
        self.migrate_existing_checkout(repo, target)
        self._recover_git_locks(target)

        existing = (target / ".git").is_dir() and self._git_ok(
            target, "rev-parse", "--git-dir"
        )
        if existing:
            print(f"Updating {repo}", flush=True)
            if not self._git_ok(target, "remote", "set-url", "origin", repo_url):
                self._run_git(
                    ["-C", str(target), "remote", "add", "origin", repo_url],
                    f"set origin for {repo}",
                )
        else:
            print(f"Cloning {repo}", flush=True)
            for stale_tmp in glob.glob(f"{target}.tmp*"):
                _remove_path(Path(stale_tmp))
            _remove_path(target)
            self._run_git(["clone", "--quiet", repo_url, str(tmp)], f"clone {repo}")

        checkout = target if existing else tmp
        self._git_ok(checkout, "config", "gc.auto", "0")
        try:
            self._refresh_checkout(repo, checkout)
        except GitError as exc:
            if not exc.corrupt_index:
                raise
            # Fetch can read the index too. Confirm the corruption locally so a
            # remote diagnostic can never trigger deletion of a healthy index.
            try:
                self._run_git(
                    ["-C", str(checkout), "ls-files", "--", ".git/index"], "check index"
                )
            except GitError as local_error:
                index = checkout / ".git/index"
                if not local_error.corrupt_index or not stat.S_ISREG(
                    index.lstat().st_mode
                ):
                    raise
                index.unlink()
                # Restore the old tracked set even if the new revision deletes
                # a path and begins ignoring it. Checkout will then remove it.
                self._run_git(["-C", str(checkout), "read-tree", "HEAD"], "rebuild index")
            else:
                raise exc
            self._refresh_checkout(repo, checkout)
        if not existing:
            tmp.replace(target)
        self.remove_stale_visibility_target(repo)
        self.update_legacy_link(repo, target)

    def repository_fingerprint(self) -> str:
        refs = " ".join(f"{repo}={ref}" for repo, ref in self.repository_refs.items())
        visibilities = " ".join(
            f"{repo}={self.repository_visibilities.get(repo, 'private')}"
            for repo in self.repositories
        )
        return (
            f"repositories={' '.join(self.repositories)}\n"
            f"repository_refs={refs}\n"
            f"repository_visibilities={visibilities}\n"
        )

    def write_ready(self) -> None:
        synced_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        _atomic_write(
            self.ready_file, f"{self.repository_fingerprint()}synced_at={synced_at}\n"
        )

    def check_ready(self) -> int:
        try:
            ready_lines = self.ready_file.read_text().splitlines()
        except OSError:
            return 1

        expected_lines = self.repository_fingerprint().splitlines()
        if ready_lines[: len(expected_lines)] != expected_lines:
            return 1
        for repo in self.repositories:
            if not (self.repository_target(repo) / ".git").is_dir():
                return 1
            if not self.legacy_repository_path(repo).is_symlink():
                return 1
        return 0

    def sync_once(self) -> bool:
        with self._cache_writer():
            return self._sync_once()

    def _sync_once(self) -> bool:
        sync_ok = True
        for repo in self.repositories:
            try:
                self._sync_repo(repo)
            except ShutdownRequested:
                _remove_path(self.ready_file)
                raise
            except Exception as exc:
                print(f"Failed to sync {repo}: {exc}", file=sys.stderr, flush=True)
                sync_ok = False
        if sync_ok and not self.shutdown_requested:
            self.write_ready()
        else:
            _remove_path(self.ready_file)
        return sync_ok and not self.shutdown_requested

    def run_forever(self) -> int:
        os.umask(0o022)
        if not self.repositories:
            print(
                "No repositories configured for repo-cache", file=sys.stderr, flush=True
            )
            return 0
        try:
            with self._cache_writer():
                _remove_path(self.ready_file)
                try:
                    self.configure_git()
                    while not self.shutdown_requested:
                        self._sync_once()
                        wake_at = time.monotonic() + self.sync_interval_seconds
                        while not self.shutdown_requested:
                            remaining = wake_at - time.monotonic()
                            if remaining <= 0:
                                break
                            time.sleep(min(STOP_POLL_SECONDS, remaining))
                finally:
                    _remove_path(self.ready_file)
        except CacheWriterBusy as exc:
            print(exc, file=sys.stderr, flush=True)
            return 1
        except ShutdownRequested:
            pass
        return 0


def main() -> int:
    sync = RepoCacheSync.from_env()
    if "--check-ready" in sys.argv[1:]:
        return sync.check_ready()
    previous = {
        sig: signal.signal(sig, sync.request_shutdown)
        for sig in (signal.SIGTERM, signal.SIGINT)
    }
    try:
        return sync.run_forever()
    finally:
        for sig, handler in previous.items():
            signal.signal(sig, handler)


if __name__ == "__main__":
    raise SystemExit(main())
