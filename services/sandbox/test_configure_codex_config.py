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
        path.write_text(textwrap.dedent(source).lstrip(), encoding="utf-8")
        configure_codex_config.configure(path, env or {})
        text = path.read_text(encoding="utf-8")
        return text, tomllib.loads(text)


class ConfigureCodexConfigTests(unittest.TestCase):
    def test_disables_fineas_multi_agent_v2_table_without_conflicting_scalar(
        self,
    ) -> None:
        text, parsed = render(
            """
            model = "gpt-5.6-sol"
            model_reasoning_effort = "medium"

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
            """,
            {
                "CODEX_CONFIG_OVERLAY": 'plan_mode_reasoning_effort = "xhigh"',
            },
        )

        self.assertNotIn("multi_agent_v2 = false", text)
        self.assertFalse(parsed["features"]["multi_agent"])
        self.assertFalse(parsed["features"]["multi_agent_v2"]["enabled"])
        self.assertEqual(
            parsed["features"]["multi_agent_v2"]["max_concurrent_threads_per_session"],
            6,
        )
        self.assertEqual(parsed["plan_mode_reasoning_effort"], "xhigh")

    def test_preserves_legacy_multi_agent_v2_scalar_when_no_table_exists(self) -> None:
        _, parsed = render(
            """
            [features]
            goals = true
            multi_agent_v2 = true
            """
        )

        self.assertFalse(parsed["features"]["multi_agent"])
        self.assertFalse(parsed["features"]["multi_agent_v2"])

    def test_multiline_text_and_commented_quoted_tables_cannot_redirect_mutation(
        self,
    ) -> None:
        _, parsed = render(
            '''
            banner = """
            [features]
            multi_agent = true
            """

            [features] # actual feature table
            multi_agent = true

            [features."multi_agent_v2"] # configured concurrency table
            enabled = true
            max_concurrent_threads_per_session = 6
            '''
        )

        self.assertIn("[features]", parsed["banner"])
        self.assertFalse(parsed["features"]["multi_agent"])
        self.assertFalse(parsed["features"]["multi_agent_v2"]["enabled"])
        self.assertEqual(
            parsed["features"]["multi_agent_v2"]["max_concurrent_threads_per_session"],
            6,
        )

    def test_top_level_reasoning_overrides_include_max(self) -> None:
        text, parsed = render(
            """
            [features]
            goals = true
            """,
            {
                "CODEX_MODEL_REASONING_SUMMARY": "concise",
                "CODEX_MODEL_REASONING_EFFORT": "max",
            },
        )

        self.assertLess(text.index("model_reasoning_summary"), text.index("[features]"))
        self.assertEqual(parsed["model_reasoning_summary"], "concise")
        self.assertEqual(parsed["model_reasoning_effort"], "max")

    def test_operator_overlay_can_override_bedrock_region(self) -> None:
        _, parsed = render(
            """
            [model_providers.amazon-bedrock]
            name = "Amazon Bedrock"
            """,
            {
                "CODEX_BEDROCK_REGION": "us-east-1",
                "CODEX_CONFIG_OVERLAY": """
                    [model_providers.amazon-bedrock.aws]
                    region = "us-west-2"
                """,
            },
        )

        self.assertEqual(
            parsed["model_providers"]["amazon-bedrock"]["aws"]["region"], "us-west-2"
        )

    def test_bedrock_region_is_injected_without_operator_overlay(self) -> None:
        _, parsed = render(
            """
            [model_providers.amazon-bedrock]
            name = "Amazon Bedrock"
            """,
            {"CODEX_BEDROCK_REGION": "us-east-1"},
        )

        self.assertEqual(
            parsed["model_providers"]["amazon-bedrock"]["aws"]["region"], "us-east-1"
        )

    def test_invalid_overlay_does_not_replace_valid_generated_config(self) -> None:
        _, parsed = render(
            """
            [features]
            goals = true
            """,
            {"CODEX_CONFIG_OVERLAY": "[invalid"},
        )

        self.assertTrue(parsed["features"]["goals"])
        self.assertFalse(parsed["features"]["multi_agent"])

    def test_invalid_generated_config_is_not_written(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "config.toml"
            original = "invalid = [\n"
            path.write_text(original, encoding="utf-8")

            with self.assertRaises(SystemExit):
                configure_codex_config.configure(path, {})

            self.assertEqual(path.read_text(encoding="utf-8"), original)


if __name__ == "__main__":
    unittest.main()
