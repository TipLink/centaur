#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'

verifier=.github/scripts/verify-reviewed-image-release.sh
scratch="$(mktemp -d -t reviewed-image-release.XXXXXXXXXX)"
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/bin"

MOCK_SHA="$(printf 'a%.0s' {1..40})"
export MOCK_SHA
export MOCK_MODE=valid
export MOCK_TAG_NAME=''

cat >"$scratch/bin/git" <<'MOCK_GIT'
#!/usr/bin/env bash
set -euo pipefail
if [[ "$*" == "rev-parse HEAD" ]]; then
  printf '%s\n' "${MOCK_SHA:?}"
else
  echo "unexpected mocked git invocation: $*" >&2
  exit 97
fi
MOCK_GIT

cat >"$scratch/bin/curl" <<'MOCK_CURL'
#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'
url=${!#}
base='https://api.github.test/repos/TipLink/centaur'

case "$url" in
  "$base/git/ref/tags/"*)
    jq -cn --arg ref "refs/tags/${MOCK_TAG_NAME:?}" --arg sha "${MOCK_SHA:?}" \
      '{ref:$ref,object:{type:"commit",sha:$sha}}'
    ;;
  "$base/git/commits/${MOCK_SHA}")
    if [[ "${MOCK_MODE:?}" == "bad-signature" ]]; then
      jq -cn --arg sha "$MOCK_SHA" '{sha:$sha,verification:{verified:false,reason:"unsigned"}}'
    else
      jq -cn --arg sha "$MOCK_SHA" '{sha:$sha,verification:{verified:true,reason:"valid"}}'
    fi
    ;;
  "$base/commits/${MOCK_SHA}/pulls")
    draft=false
    if [[ "$MOCK_MODE" == "draft" ]]; then draft=true; fi
    head_repo='TipLink/centaur'
    if [[ "$MOCK_MODE" == "fork-pr" ]]; then head_repo='untrusted/centaur'; fi
    jq -cn --arg sha "$MOCK_SHA" --argjson draft "$draft" --arg head_repo "$head_repo" \
      '[{number:76,state:"open",merged_at:null,draft:$draft,base:{ref:"main",repo:{full_name:"TipLink/centaur"}},head:{sha:$sha,repo:{full_name:$head_repo}}}]'
    ;;
  "$base/commits/${MOCK_SHA}/check-runs?filter=latest&per_page=100")
    ci_conclusion=success
    if [[ "$MOCK_MODE" == "failed-check" ]]; then ci_conclusion=failure; fi
    jq -cn --arg sha "$MOCK_SHA" --arg ci "$ci_conclusion" '{check_runs:[
      {name:"CI success",head_sha:$sha,status:"completed",conclusion:$ci,completed_at:"2026-07-12T00:00:03Z",details_url:"https://github.com/TipLink/centaur/actions/runs/11/job/101",app:{slug:"github-actions"}},
      {name:"Console CI success",head_sha:$sha,status:"completed",conclusion:"success",completed_at:"2026-07-12T00:00:02Z",details_url:"https://github.com/TipLink/centaur/actions/runs/12/job/102",app:{slug:"github-actions"}},
      {name:"Image validation success",head_sha:$sha,status:"completed",conclusion:"success",completed_at:"2026-07-12T00:00:01Z",details_url:"https://github.com/TipLink/centaur/actions/runs/13/job/103",app:{slug:"github-actions"}}
    ]}'
    ;;
  "$base/actions/runs/11")
    pr=76
    if [[ "$MOCK_MODE" == "wrong-pr-run" ]]; then pr=75; fi
    jq -cn --arg sha "$MOCK_SHA" --argjson pr "$pr" '{head_sha:$sha,event:"pull_request",status:"completed",conclusion:"success",path:".github/workflows/ci.yml",pull_requests:[{number:$pr,head:{sha:$sha}}]}'
    ;;
  "$base/actions/runs/12")
    path='.github/workflows/console-ci.yml'
    if [[ "$MOCK_MODE" == "wrong-workflow" ]]; then path='.github/workflows/codeql.yml'; fi
    jq -cn --arg sha "$MOCK_SHA" --arg path "$path" '{head_sha:$sha,event:"pull_request",status:"completed",conclusion:"success",path:$path,pull_requests:[{number:76,head:{sha:$sha}}]}'
    ;;
  "$base/actions/runs/13")
    jq -cn --arg sha "$MOCK_SHA" '{head_sha:$sha,event:"pull_request",status:"completed",conclusion:"success",path:".github/workflows/validate-images.yml",pull_requests:[{number:76,head:{sha:$sha}}]}'
    ;;
  *)
    echo "unexpected mocked curl URL: $url" >&2
    exit 98
    ;;
esac
MOCK_CURL
chmod +x "$scratch/bin/git" "$scratch/bin/curl"

export PATH="$scratch/bin:$PATH"
export GITHUB_API_TOKEN=not-a-real-token
export TRIGGER_API_URL=https://api.github.test
export TRIGGER_EVENT_NAME=workflow_dispatch
export TRIGGER_REF=refs/heads/reviewed
export TRIGGER_REF_NAME=reviewed
export TRIGGER_REF_TYPE=branch
export TRIGGER_REPOSITORY=TipLink/centaur
export TRIGGER_SHA="$MOCK_SHA"
export DISPATCH_REVIEWED_COMMIT="$MOCK_SHA"

expect_reject() {
  local label=$1
  shift
  if "$@" >"$scratch/${label}.out" 2>&1; then
    echo "reviewed release verifier unexpectedly accepted: $label" >&2
    exit 1
  fi
}

bash "$verifier" >"$scratch/valid.out"
grep -qF 'OK reviewed signed PR #76' "$scratch/valid.out"

export MOCK_MODE=bad-signature
expect_reject bad-signature bash "$verifier"
export MOCK_MODE=draft
expect_reject draft-pr bash "$verifier"
export MOCK_MODE=failed-check
expect_reject failed-check bash "$verifier"
export MOCK_MODE=wrong-workflow
expect_reject wrong-workflow bash "$verifier"
export MOCK_MODE=wrong-pr-run
expect_reject wrong-pr-run bash "$verifier"
export MOCK_MODE=fork-pr
expect_reject fork-pr bash "$verifier"

export MOCK_MODE=valid
DISPATCH_REVIEWED_COMMIT="$(printf 'b%.0s' {1..40})"
export DISPATCH_REVIEWED_COMMIT
expect_reject mismatched-dispatch-commit bash "$verifier"

export DISPATCH_REVIEWED_COMMIT="$MOCK_SHA"
export TRIGGER_EVENT_NAME=push
export TRIGGER_REF_TYPE=tag
export TRIGGER_REF_CREATED=true
attested_at="$(date +%s)"
export MOCK_TAG_NAME="reviewed-images-publish-${MOCK_SHA}-at-${attested_at}"
export TRIGGER_REF_NAME="$MOCK_TAG_NAME"
export TRIGGER_REF="refs/tags/${MOCK_TAG_NAME}"
bash "$verifier" >"$scratch/valid-tag.out"
grep -qF 'OK reviewed signed PR #76' "$scratch/valid-tag.out"

export TRIGGER_REF_CREATED=false
expect_reject moved-tag bash "$verifier"

echo "reviewed image release gate tests passed"
