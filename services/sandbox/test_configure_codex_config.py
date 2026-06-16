#!/usr/bin/env python3
from __future__ import annotations

import tempfile
import textwrap
import tomllib
import unittest
from pathlib import Path

import configure_codex_config


def render(source: str, env: dict[str, str] | None = None) -> tuple[str, dict]:
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "config.toml"
        path.write_text(textwrap.dedent(source).lstrip())
        configure_codex_config.configure(path, env or {})
        text = path.read_text()
        return text, tomllib.loads(text)


class ConfigureCodexConfigTests(unittest.TestCase):
    def test_disables_existing_multi_agent_v2_table_without_conflicting_scalar(self) -> None:
        text, parsed = render(
            """
            [features]
            goals = true
            fast_mode = true
            memories = true
            hooks = true
            enable_fanout = false
            runtime_metrics = true

            [features.multi_agent_v2]
            enabled = true
            max_concurrent_threads_per_session = 6
            """
        )

        self.assertNotIn("multi_agent_v2 = false", text)
        self.assertFalse(parsed["features"]["multi_agent"])
        self.assertFalse(parsed["features"]["multi_agent_v2"]["enabled"])
        self.assertEqual(parsed["features"]["multi_agent_v2"]["max_concurrent_threads_per_session"], 6)

    def test_preserves_legacy_multi_agent_v2_scalar_when_no_table_exists(self) -> None:
        _, parsed = render(
            """
            [features]
            goals = true
            """
        )

        self.assertFalse(parsed["features"]["multi_agent"])
        self.assertFalse(parsed["features"]["multi_agent_v2"])

    def test_removes_legacy_scalar_when_multi_agent_v2_table_exists(self) -> None:
        text, parsed = render(
            """
            [features]
            multi_agent_v2 = true

            [features.multi_agent_v2]
            enabled = true
            """
        )

        self.assertNotIn("multi_agent_v2 = false", text)
        self.assertNotIn("multi_agent_v2 = true", text)
        self.assertFalse(parsed["features"]["multi_agent_v2"]["enabled"])

    def test_top_level_reasoning_overrides_still_work(self) -> None:
        text, parsed = render(
            """
            [features]
            goals = true
            """,
            {
                "CODEX_MODEL_REASONING_SUMMARY": "concise",
                "CODEX_MODEL_REASONING_EFFORT": "low",
            },
        )

        self.assertLess(text.index("model_reasoning_summary"), text.index("[features]"))
        self.assertEqual(parsed["model_reasoning_summary"], "concise")
        self.assertEqual(parsed["model_reasoning_effort"], "low")


if __name__ == "__main__":
    unittest.main()
