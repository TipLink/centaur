import re
from pathlib import Path


DOCKERFILE = Path(__file__).with_name("Dockerfile")
ENTRYPOINT = Path(__file__).with_name("entrypoint.sh")
REPO_ROOT = Path(__file__).resolve().parents[2]
VALIDATE_IMAGES_WORKFLOW = REPO_ROOT / ".github/workflows/validate-images.yml"
PACKAGED_HARNESS_PROBE = REPO_ROOT / "scripts/probe-agent-harness-image.sh"
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


def test_sandbox_packages_and_defaults_to_the_core_harness_config() -> None:
    dockerfile = DOCKERFILE.read_text(encoding="utf-8")
    entrypoint = ENTRYPOINT.read_text(encoding="utf-8")

    assert "COPY --link --chown=1001:1001 harness/ /home/agent/harness/" in dockerfile
    assert (
        'HARNESS_CONFIG_DIR="${CENTAUR_HARNESS_CONFIG_DIR:-$HOME_DIR/harness}"'
        in entrypoint
    )
    assert (
        'cp "$HARNESS_CONFIG_DIR/codex/config.toml" '
        '"$HOME_DIR/.codex/config.toml"' in entrypoint
    )
    assert (
        'cp "$HARNESS_CONFIG_DIR/claude/settings.json" '
        '"$HOME_DIR/.claude/settings.json"' in entrypoint
    )


def test_validate_images_loads_and_probes_only_native_agent_images() -> None:
    workflow = VALIDATE_IMAGES_WORKFLOW.read_text(encoding="utf-8")
    probe = PACKAGED_HARNESS_PROBE.read_text(encoding="utf-8")

    assert "- scripts/probe-agent-harness-image.sh" in workflow
    assert "load: ${{ matrix.service == 'agent' }}" in workflow
    assert (
        "tags: ${{ matrix.service == 'agent' && "
        "format('{0}:validate-{1}', matrix.image, env.PLATFORM_SLUG) || '' }}"
        in workflow
    )
    assert "if: matrix.service == 'agent'" in workflow
    assert 'bash scripts/probe-agent-harness-image.sh "$AGENT_IMAGE"' in workflow

    assert "--network none" in probe
    assert 'export CODEX_HOME="$HOME/.codex"' in probe
    for credential in (
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
        "OPENROUTER_API_KEY",
        "META_AI_API_KEY",
    ):
        assert f"--env {credential}=" in probe
    assert '["codex", "--version"]' in probe
    assert '["codex", "features", "list"]' in probe
    assert 'BAKED_HARNESS / "codex/config.toml"' in probe
    assert 'BAKED_HARNESS / "claude/settings.json"' in probe
    assert '"method": "thread/start"' in probe
    assert '"method": "turn/start"' not in probe
    assert 'probe_provider("openrouter", "openrouter/auto")' in probe
    assert (
        'probe_provider("responses", "meta-llama/llama-4-maverick")' in probe
    )
