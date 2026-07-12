#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'

repository=${1:-}
tag=${2:-}
registry=${REGISTRY:-ghcr.io}

if [[ ! "$repository" =~ ^[a-z0-9._-]+/[a-z0-9._/-]+$ ]]; then
  echo "repository must be a lowercase registry namespace/path" >&2
  exit 2
fi
if [[ ! "$tag" =~ ^reviewed-[0-9a-f]{40}$ ]]; then
  echo "tag must be reviewed- followed by the exact 40-character commit SHA" >&2
  exit 2
fi
if [[ -z "${GITHUB_ACTOR:-}" || -z "${GHCR_TOKEN:-}" ]]; then
  echo "GITHUB_ACTOR and GHCR_TOKEN are required" >&2
  exit 2
fi

token_json="$(curl --fail --silent --show-error \
  --user "${GITHUB_ACTOR}:${GHCR_TOKEN}" \
  --get \
  --data-urlencode "scope=repository:${repository}:pull" \
  --data-urlencode "service=${registry}" \
  "https://${registry}/token")"
registry_token="$(jq -er '.token' <<<"$token_json")"
response_body="$(mktemp -t reviewed-tag-response.XXXXXXXXXX)"
trap 'rm -f "$response_body"' EXIT

if ! status="$(curl --silent --show-error \
  --output "$response_body" \
  --write-out '%{http_code}' \
  --header "Authorization: Bearer ${registry_token}" \
  --header 'Accept: application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json' \
  "https://${registry}/v2/${repository}/manifests/${tag}")"; then
  echo "registry request failed while checking ${repository}:${tag}" >&2
  exit 1
fi

case "$status" in
  404)
    if ! jq -e 'any(.errors[]?; .code == "MANIFEST_UNKNOWN")' "$response_body" >/dev/null; then
      echo "registry returned HTTP 404 without MANIFEST_UNKNOWN for ${repository}:${tag}" >&2
      exit 1
    fi
    ;;
  200)
    echo "refusing to overwrite immutable reviewed tag: ${repository}:${tag}" >&2
    exit 1
    ;;
  *)
    echo "registry returned HTTP $status while checking ${repository}:${tag}" >&2
    exit 1
    ;;
esac

echo "OK reviewed tag is absent: ${repository}:${tag}"
