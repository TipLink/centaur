#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'

probe=scripts/probe-rollback-agent-image.sh
scratch="$(mktemp -d -t rollback-agent-probe.XXXXXXXXXX)"
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/bin"

cat >"$scratch/bin/docker" <<'MOCK_DOCKER'
#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'

if [[ "${1:-}" == "image" && "${2:-}" == "inspect" ]]; then
  [[ "${MOCK_IMAGE_PRESENT:?}" == "true" ]]
  exit
fi
if [[ "${1:-}" == "run" ]]; then
  printf '%s\n' "$@" >"${MOCK_DOCKER_ARGS:?}"
  cat >"${MOCK_DOCKER_STDIN:?}"
  exit
fi

echo "unexpected mocked docker invocation: $*" >&2
exit 97
MOCK_DOCKER
chmod +x "$scratch/bin/docker"

export PATH="$scratch/bin:$PATH"
export MOCK_DOCKER_ARGS="$scratch/docker-args"
export MOCK_DOCKER_STDIN="$scratch/docker-stdin"
export MOCK_IMAGE_PRESENT=true

expect_reject() {
  local label=$1
  shift
  if "$@" >"$scratch/${label}.out" 2>&1; then
    echo "rollback agent probe unexpectedly accepted: $label" >&2
    exit 1
  fi
}

bash "$probe" centaur-agent:test linux/amd64
grep -Fxq -- '--network' "$MOCK_DOCKER_ARGS"
grep -Fxq -- 'none' "$MOCK_DOCKER_ARGS"
grep -Fxq -- '--entrypoint' "$MOCK_DOCKER_ARGS"
grep -Fxq -- '/entrypoint.sh' "$MOCK_DOCKER_ARGS"
grep -Fxq -- 'EXPECTED_DEBIAN_ARCH=amd64' "$MOCK_DOCKER_ARGS"
grep -Fxq -- 'CODEX_MODEL=gpt-5.6-sol' "$MOCK_DOCKER_ARGS"
grep -Fxq -- 'CODEX_MODEL_REASONING_EFFORT=medium' "$MOCK_DOCKER_ARGS"
grep -Fq -- 'plan_mode_reasoning_effort = "xhigh"' "$MOCK_DOCKER_ARGS"
grep -Fq -- 'max_concurrent_threads_per_session = 6' "$MOCK_DOCKER_ARGS"
grep -Fq -- '"method": "initialize"' "$MOCK_DOCKER_STDIN"
if grep -Fq -- '"method": "turn/start"' "$MOCK_DOCKER_STDIN"; then
  echo "rollback agent image probe must not start a model turn" >&2
  exit 1
fi

bash "$probe" centaur-agent:test linux/arm64
grep -Fxq -- 'EXPECTED_DEBIAN_ARCH=arm64' "$MOCK_DOCKER_ARGS"

expect_reject missing-image-argument bash "$probe"
expect_reject invalid-image-reference bash "$probe" '-unsafe' linux/amd64
expect_reject invalid-platform bash "$probe" centaur-agent:test linux/s390x
export MOCK_IMAGE_PRESENT=false
expect_reject image-not-loaded bash "$probe" centaur-agent:test linux/amd64

echo "rollback agent packaged-image probe tests passed"
