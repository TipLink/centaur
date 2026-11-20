from __future__ import annotations

import errno
import fcntl
import os
import signal
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

import repo_cache_sync


class RepoCacheSyncTest(unittest.TestCase):
    def test_repository_refs_parse_nonempty_entries(self) -> None:
        self.assertEqual(
            repo_cache_sync._repository_refs("acme/one=main bad acme/two=abc123"),
            {"acme/one": "main", "acme/two": "abc123"},
        )

    def test_repository_visibilities_default_invalid_values_to_private(self) -> None:
        self.assertEqual(
            repo_cache_sync._repository_visibilities(
                "acme/public=public acme/private=private acme/typo=internal",
                ["acme/public", "acme/private", "acme/missing", "acme/typo"],
            ),
            {
                "acme/public": "public",
                "acme/private": "private",
                "acme/missing": "private",
                "acme/typo": "private",
            },
        )

    def test_from_env_loads_repository_visibilities(self) -> None:
        old_env = os.environ.copy()
        try:
            os.environ.update(
                {
                    "REPOSITORIES": "acme/public acme/private",
                    "REPOSITORY_VISIBILITIES": "acme/public=public acme/private=bogus",
                    "SYNC_INTERVAL_SECONDS": "10",
                }
            )

            sync = repo_cache_sync.RepoCacheSync.from_env()

            self.assertEqual(
                sync.repository_visibilities,
                {"acme/public": "public", "acme/private": "private"},
            )
        finally:
            os.environ.clear()
            os.environ.update(old_env)

    def test_write_ready_preserves_readiness_format(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)

            sync = repo_cache_sync.RepoCacheSync(
                cache_dir=root / "cache",
                repositories=["acme/centaur"],
                repository_refs={"acme/centaur": "main"},
                repository_visibilities={"acme/centaur": "public"},
                sync_interval_seconds=30,
                github_token_file=root / "missing-token",
            )

            sync.write_ready()

            lines = (root / "cache" / ".repo-cache-ready").read_text().splitlines()
            self.assertEqual(lines[0], "repositories=acme/centaur")
            self.assertEqual(lines[1], "repository_refs=acme/centaur=main")
            self.assertEqual(lines[2], "repository_visibilities=acme/centaur=public")
            self.assertRegex(lines[3], r"^synced_at=\d{4}-\d{2}-\d{2}T")

    def test_check_ready_validates_fingerprint_and_repos(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo_path = root / "cache" / "private" / "acme" / "centaur" / ".git"
            repo_path.mkdir(parents=True)
            (root / "cache" / "acme").mkdir()
            (root / "cache" / "acme" / "centaur").symlink_to("../private/acme/centaur")
            sync = repo_cache_sync.RepoCacheSync(
                cache_dir=root / "cache",
                repositories=["acme/centaur"],
                repository_refs={"acme/centaur": "main"},
                repository_visibilities={"acme/centaur": "private"},
                sync_interval_seconds=30,
                github_token_file=root / "missing-token",
            )
            sync.write_ready()

            self.assertEqual(sync.check_ready(), 0)
            (root / "cache" / ".repo-cache-ready").write_text(
                "repositories=wrong\n"
                "repository_refs=acme/centaur=main\n"
                "repository_visibilities=acme/centaur=private\n"
            )
            self.assertEqual(sync.check_ready(), 1)

    def test_repository_targets_use_visibility_projection(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            sync = repo_cache_sync.RepoCacheSync(
                cache_dir=root / "cache",
                repositories=["acme/public", "acme/private"],
                repository_refs={},
                repository_visibilities={
                    "acme/public": "public",
                    "acme/private": "private",
                },
                sync_interval_seconds=30,
                github_token_file=root / "missing-token",
            )

            self.assertEqual(
                sync.repository_target("acme/public"),
                root / "cache" / "public" / "acme" / "public",
            )
            self.assertEqual(
                sync.repository_target("acme/private"),
                root / "cache" / "private" / "acme" / "private",
            )

    def test_legacy_link_points_to_visibility_projection(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            target = root / "cache" / "public" / "acme" / "docs"
            (target / ".git").mkdir(parents=True)
            sync = repo_cache_sync.RepoCacheSync(
                cache_dir=root / "cache",
                repositories=["acme/docs"],
                repository_refs={},
                repository_visibilities={"acme/docs": "public"},
                sync_interval_seconds=30,
                github_token_file=root / "missing-token",
            )

            sync.update_legacy_link("acme/docs", target)

            link = root / "cache" / "acme" / "docs"
            self.assertTrue(link.is_symlink())
            self.assertEqual(link.resolve(), target.resolve())

    def test_migrate_existing_checkout_moves_old_root_to_projection(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            old = root / "cache" / "acme" / "docs"
            (old / ".git").mkdir(parents=True)
            sync = repo_cache_sync.RepoCacheSync(
                cache_dir=root / "cache",
                repositories=["acme/docs"],
                repository_refs={},
                repository_visibilities={"acme/docs": "public"},
                sync_interval_seconds=30,
                github_token_file=root / "missing-token",
            )

            target = sync.repository_target("acme/docs")
            sync.migrate_existing_checkout("acme/docs", target)

            self.assertTrue((target / ".git").is_dir())
            self.assertFalse(old.exists())

    def test_git_askpass_reads_token_written_after_env_is_built(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            token_file = Path(tmp) / "token"
            sync = repo_cache_sync.RepoCacheSync(
                cache_dir=Path(tmp) / "cache",
                repositories=["acme/private"],
                repository_refs={},
                repository_visibilities={"acme/private": "private"},
                sync_interval_seconds=30,
                github_token_file=token_file,
            )
            askpass = sync._git_env()["GIT_ASKPASS"]

            def password() -> str:
                return subprocess.run(
                    [askpass, "Password for 'https://x-access-token@github.com': "],
                    check=True,
                    text=True,
                    capture_output=True,
                ).stdout.strip()

            self.assertEqual(password(), "")
            token_file.write_text("first-token\n")
            self.assertEqual(password(), "first-token")
            token_file.write_text("rotated-token\n")
            self.assertEqual(password(), "rotated-token")

    def test_run_forever_restores_repo_cache_umask(self) -> None:
        class StopAfterUmask(repo_cache_sync.RepoCacheSync):
            def configure_git(self) -> None:
                raise RuntimeError("stop")

        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        old_umask = os.umask(0o077)
        try:
            sync = StopAfterUmask(
                cache_dir=Path(tmp.name),
                repositories=["acme/centaur"],
                repository_refs={},
                repository_visibilities={},
                sync_interval_seconds=30,
                github_token_file=Path("/tmp/missing-token"),
            )
            with self.assertRaises(RuntimeError):
                sync.run_forever()
            current_umask = os.umask(old_umask)
            self.assertEqual(current_umask, 0o022)
        finally:
            os.umask(old_umask)


class RepoCacheCheckoutTest(unittest.TestCase):
    repo = "acme/docs"

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.git_env = {
            **os.environ,
            "LC_ALL": "C",
            "GIT_CONFIG_GLOBAL": str(self.root / "gitconfig"),
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_AUTHOR_NAME": "test",
            "GIT_AUTHOR_EMAIL": "test@example.com",
            "GIT_COMMITTER_NAME": "test",
            "GIT_COMMITTER_EMAIL": "test@example.com",
        }
        self.work = self.root / "remotes/acme/docs.git"
        self.git("init", "-q", "-b", "main", str(self.work))
        self.git(
            "config",
            "--global",
            f"url.file://{self.root / 'remotes'}/.insteadOf",
            "https://github.com/",
        )
        (self.work / "tracked").write_text("original")
        self.git("-C", str(self.work), "add", "tracked")
        self.git("-C", str(self.work), "commit", "-qm", "one")
        self.git("-C", str(self.work), "tag", "v1")
        self.sync = repo_cache_sync.RepoCacheSync(
            cache_dir=self.root / "cache",
            repositories=[self.repo],
            repository_refs={},
            repository_visibilities={self.repo: "public"},
            sync_interval_seconds=600,
            github_token_file=self.root / "no-token",
        )
        self.sync.git_env = self.git_env
        self.target = self.sync.repository_target(self.repo)

    def git(self, *args: str) -> str:
        return subprocess.run(
            ["git", *args], check=True, env=self.git_env, text=True, capture_output=True
        ).stdout.strip()

    def test_recovers_stale_locks_and_moved_tag(self) -> None:
        self.sync.sync_repo(self.repo)
        self.git("-C", str(self.work), "commit", "-q", "--allow-empty", "-m", "two")
        self.git("-C", str(self.work), "tag", "-f", "v1")
        expected = self.git("-C", str(self.work), "rev-parse", "HEAD")
        git_dir = self.target / ".git"
        locks = [
            git_dir / name
            for name in (
                "index.lock",
                "HEAD.lock",
                "packed-refs.lock",
                "shallow.lock",
                "config.lock",
                "ORIG_HEAD.lock",
                "FETCH_HEAD.lock",
                "refs/remotes/origin/main.lock",
                "refs/heads/nested/topic.lock",
            )
        ]
        for lock in locks:
            lock.parent.mkdir(parents=True, exist_ok=True)
            lock.write_text("stale")
        other = git_dir / "unrelated.lock"
        other.touch()
        self.assertTrue(self.sync.sync_once())
        self.assertFalse(any(lock.exists() for lock in locks))
        self.assertTrue(other.exists())
        self.assertEqual(
            self.git("-C", str(self.target), "rev-parse", "HEAD"), expected
        )
        self.assertEqual(self.git("-C", str(self.target), "rev-parse", "v1"), expected)
        self.assertEqual(self.sync.check_ready(), 0)

    def test_dirty_tracked_files_are_restored_for_default_and_pinned_refs(self) -> None:
        self.sync.sync_repo(self.repo)
        sha = self.git("-C", str(self.work), "rev-parse", "HEAD")
        self.git("-C", str(self.work), "update-ref", "refs/review/one", sha)
        for ref in ("", "main", "v1", sha, "refs/review/one"):
            with self.subTest(ref=ref):
                self.sync.repository_refs = {self.repo: ref} if ref else {}
                (self.target / "tracked").write_text("partial update")
                self.assertTrue(self.sync.sync_once())
                self.assertEqual((self.target / "tracked").read_text(), "original")
                self.assertEqual(
                    self.git("-C", str(self.target), "status", "--porcelain"), ""
                )

    def test_rebuilds_truncated_index(self) -> None:
        (self.work / "obsolete").touch()
        self.git("-C", str(self.work), "add", "obsolete")
        self.git("-C", str(self.work), "commit", "-qm", "track obsolete path")
        self.sync.sync_repo(self.repo)
        (self.target / ".git/index").write_bytes(b"partial")
        (self.target / "tracked").write_text("partial update")
        self.git("-C", str(self.work), "rm", "obsolete")
        (self.work / ".gitignore").write_text("obsolete\n")
        self.git("-C", str(self.work), "add", ".gitignore")
        self.git("-C", str(self.work), "commit", "-qm", "remove obsolete path")
        self.assertTrue(self.sync.sync_once())
        self.assertFalse((self.target / "obsolete").exists())
        self.assertEqual(self.git("-C", str(self.target), "ls-files").splitlines(), [".gitignore", "tracked"])
        self.assertEqual((self.target / "tracked").read_text(), "original")
        self.assertEqual(self.sync.check_ready(), 0)

    def test_writer_lock_is_private_on_creation_and_upgrade(self) -> None:
        lock = self.sync.cache_dir / ".repo-cache-sync.lock"
        with self.sync._cache_writer():
            self.assertEqual(stat.S_IMODE(lock.stat().st_mode), 0o600)
        inode = lock.stat().st_ino
        lock.chmod(0o644)
        with self.sync._cache_writer():
            self.assertEqual(lock.stat().st_ino, inode)
            self.assertEqual(stat.S_IMODE(lock.stat().st_mode), 0o600)

    def test_writer_refuses_symlinked_lock(self) -> None:
        self.sync.cache_dir.mkdir()
        target = self.root / "outside-lock"
        target.touch()
        target.chmod(0o644)
        (self.sync.cache_dir / ".repo-cache-sync.lock").symlink_to(target)
        with self.assertRaises(OSError) as error:
            self.sync.sync_once()
        self.assertEqual(error.exception.errno, errno.ELOOP)
        self.assertEqual(stat.S_IMODE(target.stat().st_mode), 0o644)

    def test_writer_rejects_nonregular_lock_before_chmod(self) -> None:
        self.sync.cache_dir.mkdir()
        lock = self.sync.cache_dir / ".repo-cache-sync.lock"
        os.mkfifo(lock)
        lock.chmod(0o644)
        with self.assertRaisesRegex(RuntimeError, "regular repo-cache writer lock"):
            self.sync.sync_once()
        self.assertEqual(stat.S_IMODE(lock.stat().st_mode), 0o644)

    def test_contending_writer_leaves_lock_and_ready_marker_alone(self) -> None:
        self.assertTrue(self.sync.sync_once())
        marker = self.sync.ready_file.read_bytes()
        index_lock = self.target / ".git/index.lock"
        index_lock.touch()
        with (self.sync.cache_dir / ".repo-cache-sync.lock").open("r+") as writer:
            fcntl.flock(writer, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.assertEqual(self.sync.run_forever(), 1)
            with self.assertRaises(repo_cache_sync.CacheWriterBusy):
                self.sync.sync_repo(self.repo)
        self.assertTrue(index_lock.exists())
        self.assertEqual(self.sync.ready_file.read_bytes(), marker)

    def test_remote_index_error_preserves_healthy_local_index(self) -> None:
        self.assertTrue(self.sync.sync_once())
        index = self.target / ".git/index"
        before = index.read_bytes()
        with mock.patch.object(
            self.sync,
            "_refresh_checkout",
            side_effect=repo_cache_sync.GitError("fetch", "remote: index file corrupt"),
        ) as refresh:
            self.assertFalse(self.sync.sync_once())
        refresh.assert_called_once()
        self.assertEqual(index.read_bytes(), before)
        self.assertFalse(self.sync.ready_file.exists())

    def test_cleanup_rejects_nonregular_locks_and_does_not_follow_ref_symlinks(
        self,
    ) -> None:
        self.assertTrue(self.sync.sync_once())
        outside = self.root / "outside"
        outside.mkdir()
        protected = outside / "main.lock"
        protected.write_text("keep")
        git_dir = self.target / ".git"
        (git_dir / "refs/elsewhere").symlink_to(outside, target_is_directory=True)
        symlink = git_dir / "index.lock"
        symlink.symlink_to(protected)
        self.assertFalse(self.sync.sync_once())
        self.assertTrue(symlink.is_symlink())
        self.assertEqual(protected.read_text(), "keep")
        self.assertEqual(self.sync.check_ready(), 1)
        symlink.unlink()
        # Exercise traversal even if Git rejects the foreign ref on update.
        with self.sync._cache_writer():
            self.sync._recover_git_locks(self.target)
        self.assertEqual(protected.read_text(), "keep")

    def start_daemon(self, **extra_env: str) -> subprocess.Popen:
        env = {
            **self.git_env,
            "REPO_CACHE_DIR": str(self.sync.cache_dir),
            "REPOSITORIES": self.repo,
            "REPOSITORY_VISIBILITIES": f"{self.repo}=public",
            "SYNC_INTERVAL_SECONDS": "600",
            **extra_env,
        }
        process = subprocess.Popen(
            [sys.executable, repo_cache_sync.__file__],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )

        def cleanup():
            if process.poll() is None:
                process.kill()
            process.communicate(timeout=10)

        self.addCleanup(cleanup)
        return process

    def wait_for_file(self, path: Path, process: subprocess.Popen) -> None:
        deadline = time.monotonic() + 10
        while not path.exists():
            if process.poll() is not None:
                self.fail(f"repo-cache exited early: {process.communicate()[1]!r}")
            self.assertLess(
                time.monotonic(), deadline, f"timed out waiting for {path.name}"
            )
            time.sleep(0.02)

    def test_sigterm_wakes_idle_sync_and_releases_writer(self) -> None:
        process = self.start_daemon()
        self.wait_for_file(self.sync.ready_file, process)
        process.send_signal(signal.SIGTERM)
        self.assertEqual(process.wait(timeout=5), 0)
        self.assertFalse(self.sync.ready_file.exists())
        self.assertTrue(self.sync.sync_once())

    def start_blocked_clone(
        self, ignore_term: bool = False
    ) -> tuple[subprocess.Popen, int]:
        started = self.root / "started"
        fake_git = self.root / "git"
        fake_git.write_text(
            """#!/bin/sh
case "$1" in
  clone)
    if [ "$IGNORE_TERM" = yes ]; then trap '' TERM; fi
    echo "$$" > "$STARTED.tmp" && mv "$STARTED.tmp" "$STARTED"
    exec sleep 60 ;;
  config) exit 0 ;;
  *) exit 1 ;;
esac
"""
        )
        fake_git.chmod(0o755)
        process = self.start_daemon(
            PATH=f"{self.root}:{os.environ['PATH']}",
            REPOSITORIES=f"{self.repo} acme/next",
            STARTED=str(started),
            IGNORE_TERM="yes" if ignore_term else "no",
        )
        self.wait_for_file(started, process)
        pid = int(started.read_text())
        started.unlink()
        return process, pid

    def test_shutdown_stops_active_git_without_starting_next_repo(self) -> None:
        for sig in (signal.SIGINT, signal.SIGTERM):
            with self.subTest(sig=sig):
                process, pid = self.start_blocked_clone(
                    ignore_term=sig == signal.SIGTERM
                )
                process.send_signal(sig)
                stdout, stderr = process.communicate(timeout=10)
                self.assertEqual(process.returncode, 0, stderr)
                self.assertNotIn(b"acme/next", stdout)
                with self.assertRaises(ProcessLookupError):
                    os.kill(pid, 0)
                self.assertFalse(self.sync.ready_file.exists())

    def test_git_retains_writer_lock_if_sync_process_dies(self) -> None:
        process, pid = self.start_blocked_clone()
        try:
            process.kill()
            process.wait(timeout=5)
            with self.assertRaises(repo_cache_sync.CacheWriterBusy):
                self.sync.sync_repo(self.repo)
        finally:
            os.killpg(pid, signal.SIGKILL)


if __name__ == "__main__":
    unittest.main()
