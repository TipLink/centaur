#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'

image=${1:-}
platform=${2:-}

if [[ ! "$image" =~ ^[A-Za-z0-9][A-Za-z0-9._/@:-]*$ ]]; then
  echo "usage: $0 IMAGE linux/amd64|linux/arm64" >&2
  exit 2
fi

case "$platform" in
  linux/amd64)
    expected_debian_arch=amd64
    ;;
  linux/arm64)
    expected_debian_arch=arm64
    ;;
  *)
    echo "usage: $0 IMAGE linux/amd64|linux/arm64" >&2
    exit 2
    ;;
esac

if ! command -v docker >/dev/null 2>&1; then
  echo "missing required command: docker" >&2
  exit 1
fi
if ! docker image inspect "$image" >/dev/null 2>&1; then
  echo "rollback agent image is not loaded locally: $image" >&2
  exit 1
fi

# These are the exact Fineas rollback deployment deltas. The bridge image owns
# the legacy-compatible base config; entrypoint.sh must merge these values over
# that base without losing its OpenRouter provider or root trust declaration.
codex_overlay='model = "gpt-5.6-sol"
plan_mode_reasoning_effort = "xhigh"

[features.multi_agent_v2]
enabled = false
max_concurrent_threads_per_session = 6'
claude_overlay='{"model":"claude-sonnet-5","effortLevel":"high","alwaysThinkingEnabled":false}'

docker run --rm -i \
  --network none \
  --entrypoint /entrypoint.sh \
  --env "EXPECTED_DEBIAN_ARCH=${expected_debian_arch}" \
  --env 'EXPECTED_CODEX_VERSION=codex-cli 0.144.1' \
  --env 'EXPECTED_CLAUDE_VERSION=2.1.198 (Claude Code)' \
  --env 'EXPECTED_PLAYWRIGHT_VERSION=Version 1.58.0' \
  --env 'EXPECTED_AGENT_BROWSER_VERSION=0.26.0' \
  --env 'EXPECTED_HARNESS_SERVER_VERSION=harness-server 0.1.0' \
  --env CODEX_AUTH_MODE=access_token \
  --env CLAUDE_CODE_AUTH_MODE=access_token \
  --env CODEX_MODEL=gpt-5.6-sol \
  --env CODEX_MODEL_REASONING_EFFORT=medium \
  --env "CODEX_CONFIG_OVERLAY=${codex_overlay}" \
  --env CLAUDE_MODEL=claude-sonnet-5 \
  --env CLAUDE_CODE_EFFORT_LEVEL=high \
  --env "CLAUDE_SETTINGS_OVERLAY=${claude_overlay}" \
  --env GOOGLE_APPLICATION_CREDENTIALS=/tmp/centaur-no-network-adc.json \
  --env OPENAI_API_KEY= \
  --env CODEX_API_KEY= \
  --env OPENROUTER_API_KEY= \
  --env META_AI_API_KEY= \
  --env CENTAUR_TOOLS_URL= \
  "$image" /bin/bash -seu <<'CONTAINER_SCRIPT'
set -euo pipefail
IFS=$'\n\t'

test "$(dpkg --print-architecture)" = "$EXPECTED_DEBIAN_ARCH"
test "$(codex --version)" = "$EXPECTED_CODEX_VERSION"
test "$(claude --version)" = "$EXPECTED_CLAUDE_VERSION"
test "$(playwright --version)" = "$EXPECTED_PLAYWRIGHT_VERSION"
test "$(harness-server --version)" = "$EXPECTED_HARNESS_SERVER_VERSION"

agent_browser_version="$(agent-browser --version)"
case "$agent_browser_version" in
  "$EXPECTED_AGENT_BROWSER_VERSION" | "agent-browser $EXPECTED_AGENT_BROWSER_VERSION") ;;
  *)
    echo "unexpected agent-browser version: $agent_browser_version" >&2
    exit 1
    ;;
esac

for command in codex claude playwright agent-browser harness-server; do
  command -v "$command" >/dev/null
done
test "${AGENT_BROWSER_EXECUTABLE_PATH:-}" = \
  /home/agent/.local/bin/centaur-agent-browser-chromium
test -x "$AGENT_BROWSER_EXECUTABLE_PATH"
"$AGENT_BROWSER_EXECUTABLE_PATH" --version >/dev/null

test "${CODEX_MODEL:-}" = gpt-5.6-sol
test "${CODEX_MODEL_REASONING_EFFORT:-}" = medium
test "${CLAUDE_MODEL:-}" = claude-sonnet-5
test "${CLAUDE_CODE_EFFORT_LEVEL:-}" = high
test -z "${CENTAUR_HARNESS_CONFIG_DIR:-}"

python3 - <<'PY'
from __future__ import annotations

import json
import os
import select
import subprocess
import time
import tomllib
from pathlib import Path


home = Path.home()
with (home / ".codex/config.toml").open("rb") as handle:
    codex_config = tomllib.load(handle)
with (home / ".claude/settings.json").open(encoding="utf-8") as handle:
    claude_settings = json.load(handle)

assert codex_config["model"] == "gpt-5.6-sol"
assert codex_config["model_reasoning_effort"] == "medium"
assert codex_config["plan_mode_reasoning_effort"] == "xhigh"
assert codex_config["service_tier"] == "fast"

features = codex_config["features"]
assert features["multi_agent"] is False
assert features["multi_agent_v2"] == {
    "enabled": False,
    "max_concurrent_threads_per_session": 6,
}
assert features["enable_fanout"] is False

assert codex_config["model_providers"] == {
    "openrouter": {
        "name": "OpenRouter",
        "base_url": "https://openrouter.ai/api/v1",
        "env_key": "OPENROUTER_API_KEY",
        "wire_api": "responses",
        "requires_openai_auth": False,
    }
}
assert codex_config["projects"] == {"/": {"trust_level": "trusted"}}

assert claude_settings == {
    "model": "claude-sonnet-5",
    "effortLevel": "high",
    "permissions": {
        "defaultMode": "bypassPermissions",
        "additionalDirectories": [
            "/home/agent/workspace",
            "/home/agent/uploads",
        ],
    },
    "includeCoAuthoredBy": False,
    "cleanupPeriodDays": 1,
    "viewMode": "verbose",
    "alwaysThinkingEnabled": False,
}

feature_lines = subprocess.run(
    ["codex", "features", "list"],
    check=True,
    capture_output=True,
    text=True,
).stdout.splitlines()
feature_states = {
    fields[0]: fields[-1]
    for line in feature_lines
    if len(fields := line.split()) >= 2
}
assert feature_states.get("multi_agent") == "false"
assert feature_states.get("multi_agent_v2") == "false"


process = subprocess.Popen(
    ["codex", "app-server", "--listen", "stdio://"],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    text=True,
)
try:
    assert process.stdin is not None
    process.stdin.write(
        json.dumps(
            {
                "id": 1,
                "method": "initialize",
                "params": {
                    "clientInfo": {
                        "name": "centaur-rollback-image-probe",
                        "title": None,
                        "version": "0",
                    },
                    "capabilities": None,
                },
            },
            separators=(",", ":"),
        )
        + "\n"
    )
    process.stdin.flush()

    assert process.stdout is not None
    deadline = time.monotonic() + 10
    while True:
        remaining = deadline - time.monotonic()
        assert remaining > 0, "timed out waiting for Codex app-server initialize"
        readable, _, _ = select.select([process.stdout], [], [], remaining)
        assert readable, "timed out waiting for Codex app-server initialize"
        line = process.stdout.readline()
        assert line, f"Codex app-server exited during initialize: {process.poll()}"
        message = json.loads(line)
        if message.get("id") != 1:
            continue
        assert "error" not in message, message["error"]
        assert "result" in message
        break
finally:
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)

print("deploy-composed rollback agent harness probe passed")
PY
CONTAINER_SCRIPT
