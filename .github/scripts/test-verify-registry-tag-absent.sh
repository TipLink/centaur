#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'

verifier=.github/scripts/verify-registry-tag-absent.sh
scratch="$(mktemp -d -t registry-tag-absent.XXXXXXXXXX)"
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/bin"

cat >"$scratch/bin/curl" <<'MOCK_CURL'
#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'

url=${!#}
if [[ "$url" == "https://ghcr.io/token" ]]; then
  printf '{"token":"mock-registry-token"}\n'
  exit 0
fi
if [[ "$url" != "https://ghcr.io/v2/tiplink/centaur/example/manifests/${MOCK_TAG:?}" ]]; then
  echo "unexpected mocked curl URL: $url" >&2
  exit 97
fi

output=''
previous=''
for argument in "$@"; do
  if [[ "$previous" == "--output" ]]; then
    output="$argument"
    break
  fi
  previous="$argument"
done
if [[ -z "$output" ]]; then
  echo "mocked manifest request omitted --output" >&2
  exit 98
fi

case "${MOCK_STATUS:?}" in
  404-known)
    printf '{"errors":[{"code":"MANIFEST_UNKNOWN"}]}\n' >"$output"
    printf '404'
    ;;
  404-unknown)
    printf '{"errors":[{"code":"DENIED"}]}\n' >"$output"
    printf '404'
    ;;
  200)
    printf '{}\n' >"$output"
    printf '200'
    ;;
  403)
    printf '{}\n' >"$output"
    printf '403'
    ;;
  *) exit 99 ;;
esac
MOCK_CURL
chmod +x "$scratch/bin/curl"

export PATH="$scratch/bin:$PATH"
export GITHUB_ACTOR=fineas-bot
export GHCR_TOKEN=not-a-real-token
MOCK_TAG="reviewed-$(printf 'a%.0s' {1..40})"
export MOCK_TAG

expect_reject() {
  local label=$1
  shift
  if "$@" >"$scratch/${label}.out" 2>&1; then
    echo "registry absence verifier unexpectedly accepted: $label" >&2
    exit 1
  fi
}

export MOCK_STATUS=404-known
bash "$verifier" tiplink/centaur/example "$MOCK_TAG" >"$scratch/safe.out"
grep -qF 'OK reviewed tag is absent' "$scratch/safe.out"

export MOCK_STATUS=200
expect_reject existing-tag bash "$verifier" tiplink/centaur/example "$MOCK_TAG"
grep -qF 'refusing to overwrite immutable reviewed tag' "$scratch/existing-tag.out"

export MOCK_STATUS=404-unknown
expect_reject ambiguous-404 bash "$verifier" tiplink/centaur/example "$MOCK_TAG"

export MOCK_STATUS=403
expect_reject forbidden bash "$verifier" tiplink/centaur/example "$MOCK_TAG"

expect_reject shortened-tag bash "$verifier" tiplink/centaur/example reviewed-aaaaaaa

echo "registry reviewed-tag absence tests passed"
