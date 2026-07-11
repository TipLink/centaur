import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("select_overlay_prompt.sh")


class SelectOverlayPromptTest(unittest.TestCase):
    def select(
        self,
        *,
        home: Path,
        overlay_dir: Path | None = None,
        image_overlay_dir: Path | None = None,
    ) -> str:
        env = {**os.environ, "HOME": str(home)}
        if overlay_dir is None:
            env.pop("CENTAUR_OVERLAY_DIR", None)
        else:
            env["CENTAUR_OVERLAY_DIR"] = str(overlay_dir)
        if image_overlay_dir is None:
            env.pop("CENTAUR_IMAGE_OVERLAY_DIR", None)
        else:
            env["CENTAUR_IMAGE_OVERLAY_DIR"] = str(image_overlay_dir)
        return subprocess.check_output(["bash", str(SCRIPT)], env=env, text=True).strip()

    def test_explicit_repo_prompt_wins_over_image_fallback(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            home = root / "home"
            overlay = root / "overlay"
            fallback = home / "AGENTS_OVERLAY.md"
            repo_prompt = overlay / "services" / "sandbox" / "SYSTEM_PROMPT.md"
            fallback.parent.mkdir(parents=True)
            repo_prompt.parent.mkdir(parents=True)
            fallback.write_text("image fallback")
            repo_prompt.write_text("live repo prompt")

            self.assertEqual(self.select(home=home, overlay_dir=overlay), str(repo_prompt))

    def test_image_prompt_is_used_when_repo_root_is_unavailable(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            home = root / "home"
            fallback = home / "AGENTS_OVERLAY.md"
            fallback.parent.mkdir(parents=True)
            fallback.write_text("image fallback")

            self.assertEqual(self.select(home=home, overlay_dir=root / "missing"), str(fallback))

    def test_existing_repo_without_prompt_disables_image_fallbacks(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            home = root / "home"
            overlay = root / "overlay"
            image_overlay = root / "image-overlay"
            (home / "AGENTS_OVERLAY.md").parent.mkdir(parents=True)
            (home / "AGENTS_OVERLAY.md").write_text("home fallback")
            image_prompt = image_overlay / "services" / "sandbox" / "SYSTEM_PROMPT.md"
            image_prompt.parent.mkdir(parents=True)
            image_prompt.write_text("mounted image fallback")
            overlay.mkdir()

            self.assertEqual(
                self.select(
                    home=home,
                    overlay_dir=overlay,
                    image_overlay_dir=image_overlay,
                ),
                "",
            )

    def test_mounted_image_prompt_is_used_when_explicit_repo_is_absent(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            home = root / "home"
            image_overlay = root / "image-overlay"
            image_prompt = image_overlay / "services" / "sandbox" / "SYSTEM_PROMPT.md"
            image_prompt.parent.mkdir(parents=True)
            image_prompt.write_text("mounted image fallback")

            self.assertEqual(
                self.select(
                    home=home,
                    overlay_dir=root / "missing-repo",
                    image_overlay_dir=image_overlay,
                ),
                str(image_prompt),
            )

    def test_no_prompt_returns_empty_output(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            self.assertEqual(self.select(home=Path(temp_dir)), "")


if __name__ == "__main__":
    unittest.main()
