import re
from pathlib import Path


DOCKERFILE = Path(__file__).with_name("Dockerfile")
ENTRYPOINT = Path(__file__).with_name("entrypoint.sh")
REVIEWED_VERSION_BASELINES = {
    # These are TipLink's tested pins. Codex 0.144.0 already supports GPT-5.6;
    # 0.144.1 is retained for its later installer/code-mode fixes. Claude Code
    # 2.1.197 is the Sonnet 5 floor, and 2.1.198 is TipLink's tested patch.
    "CLAUDE_CODE_VERSION": (2, 1, 198),
    "CODEX_VERSION": (0, 144, 1),
    "PLAYWRIGHT_VERSION": (1, 58, 0),
}


def _version_pin(name: str) -> tuple[int, ...]:
    source = DOCKERFILE.read_text(encoding="utf-8")
    match = re.search(
        rf"^ARG {re.escape(name)}=([0-9]+(?:\.[0-9]+)+)$", source, re.MULTILINE
    )
    assert match is not None, f"missing numeric {name} pin in {DOCKERFILE}"
    return tuple(int(part) for part in match.group(1).split("."))


def test_runtime_pins_meet_tiplink_reviewed_baselines() -> None:
    for name, minimum in REVIEWED_VERSION_BASELINES.items():
        assert _version_pin(name) >= minimum, (
            f"{name} regressed below {'.'.join(map(str, minimum))}"
        )


def test_sandbox_keeps_cross_architecture_development_tools() -> None:
    source = DOCKERFILE.read_text(encoding="utf-8")

    for required in (
        "apt.releases.hashicorp.com",
        "docker-ce-cli terraform",
        "terraform version",
        "playwright install --with-deps --only-shell chromium",
        "AGENT_BROWSER_EXECUTABLE_PATH=/home/agent/.local/bin/centaur-agent-browser-chromium",
        "agent-browser install",
        "-path '*/chrome-*/chrome'",
        "-path '*/chrome-linux/headless_shell'",
    ):
        assert required in source


def test_node_tools_opt_in_to_the_sandbox_proxy_without_dropping_options() -> None:
    source = ENTRYPOINT.read_text(encoding="utf-8")

    assert '*" --use-env-proxy "*)' in source
    assert 'NODE_OPTIONS="${NODE_OPTIONS:+$NODE_OPTIONS }--use-env-proxy"' in source
