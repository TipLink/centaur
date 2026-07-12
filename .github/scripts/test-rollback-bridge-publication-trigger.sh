#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'

publisher=.github/workflows/publish-images.yml
pin=.github/rollback-bridge-reviewed-forward-commit
scratch="$(mktemp -d -t rollback-bridge-trigger.XXXXXXXXXX)"
trap 'rm -rf "$scratch"' EXIT
gate_script="$scratch/release-gate.sh"

awk '
  $0 == "      - name: Require an exact reviewed publication trigger" {
    in_step = 1
    next
  }
  in_step && $0 == "        run: |" {
    capture = 1
    next
  }
  capture && /^  [^[:space:]]/ { exit }
  capture {
    sub(/^          /, "")
    print
  }
' "$publisher" >"$gate_script"
# shellcheck disable=SC2016 # Extracted workflow shell is intentionally matched literally.
grep -qF 'case "$TRIGGER_EVENT_NAME" in' "$gate_script" || {
  echo "could not extract rollback bridge publication release gate" >&2
  exit 1
}

bridge="$(git rev-parse HEAD)"
forward="$(tr -d '\r\n' < "$pin")"
now="$(date +%s)"

curl() {
  printf '{"ref":"%s","object":{"type":"%s","sha":"%s"}}\n' \
    "${MOCK_REF:?}" "${MOCK_OBJECT_TYPE:?}" "${MOCK_OBJECT_SHA:?}"
}
export -f curl

run_gate() {
  GITHUB_OUTPUT=/dev/null \
    TRIGGER_API_URL=https://api.github.invalid \
    TRIGGER_REPOSITORY=TipLink/centaur \
    GITHUB_API_TOKEN=test-token \
    bash "$gate_script"
}

manual() {
  TRIGGER_EVENT_NAME=workflow_dispatch \
    TRIGGER_REF=refs/heads/test \
    TRIGGER_REF_CREATED='' \
    TRIGGER_REF_NAME=test \
    TRIGGER_REF_TYPE=branch \
    TRIGGER_SHA="$bridge" \
    LIVE_UPDATER_SCOPE_VERIFIED="$1" \
    DISPATCH_FORWARD_COMMIT="$2" \
    run_gate
}

push_tag() {
  local tag="$1"
  local created="$2"
  TRIGGER_EVENT_NAME=push \
    TRIGGER_REF="refs/tags/${tag}" \
    TRIGGER_REF_CREATED="$created" \
    TRIGGER_REF_NAME="$tag" \
    TRIGGER_REF_TYPE=tag \
    TRIGGER_SHA="$bridge" \
    LIVE_UPDATER_SCOPE_VERIFIED='' \
    DISPATCH_FORWARD_COMMIT='' \
    run_gate
}

expect_reject() {
  local label="$1"
  shift
  if "$@" >"$scratch/${label}.out" 2>&1; then
    echo "publication release gate unexpectedly accepted: $label" >&2
    exit 1
  fi
}

manual true "$forward"
expect_reject manual-false manual false "$forward"
wrong_forward="$(printf '0%.0s' {1..40})"
expect_reject manual-wrong-forward manual true "$wrong_forward"

valid="rollback-bridge-publish-live-scope-verified-${bridge}-forward-${forward}-at-${now}"
export MOCK_REF="refs/tags/${valid}" MOCK_OBJECT_TYPE=commit MOCK_OBJECT_SHA="$bridge"
push_tag "$valid" true

export MOCK_OBJECT_TYPE=tag
expect_reject annotated-remote-ref push_tag "$valid" true
export MOCK_OBJECT_TYPE=commit
expect_reject reused-or-updated-ref push_tag "$valid" false
wrong_sha="$(printf '0%.0s' {1..40})"
export MOCK_OBJECT_SHA="$wrong_sha"
expect_reject wrong-remote-target push_tag "$valid" true
export MOCK_OBJECT_SHA="$bridge"

wrong_forward_tag="rollback-bridge-publish-live-scope-verified-${bridge}-forward-${wrong_forward}-at-${now}"
export MOCK_REF="refs/tags/${wrong_forward_tag}"
expect_reject tag-wrong-forward push_tag "$wrong_forward_tag" true

stale="rollback-bridge-publish-live-scope-verified-${bridge}-forward-${forward}-at-$((now - 901))"
export MOCK_REF="refs/tags/${stale}"
expect_reject stale-timestamp push_tag "$stale" true

future="rollback-bridge-publish-live-scope-verified-${bridge}-forward-${forward}-at-$((now + 180))"
export MOCK_REF="refs/tags/${future}"
expect_reject future-timestamp push_tag "$future" true

oversized="rollback-bridge-publish-live-scope-verified-${bridge}-forward-${forward}-at-10000000000"
export MOCK_REF="refs/tags/${oversized}"
expect_reject overflow-timestamp push_tag "$oversized" true

echo "rollback bridge publication trigger tests passed"
