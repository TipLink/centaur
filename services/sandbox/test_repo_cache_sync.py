from __future__ import annotations

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

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

    def test_clean_checkout_restores_tracked_files_and_removes_untracked_files(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            target = root / "repo"
            target.mkdir()
            subprocess.run(["git", "init", "-q", str(target)], check=True)
            subprocess.run(
                ["git", "-C", str(target), "config", "user.email", "test@example.com"],
                check=True,
            )
            subprocess.run(
                ["git", "-C", str(target), "config", "user.name", "Repo Cache Test"],
                check=True,
            )
            tracked = target / "index.mjs"
            tracked.write_text("reviewed\n")
            subprocess.run(["git", "-C", str(target), "add", "index.mjs"], check=True)
            subprocess.run(
                ["git", "-C", str(target), "commit", "-q", "-m", "fixture"],
                check=True,
            )
            tracked.write_text("tampered\n")
            untracked = target / "untracked.mjs"
            untracked.write_text("tampered\n")
            ignored = target / "node_modules" / "pg" / "index.mjs"
            ignored.parent.mkdir(parents=True)
            ignored.write_text("tampered\n")
            (target / ".git" / "info" / "exclude").write_text("node_modules/\n")
            sync = repo_cache_sync.RepoCacheSync(
                cache_dir=root / "cache",
                repositories=["acme/centaur"],
                repository_refs={},
                repository_visibilities={},
                sync_interval_seconds=30,
                github_token_file=root / "missing-token",
            )

            sync.clean_checkout("acme/centaur", target)

            self.assertEqual(tracked.read_text(), "reviewed\n")
            self.assertFalse(untracked.exists())
            self.assertFalse(ignored.exists())

    def test_checkout_pinned_ref_discards_dirty_tracked_file_while_advancing(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            target = root / "repo"
            target.mkdir()
            subprocess.run(["git", "init", "-q", str(target)], check=True)
            subprocess.run(
                ["git", "-C", str(target), "config", "user.email", "test@example.com"],
                check=True,
            )
            subprocess.run(
                ["git", "-C", str(target), "config", "user.name", "Repo Cache Test"],
                check=True,
            )
            tracked = target / "index.mjs"
            tracked.write_text("first\n")
            subprocess.run(["git", "-C", str(target), "add", "index.mjs"], check=True)
            subprocess.run(
                ["git", "-C", str(target), "commit", "-q", "-m", "first"],
                check=True,
            )
            first = subprocess.run(
                ["git", "-C", str(target), "rev-parse", "HEAD"],
                check=True,
                text=True,
                stdout=subprocess.PIPE,
            ).stdout.strip()
            tracked.write_text("second\n")
            subprocess.run(["git", "-C", str(target), "commit", "-qam", "second"], check=True)
            second = subprocess.run(
                ["git", "-C", str(target), "rev-parse", "HEAD"],
                check=True,
                text=True,
                stdout=subprocess.PIPE,
            ).stdout.strip()
            subprocess.run(
                ["git", "-C", str(target), "checkout", "-q", "--detach", first],
                check=True,
            )
            tracked.write_text("tampered\n")
            sync = repo_cache_sync.RepoCacheSync(
                cache_dir=root / "cache",
                repositories=["acme/centaur"],
                repository_refs={"acme/centaur": second},
                repository_visibilities={},
                sync_interval_seconds=30,
                github_token_file=root / "missing-token",
            )

            sync.checkout_repo("acme/centaur", target)

            self.assertEqual(tracked.read_text(), "second\n")
            self.assertEqual(sync._git_output(target, "rev-parse", "HEAD"), second)

    def test_run_forever_restores_repo_cache_umask(self) -> None:
        class StopAfterUmask(repo_cache_sync.RepoCacheSync):
            def configure_git(self) -> None:
                raise RuntimeError("stop")

        old_umask = os.umask(0o077)
        try:
            sync = StopAfterUmask(
                cache_dir=Path("/tmp"),
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


if __name__ == "__main__":
    unittest.main()
