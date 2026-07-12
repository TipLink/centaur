#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'

resolver=.github/scripts/resolve-runnable-image-digest.sh
scratch="$(mktemp -d -t runnable-image-digest.XXXXXXXXXX)"
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/bin"

cat >"$scratch/bin/docker" <<'MOCK_DOCKER'
#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'

if [[ "$#" -ne 5 || "$1" != "buildx" || "$2" != "imagetools" ||
  "$3" != "inspect" || "$5" != "--raw" ]]; then
  echo "unexpected mocked docker invocation: $*" >&2
  exit 97
fi

case "$4" in
  "$MOCK_INDEX_REF")
    printf '%s\n' "$MOCK_INDEX_JSON"
    ;;
  "$MOCK_CHILD_REF")
    if [[ "$MOCK_CHILD_PULLABLE" != "true" ]]; then
      exit 98
    fi
    printf '{"mediaType":"application/vnd.oci.image.manifest.v1+json"}\n'
    ;;
  *)
    echo "unexpected mocked image reference: $4" >&2
    exit 99
    ;;
esac
MOCK_DOCKER
chmod +x "$scratch/bin/docker"

digest_arm64="sha256:$(printf 'a%.0s' {1..64})"
digest_amd64="sha256:$(printf 'b%.0s' {1..64})"
digest_attestation="sha256:$(printf 'c%.0s' {1..64})"
MOCK_INDEX_REF="ghcr.io/tiplink/centaur/example@sha256:$(printf 'd%.0s' {1..64})"
export MOCK_INDEX_REF
export MOCK_CHILD_REF="ghcr.io/tiplink/centaur/example@${digest_arm64}"
export MOCK_CHILD_PULLABLE=true
export PATH="$scratch/bin:$PATH"

make_index() {
  jq -cn \
    --arg arm64 "$digest_arm64" \
    --arg amd64 "$digest_amd64" \
    --arg attestation "$digest_attestation" '
      {
        mediaType: "application/vnd.oci.image.index.v1+json",
        manifests: [
          {
            mediaType: "application/vnd.oci.image.manifest.v1+json",
            digest: $arm64,
            platform: {os: "linux", architecture: "arm64"}
          },
          {
            mediaType: "application/vnd.oci.image.manifest.v1+json",
            digest: $amd64,
            platform: {os: "linux", architecture: "amd64"}
          },
          {
            mediaType: "application/vnd.oci.image.manifest.v1+json",
            digest: $attestation,
            platform: {os: "unknown", architecture: "unknown"},
            annotations: {"vnd.docker.reference.type": "attestation-manifest"}
          }
        ]
      }
    '
}

expect_reject() {
  local label=$1
  shift
  if "$@" >"$scratch/${label}.out" 2>&1; then
    echo "runnable digest resolver unexpectedly accepted: $label" >&2
    exit 1
  fi
}

MOCK_INDEX_JSON="$(make_index)"
export MOCK_INDEX_JSON
actual="$(bash "$resolver" "$MOCK_INDEX_REF" linux arm64)"
[[ "$actual" == "$digest_arm64" ]] || {
  echo "resolver returned $actual instead of $digest_arm64" >&2
  exit 1
}

duplicate_json="$(jq --argjson duplicate "$(jq -c '.manifests[0]' <<<"$MOCK_INDEX_JSON")" \
  '.manifests += [$duplicate]' <<<"$MOCK_INDEX_JSON")"
export MOCK_INDEX_JSON="$duplicate_json"
expect_reject duplicate-platform-child bash "$resolver" "$MOCK_INDEX_REF" linux arm64

MOCK_INDEX_JSON="$(jq '.manifests |= map(select(.platform.architecture != "arm64"))' <<<"$(make_index)")"
export MOCK_INDEX_JSON
expect_reject missing-platform-child bash "$resolver" "$MOCK_INDEX_REF" linux arm64

MOCK_INDEX_JSON="$(jq '.manifests[0].digest = "sha256:not-a-digest"' <<<"$(make_index)")"
export MOCK_INDEX_JSON
expect_reject invalid-child-digest bash "$resolver" "$MOCK_INDEX_REF" linux arm64

export MOCK_INDEX_JSON='{"mediaType":"application/vnd.oci.image.manifest.v1+json"}'
expect_reject direct-manifest bash "$resolver" "$MOCK_INDEX_REF" linux arm64

MOCK_INDEX_JSON="$(jq '.mediaType = "application/example.index.v1+json"' <<<"$(make_index)")"
export MOCK_INDEX_JSON
expect_reject unsupported-index-media-type bash "$resolver" "$MOCK_INDEX_REF" linux arm64

MOCK_INDEX_JSON="$(make_index)"
export MOCK_INDEX_JSON
export MOCK_CHILD_PULLABLE=false
expect_reject inaccessible-child bash "$resolver" "$MOCK_INDEX_REF" linux arm64
export MOCK_CHILD_PULLABLE=true

expect_reject unpinned-index bash "$resolver" ghcr.io/tiplink/centaur/example:latest linux arm64
expect_reject invalid-platform bash "$resolver" "$MOCK_INDEX_REF" 'Linux!' arm64

echo "runnable image digest resolver tests passed"
