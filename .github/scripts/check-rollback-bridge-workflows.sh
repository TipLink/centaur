#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'

publisher=.github/workflows/publish-images.yml
validator=.github/workflows/validate-images.yml
ci=.github/workflows/ci.yml
pin=.github/rollback-bridge-reviewed-forward-commit

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

grep -q '^  workflow_dispatch:$' "$publisher" || fail "publisher must be manual-only"
if grep -Eq '^  (push|pull_request):' "$publisher"; then
  fail "publisher must not run on push or pull request events"
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
grep -Fq 'pattern: digests-*-linux-arm64' "$publisher" ||
  fail "release descriptor must download this run's arm64 digest artifacts"
grep -Fq 'if [[ "$digest" != "$built_digest" ]]' "$publisher" ||
  fail "release descriptor must bind each tagged arm64 digest to this run's build artifact"
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
grep -Fq '.github/rollback-bridge-reviewed-forward-commit' "$publisher" ||
  fail "publisher does not read the central reviewed forward commit pin"
placeholder="__REVIEWED_FORWARD_""COMMIT_REQUIRED__"
unexpected_placeholder_files="$(
  git grep -lF "$placeholder" -- .github services docs 2>/dev/null |
    grep -vxF "$pin" || true
)"
if [[ -n "$unexpected_placeholder_files" ]]; then
  fail "the unresolved forward commit placeholder may exist only in $pin"
fi

echo "rollback bridge workflow safety checks passed"
