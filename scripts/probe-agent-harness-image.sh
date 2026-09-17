#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/.." && pwd)"
image="${1:-}"

if [[ -z "$image" || "$image" == *$'\n'* ]]; then
  echo "usage: $0 IMAGE" >&2
  exit 2
fi
if ! command -v docker >/dev/null 2>&1; then
  echo "missing required command: docker" >&2
  exit 1
fi
if ! docker image inspect "$image" >/dev/null 2>&1; then
  echo "agent image is not loaded locally: $image" >&2
  exit 1
fi

codex_version="$(
  awk -F= '$1 == "ARG CODEX_VERSION" { print $2; exit }' \
    "${repo_root}/services/sandbox/Dockerfile"
)"
if [[ ! "$codex_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "could not derive the exact CODEX_VERSION from the sandbox Dockerfile" >&2
  exit 1
fi

docker run --rm -i \
  --network none \
  --entrypoint /bin/bash \
  --env "EXPECTED_CODEX_VERSION=codex-cli ${codex_version}" \
  --env OPENAI_API_KEY= \
  --env CODEX_API_KEY= \
  --env OPENROUTER_API_KEY= \
  --env META_AI_API_KEY= \
  "$image" -seu <<'CONTAINER_SCRIPT'
set -euo pipefail
IFS=$'\n\t'

export HOME=/tmp/centaur-harness-probe
export CODEX_HOME="$HOME/.codex"
mkdir -p "$CODEX_HOME"
cp /home/agent/harness/codex/config.toml "$CODEX_HOME/config.toml"

python3 - <<'PY'
from __future__ import annotations

import json
import os
import select
import subprocess
import time
import tomllib
from pathlib import Path


BAKED_HARNESS = Path("/home/agent/harness")


with (BAKED_HARNESS / "codex/config.toml").open("rb") as handle:
    baked_codex = tomllib.load(handle)
with (BAKED_HARNESS / "claude/settings.json").open(encoding="utf-8") as handle:
    baked_claude = json.load(handle)

assert baked_codex["model_providers"]["openrouter"] == {
    "name": "OpenRouter",
    "base_url": "https://openrouter.ai/api/v1",
    "env_key": "OPENROUTER_API_KEY",
    "wire_api": "responses",
    "requires_openai_auth": False,
}
assert baked_codex["model_providers"]["responses"] == {
    "name": "azure",
    "base_url": "https://api.ai.meta.com/v1",
    "env_key": "META_AI_API_KEY",
    "wire_api": "responses",
    "requires_openai_auth": False,
}
assert baked_codex["projects"]["/"]["trust_level"] == "trusted"
assert baked_claude["permissions"]["defaultMode"] == "bypassPermissions"
assert isinstance(baked_claude["model"], str) and baked_claude["model"]

version = subprocess.run(
    ["codex", "--version"],
    check=True,
    capture_output=True,
    text=True,
).stdout.strip()
assert version == os.environ["EXPECTED_CODEX_VERSION"], (
    f"unexpected packaged Codex version: {version!r}"
)

features_output = subprocess.run(
    ["codex", "features", "list"],
    check=True,
    capture_output=True,
    text=True,
).stdout
features = {
    fields[0]: fields[-1]
    for line in features_output.splitlines()
    if len(fields := line.split()) >= 2
}
assert features.get("multi_agent") == "false"
assert features.get("multi_agent_v2") == "false"


def send(process: subprocess.Popen[str], request: dict) -> None:
    assert process.stdin is not None
    process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
    process.stdin.flush()


def read_response(process: subprocess.Popen[str], request_id: int) -> dict:
    assert process.stdout is not None
    deadline = time.monotonic() + 10
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise AssertionError(f"timed out waiting for app-server response {request_id}")
        readable, _, _ = select.select([process.stdout], [], [], remaining)
        if not readable:
            raise AssertionError(f"timed out waiting for app-server response {request_id}")
        line = process.stdout.readline()
        if not line:
            raise AssertionError(
                f"app-server exited before response {request_id}: {process.poll()}"
            )
        message = json.loads(line)
        if message.get("id") != request_id:
            continue
        if "error" in message:
            raise AssertionError(
                f"app-server rejected response {request_id}: {message['error']!r}"
            )
        return message


def probe_provider(provider: str, model: str) -> None:
    process = subprocess.Popen(
        ["codex", "app-server", "--listen", "stdio://"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        send(
            process,
            {
                "id": 1,
                "method": "initialize",
                "params": {
                    "clientInfo": {
                        "name": "centaur-image-probe",
                        "title": None,
                        "version": "0",
                    },
                    "capabilities": None,
                },
            },
        )
        read_response(process, 1)
        send(
            process,
            {
                "id": 2,
                "method": "thread/start",
                "params": {
                    "approvalPolicy": "never",
                    "sandbox": "danger-full-access",
                    "model": model,
                    "modelProvider": provider,
                },
            },
        )
        response = read_response(process, 2)
        thread_id = response.get("result", {}).get("thread", {}).get("id")
        assert isinstance(thread_id, str) and thread_id
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)


# No turn/start request is sent. Docker also disables the container network, so
# these checks exercise packaged provider discovery without a model call.
probe_provider("openrouter", "openrouter/auto")
probe_provider("responses", "meta-llama/llama-4-maverick")

print("packaged agent harness probe passed")
PY
CONTAINER_SCRIPT
