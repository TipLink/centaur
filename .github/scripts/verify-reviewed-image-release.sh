#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'

required_env=(
  GITHUB_API_TOKEN
  TRIGGER_API_URL
  TRIGGER_EVENT_NAME
  TRIGGER_REF
  TRIGGER_REF_NAME
  TRIGGER_REF_TYPE
  TRIGGER_REPOSITORY
  TRIGGER_SHA
)
for name in "${required_env[@]}"; do
  if [[ -z "${!name:-}" ]]; then
    echo "missing required environment variable: $name" >&2
    exit 2
  fi
done
unset name required_env

if [[ ! "$TRIGGER_REPOSITORY" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ||
  ! "$TRIGGER_SHA" =~ ^[0-9a-f]{40}$ ]]; then
  echo "invalid trigger repository or commit SHA" >&2
  exit 2
fi
checked_out_sha="$(git rev-parse HEAD)"
if [[ "$checked_out_sha" != "$TRIGGER_SHA" ]]; then
  echo "publication trigger SHA does not match the checked-out commit" >&2
  exit 1
fi

api_get() {
  curl --fail --silent --show-error \
    --header "Authorization: Bearer ${GITHUB_API_TOKEN}" \
    --header 'Accept: application/vnd.github+json' \
    --header 'X-GitHub-Api-Version: 2022-11-28' \
    "$1"
}

case "$TRIGGER_EVENT_NAME" in
  workflow_dispatch)
    if [[ "${DISPATCH_REVIEWED_COMMIT:-}" != "$TRIGGER_SHA" ]]; then
      echo "workflow dispatch reviewed_commit must match the exact checked-out commit" >&2
      exit 1
    fi
    ;;
  push)
    if [[ "$TRIGGER_REF_TYPE" != "tag" || "${TRIGGER_REF_CREATED:-}" != "true" ||
      "$TRIGGER_REF" != "refs/tags/${TRIGGER_REF_NAME}" ]]; then
      echo "publication must be triggered by a newly created tag ref" >&2
      exit 1
    fi
    tag_pattern='^reviewed-images-publish-([0-9a-f]{40})-at-([1-9][0-9]{9})$'
    if [[ ! "$TRIGGER_REF_NAME" =~ $tag_pattern ||
      "${BASH_REMATCH[1]:-}" != "$TRIGGER_SHA" ]]; then
      echo "publication tag must encode the exact reviewed commit" >&2
      exit 1
    fi
    attested_at="${BASH_REMATCH[2]}"
    now="$(date +%s)"
    if ((attested_at > now + 120 || now - attested_at > 900)); then
      echo "publication tag timestamp must be within the 900-second admission window" >&2
      exit 1
    fi
    encoded_ref="$(jq -nr --arg value "$TRIGGER_REF_NAME" '$value|@uri')"
    ref_json="$(api_get "${TRIGGER_API_URL}/repos/${TRIGGER_REPOSITORY}/git/ref/tags/${encoded_ref}")"
    if [[ "$(jq -er '.ref' <<<"$ref_json")" != "$TRIGGER_REF" ||
      "$(jq -er '.object.type' <<<"$ref_json")" != "commit" ||
      "$(jq -er '.object.sha' <<<"$ref_json")" != "$TRIGGER_SHA" ]]; then
      echo "publication tag must be a lightweight ref directly targeting the reviewed commit" >&2
      exit 1
    fi
    ;;
  *)
    echo "unsupported publication trigger: $TRIGGER_EVENT_NAME" >&2
    exit 1
    ;;
esac

commit_json="$(api_get "${TRIGGER_API_URL}/repos/${TRIGGER_REPOSITORY}/git/commits/${TRIGGER_SHA}")"
if [[ "$(jq -r '.sha' <<<"$commit_json")" != "$TRIGGER_SHA" ||
  "$(jq -r '.verification.verified' <<<"$commit_json")" != "true" ||
  "$(jq -r '.verification.reason' <<<"$commit_json")" != "valid" ]]; then
  echo "reviewed publication commit is not GitHub-signature verified" >&2
  exit 1
fi

pulls_json="$(api_get "${TRIGGER_API_URL}/repos/${TRIGGER_REPOSITORY}/commits/${TRIGGER_SHA}/pulls")"
reviewed_pr="$(jq -cer --arg sha "$TRIGGER_SHA" --arg repo "$TRIGGER_REPOSITORY" '
  [ .[]
    | select(.base.ref == "main")
    | select(.base.repo.full_name == $repo)
    | select(.head.sha == $sha)
    | select(.head.repo.full_name == $repo)
    | select(.draft == false)
    | select(.state == "open" or .merged_at != null)
  ]
  | if length == 1 then .[0] else error("expected exactly one ready or merged main PR at the trigger SHA") end
' <<<"$pulls_json")"
reviewed_pr_number="$(jq -er '.number' <<<"$reviewed_pr")"
reviewed_head_branch="$(jq -er '.head.ref' <<<"$reviewed_pr")"

checks_json="$(api_get "${TRIGGER_API_URL}/repos/${TRIGGER_REPOSITORY}/commits/${TRIGGER_SHA}/check-runs?filter=latest&per_page=100")"

require_workflow_check() {
  local check_name="$1"
  local workflow_path="$2"
  local check_json details_url run_id run_json
  check_json="$(jq -cer --arg name "$check_name" --arg sha "$TRIGGER_SHA" '
    [.check_runs[]
      | select(.name == $name and .head_sha == $sha and .app.slug == "github-actions")]
    | sort_by(.completed_at // .started_at // "")
    | if length > 0 then .[-1] else error("missing required GitHub Actions check") end
  ' <<<"$checks_json")"
  if [[ "$(jq -r '.status' <<<"$check_json")" != "completed" ||
    "$(jq -r '.conclusion' <<<"$check_json")" != "success" ]]; then
    echo "required check is not successful: $check_name" >&2
    exit 1
  fi
  details_url="$(jq -er '.details_url' <<<"$check_json")"
  if [[ ! "$details_url" =~ /actions/runs/([0-9]+)/job/ ]]; then
    echo "required check is not bound to an Actions workflow run: $check_name" >&2
    exit 1
  fi
  run_id="${BASH_REMATCH[1]}"
  run_json="$(api_get "${TRIGGER_API_URL}/repos/${TRIGGER_REPOSITORY}/actions/runs/${run_id}")"
  if [[ "$(jq -r '.head_sha' <<<"$run_json")" != "$TRIGGER_SHA" ||
    "$(jq -r '.event' <<<"$run_json")" != "pull_request" ||
    "$(jq -r '.status' <<<"$run_json")" != "completed" ||
    "$(jq -r '.conclusion' <<<"$run_json")" != "success" ||
    "$(jq -r '.path' <<<"$run_json")" != "$workflow_path" ||
    "$(jq -r '.head_repository.full_name' <<<"$run_json")" != "$TRIGGER_REPOSITORY" ||
    "$(jq -r '.head_branch' <<<"$run_json")" != "$reviewed_head_branch" ]]; then
    echo "required check is not a successful exact-head run of $workflow_path: $check_name" >&2
    exit 1
  fi
}

require_workflow_check "CI success" ".github/workflows/ci.yml"
require_workflow_check "Console CI success" ".github/workflows/console-ci.yml"
require_workflow_check "Image validation success" ".github/workflows/validate-images.yml"

echo "OK reviewed signed PR #${reviewed_pr_number} and exact-head non-CodeQL checks authorize image publication"
