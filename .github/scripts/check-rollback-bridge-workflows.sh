#!/usr/bin/env bash
# shellcheck disable=SC2016 # Workflow expressions and shell variables below are literal guards.
set -euo pipefail
IFS=$'\n\t'

publisher=.github/workflows/publish-images.yml
validator=.github/workflows/validate-images.yml
ci=.github/workflows/ci.yml
pin=.github/rollback-bridge-reviewed-forward-commit
sandbox_dockerfile=services/sandbox/Dockerfile

fail() {
  echo "rollback bridge workflow safety check failed: $*" >&2
  exit 1
}

[[ "$(head -n 1 "$validator")" == "name: Publish Images" ]] ||
  fail "PR validator must preserve the historical Publish Images check context"
grep -q '^  pull_request:$' "$validator" || fail "PR validator must run on pull requests"
if grep -Eq '^    (branches|paths):' "$validator"; then
  fail "PR validator must be unfiltered so the historical required check is always reported"
fi
if grep -Eq '^  (push|workflow_dispatch):' "$validator"; then
  fail "PR validator must not have a publication trigger"
fi
grep -q '^  contents: read$' "$validator" || fail "PR validator must be read-only"
grep -q '^          push: false$' "$validator" || fail "PR validator must build with push disabled"
if grep -q 'docker/login-action' "$validator" || grep -q 'packages: write' "$validator"; then
  fail "PR validator must not receive registry credentials"
fi
grep -Fq 'service: [api-rs, slackbotv2, linearbot, discordbot, teamsbot, agent, iron-proxy, console]' "$validator" ||
  fail "PR validator must preserve the historical eight-image required-check matrix"
grep -q '^  agent-arm64:$' "$validator" ||
  fail "PR validator must build the rollback agent natively on arm64"
grep -Fq 'runs-on: ubuntu-24.04-arm' "$validator" ||
  fail "arm64 rollback validation must use the native GitHub arm runner"
grep -Fq 'centaur-agent:rollback-validate-linux-arm64' "$validator" ||
  fail "arm64 rollback validation must load and probe the packaged image"
grep -Fq '"$AGENT_BROWSER_EXECUTABLE_PATH" --version' "$validator" ||
  fail "arm64 rollback validation must execute the selected native browser"

grep -q '^  workflow_dispatch:$' "$publisher" ||
  fail "publisher must preserve the confirmed manual path"
expected_push_trigger=$'  push:\n    tags:\n      - '\''rollback-bridge-publish-live-scope-verified-*'\'''
actual_push_trigger="$(awk '
  $0 == "  push:" { capture = 1 }
  capture && /^[^[:space:]]/ { exit }
  capture { print }
' "$publisher")"
[[ "$actual_push_trigger" == "$expected_push_trigger" ]] ||
  fail "publisher push trigger must contain only the exact live-scope tag prefix"
if grep -Eq '^  (pull_request|create|schedule):' "$publisher"; then
  fail "publisher must not run for branches, pull requests, create events, or schedules"
fi
expected_tag_pattern='^rollback-bridge-publish-live-scope-verified-([0-9a-f]{40})-forward-([0-9a-f]{40})-at-([1-9][0-9]{9})$'
actual_tag_pattern="$(awk -F "'" '/^[[:space:]]*tag_pattern=/{ print $2 }' "$publisher")"
[[ "$actual_tag_pattern" == "$expected_tag_pattern" ]] ||
  fail "publisher tag parser must require exact full SHAs and exactly ten timestamp digits"
valid_trigger_tag="rollback-bridge-publish-live-scope-verified-$(printf 'a%.0s' {1..40})-forward-$(printf 'b%.0s' {1..40})-at-1234567890"
oversized_trigger_tag="${valid_trigger_tag}1"
[[ "$valid_trigger_tag" =~ $actual_tag_pattern ]] ||
  fail "publisher tag parser rejected its exact valid trigger shape"
if [[ "$oversized_trigger_tag" =~ $actual_tag_pattern ]]; then
  fail "publisher tag parser accepted an overflow-capable timestamp"
fi
grep -q '^      live_updater_scope_verified:$' "$publisher" ||
  fail "publisher must require an explicit live updater-scope acknowledgement"
grep -Fq 'LIVE_UPDATER_SCOPE_VERIFIED: ${{ inputs.live_updater_scope_verified }}' "$publisher" ||
  fail "publisher must bind the live updater-scope acknowledgement into its release gate"
grep -Fq 'none of the four exact bridge repositories is in image-list' "$publisher" ||
  fail "publisher must state the fail-closed live updater-scope proof"
if grep -qE 'image_updater_disabled|IMAGE_UPDATER_DISABLED' "$publisher"; then
  fail "publisher must not claim that global Image Updater disablement is a publication precondition"
fi
for required_trigger_guard in \
  'TRIGGER_REF_CREATED: ${{ github.event.created }}' \
  'git cat-file -t "$TRIGGER_SHA"' \
  '"$bridge_commit" != "$TRIGGER_SHA"' \
  '"$tag_bridge_commit" != "$bridge_commit"' \
  '"$tag_forward_commit" != "$commit"' \
  '/git/ref/tags/${encoded_ref}' \
  "'.object.type'" \
  "'.object.sha'" \
  'attested_at > now + 120' \
  'now - attested_at > 900'; do
  grep -Fq "$required_trigger_guard" "$publisher" ||
    fail "publisher is missing exact tag trigger guard: $required_trigger_guard"
done
if grep -Eq 'kubectl|KUBECONFIG|kubeconfig' "$publisher"; then
  fail "publisher must not receive Kubernetes access"
fi
grep -Fq 'service: [api-rs, slackbotv2, agent, iron-proxy]' "$publisher" ||
  fail "publisher build matrix must contain exactly the four rollback runtime images"
grep -q '^  cancel-in-progress: false$' "$publisher" ||
  fail "publisher dispatches must serialize rather than cancel an in-flight publication"
grep -q '^  group: publish-reviewed-rollback-bridge-images$' "$publisher" ||
  fail "publisher concurrency must serialize globally across refs"
grep -q '^    needs: tag-absence-gate$' "$publisher" ||
  fail "digest publication must wait for the reviewed-tag absence gate"
grep -q '^  tag-absence-gate:$' "$publisher" ||
  fail "publisher is missing the reviewed-tag absence gate"
grep -Fq 'case "$status" in' "$publisher" ||
  fail "reviewed-tag absence gate must distinguish explicit registry HTTP status"
if [[ "$(grep -Fc '.code == "MANIFEST_UNKNOWN"' "$publisher")" -ne 2 ]]; then
  fail "publisher must require explicit MANIFEST_UNKNOWN at both absence checks"
fi
if [[ "$(grep -Fc 'application/vnd.oci.image.manifest.v1+json' "$publisher")" -ne 2 ||
  "$(grep -Fc 'application/vnd.docker.distribution.manifest.v2+json' "$publisher")" -ne 2 ]]; then
  fail "both absence checks must negotiate single-platform and multi-platform manifests"
fi
grep -Fq 'refusing to overwrite immutable reviewed tag during final recheck' "$publisher" ||
  fail "each manifest publication must recheck tag absence immediately before creation"
if grep -Eq 'centaur-(linearbot|discordbot|teamsbot|console)' "$publisher"; then
  fail "publisher must not build or describe non-bridge images"
fi
grep -Fq 'type=raw,value=reviewed-${{ github.sha }}' "$publisher" ||
  fail "publisher must use the reviewed-full-commit tag namespace"
grep -Fq 'tag="reviewed-${RELEASE_REVISION}"' "$publisher" ||
  fail "release descriptor must use the reviewed-full-commit tag namespace"
grep -Fq 'pattern: merge-index-digests-${{ matrix.image }}-*' "$publisher" ||
  fail "manifest merge must consume this run's attested per-platform index digests"
grep -Fq 'working-directory: ${{ runner.temp }}/merge-index-digests' "$publisher" ||
  fail "manifest merge must preserve the attested index digest inputs"
grep -Fq 'if [[ "${#merge_index_digest_files[@]}" -ne 2 ]]' "$publisher" ||
  fail "manifest merge must require both attested platform index digests"
grep -Fq 'pattern: runnable-digests-*-linux-arm64' "$publisher" ||
  fail "release descriptor must download this run's runnable arm64 child digests"
grep -Fq 'resolve-runnable-image-digest.sh' "$publisher" ||
  fail "build must resolve each runnable platform child from its attested index"
grep -Fq 'if [[ "$digest" != "$run_child_digest" ]]' "$publisher" ||
  fail "release descriptor must bind each tagged arm64 child to this run's runnable child"
grep -Fq 'refusing to overwrite immutable reviewed tag' "$publisher" ||
  fail "publisher must refuse to overwrite an existing reviewed tag"
if grep -Eq 'type=sha|value=(latest|main|edge)|sha-\$\{|::7' "$publisher"; then
  fail "publisher contains a legacy, mutable, or shortened deploy-shaped tag"
fi

expected_descriptor_rows=$'api-rs\tcentaur-api-rs\nslackbotv2\tcentaur-slackbotv2\nsandbox\tcentaur-agent\niron-proxy\tcentaur-iron-proxy'
actual_descriptor_rows="$(awk '
  /done <<'"'"'COMPONENTS'"'"'/ { capture = 1; next }
  capture && /^[[:space:]]*COMPONENTS$/ { exit }
  capture { sub(/^[[:space:]]+/, ""); print }
' "$publisher")"
[[ "$actual_descriptor_rows" == "$expected_descriptor_rows" ]] ||
  fail "release descriptor rows do not exactly match the four infra rollback runtime rows"

grep -Fq '.github/rollback-bridge-reviewed-forward-commit' "$ci" ||
  fail "CI does not read the central reviewed forward commit pin"
mapfile -t integration_control_keys < <(
  awk -F ': ' '/^[[:space:]]+CENTAUR_CONTROL_API_KEY: / { print $2 }' "$ci"
)
[[ "${#integration_control_keys[@]}" -eq 2 ]] ||
  fail "CI must configure exactly two forward integration control keys"
for integration_control_key in "${integration_control_keys[@]}"; do
  [[ "${#integration_control_key}" -ge 32 ]] ||
    fail "CI contains a forward integration control key shorter than 32 bytes"
done
unset integration_control_key integration_control_keys

grep -Fq 'ENV AGENT_BROWSER_EXECUTABLE_PATH=/home/agent/.local/bin/centaur-agent-browser-chromium' \
  "$sandbox_dockerfile" ||
  fail "rollback sandbox must expose the reviewed native browser path"
grep -Fq 'amd64)' "$sandbox_dockerfile" ||
  fail "rollback sandbox must bind agent-browser Chrome on amd64"
grep -Fq 'arm64)' "$sandbox_dockerfile" ||
  fail "rollback sandbox must bind Playwright Chromium on arm64"
grep -Fq -- "-type f -path '*/chrome-linux/headless_shell'" "$sandbox_dockerfile" ||
  fail "rollback sandbox arm64 path must use the installed Playwright shell"
grep -Fq 'ln -sf "$browser" "$AGENT_BROWSER_EXECUTABLE_PATH"' "$sandbox_dockerfile" ||
  fail "rollback sandbox must create the stable browser executable link"
grep -Fq '"$AGENT_BROWSER_EXECUTABLE_PATH" --version' "$sandbox_dockerfile" ||
  fail "rollback sandbox build must execute the selected browser"

grep -Fq '.github/rollback-bridge-reviewed-forward-commit' "$publisher" ||
  fail "publisher does not read the central reviewed forward commit pin"
for resolver_path in \
  '^\.github/scripts/resolve-runnable-image-digest\.sh$' \
  '^\.github/scripts/test-resolve-runnable-image-digest\.sh$'; do
  grep -Fq "$resolver_path" "$ci" ||
    fail "CI change detection does not cover $resolver_path"
done
placeholder="__REVIEWED_FORWARD_""COMMIT_REQUIRED__"
unexpected_placeholder_files="$(
  git grep -lF "$placeholder" -- .github services docs 2>/dev/null |
    grep -vxF "$pin" || true
)"
if [[ -n "$unexpected_placeholder_files" ]]; then
  fail "the unresolved forward commit placeholder may exist only in $pin"
fi

bash .github/scripts/test-rollback-bridge-publication-trigger.sh
bash .github/scripts/test-resolve-runnable-image-digest.sh

echo "rollback bridge workflow safety checks passed"
