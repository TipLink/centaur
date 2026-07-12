#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'

image_index_ref=${1:-}
platform_os=${2:-}
platform_architecture=${3:-}

if [[ ! "$image_index_ref" =~ @sha256:[0-9a-f]{64}$ ]]; then
  echo "image index reference must end in a full sha256 digest" >&2
  exit 1
fi
if [[ ! "$platform_os" =~ ^[a-z0-9]+$ ||
  ! "$platform_architecture" =~ ^[a-z0-9_]+$ ]]; then
  echo "platform OS and architecture must be non-empty lowercase identifiers" >&2
  exit 1
fi

index_json="$(docker buildx imagetools inspect "$image_index_ref" --raw)"
if ! jq -e '
  type == "object" and
  (
    .mediaType == "application/vnd.oci.image.index.v1+json" or
    .mediaType == "application/vnd.docker.distribution.manifest.list.v2+json"
  ) and
  (.manifests | type == "array")
' <<<"$index_json" >/dev/null; then
  echo "build output is not a parseable OCI/Docker image index: $image_index_ref" >&2
  exit 1
fi

runnable_digests=()
while IFS= read -r digest; do
  runnable_digests+=("$digest")
done < <(
  jq -r \
    --arg os "$platform_os" \
    --arg architecture "$platform_architecture" '
      .manifests[]
      | select(
          .platform.os == $os and
          .platform.architecture == $architecture and
          (
            .mediaType == "application/vnd.oci.image.manifest.v1+json" or
            .mediaType == "application/vnd.docker.distribution.manifest.v2+json"
          )
        )
      | .digest
    ' <<<"$index_json"
)

if [[ "${#runnable_digests[@]}" -ne 1 ]]; then
  echo "expected exactly one runnable ${platform_os}/${platform_architecture} child in $image_index_ref; found ${#runnable_digests[@]}" >&2
  exit 1
fi

runnable_digest=${runnable_digests[0]}
if [[ ! "$runnable_digest" =~ ^sha256:[0-9a-f]{64}$ ]]; then
  echo "index contains an invalid runnable child digest: $runnable_digest" >&2
  exit 1
fi

repository=${image_index_ref%@*}
docker buildx imagetools inspect "${repository}@${runnable_digest}" --raw >/dev/null

printf '%s\n' "$runnable_digest"
