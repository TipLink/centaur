#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'

verifier=.github/scripts/verify-reviewed-rollback-release.sh
scratch="$(mktemp -d -t reviewed-rollback-release.XXXXXXXXXX)"
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/bin"

MOCK_SHA="$(printf 'a%.0s' {1..40})"
export MOCK_SHA MOCK_MODE=valid

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
url=${!#}
base='https://api.github.test/repos/TipLink/centaur'

case "$url" in
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
    base_ref=main
    if [[ "$MOCK_MODE" == "wrong-base" ]]; then base_ref=release; fi
    state=open
    if [[ "$MOCK_MODE" == "closed-unmerged" ]]; then state=closed; fi
    duplicate=false
    if [[ "$MOCK_MODE" == "duplicate-pr" ]]; then duplicate=true; fi
    jq -cn --arg sha "$MOCK_SHA" --argjson draft "$draft" --arg head_repo "$head_repo" \
      --arg base_ref "$base_ref" --arg state "$state" --argjson duplicate "$duplicate" \
      '[{number:78,state:$state,merged_at:null,draft:$draft,base:{ref:$base_ref,repo:{full_name:"TipLink/centaur"}},head:{sha:$sha,repo:{full_name:$head_repo}}}] | if $duplicate then . + . else . end'
    ;;
  "$base/commits/${MOCK_SHA}/check-runs?filter=latest&per_page=100")
    ci=success
    if [[ "$MOCK_MODE" == "failed-check" ]]; then ci=failure; fi
    image_name='Image validation success'
    if [[ "$MOCK_MODE" == "missing-check" ]]; then image_name='unrelated check'; fi
    jq -cn --arg sha "$MOCK_SHA" --arg ci "$ci" --arg image_name "$image_name" '{check_runs:[
      {name:"CI success",head_sha:$sha,status:"completed",conclusion:$ci,completed_at:"2026-07-12T00:00:03Z",details_url:"https://github.com/TipLink/centaur/actions/runs/11/job/101",app:{slug:"github-actions"}},
      {name:"Console CI success",head_sha:$sha,status:"completed",conclusion:"success",completed_at:"2026-07-12T00:00:02Z",details_url:"https://github.com/TipLink/centaur/actions/runs/12/job/102",app:{slug:"github-actions"}},
      {name:$image_name,head_sha:$sha,status:"completed",conclusion:"success",completed_at:"2026-07-12T00:00:01Z",details_url:"https://github.com/TipLink/centaur/actions/runs/13/job/103",app:{slug:"github-actions"}},
      {name:"CodeQL",head_sha:$sha,status:"completed",conclusion:"failure",completed_at:"2026-07-12T00:00:04Z",details_url:"https://github.com/TipLink/centaur/actions/runs/14/job/104",app:{slug:"github-actions"}}
    ]}'
    ;;
  "$base/actions/runs/11")
    pr=78
    if [[ "$MOCK_MODE" == "wrong-pr-run" ]]; then pr=77; fi
    event=pull_request
    if [[ "$MOCK_MODE" == "wrong-event" ]]; then event=push; fi
    run_sha="$MOCK_SHA"
    if [[ "$MOCK_MODE" == "wrong-run-head" ]]; then run_sha="$(printf '0%.0s' {1..40})"; fi
    jq -cn --arg sha "$run_sha" --arg event "$event" --argjson pr "$pr" '{head_sha:$sha,event:$event,status:"completed",conclusion:"success",path:".github/workflows/ci.yml",pull_requests:[{number:$pr,head:{sha:$sha}}]}'
    ;;
  "$base/actions/runs/12")
    path='.github/workflows/console-ci.yml'
    if [[ "$MOCK_MODE" == "wrong-workflow" ]]; then path='.github/workflows/codeql.yml'; fi
    jq -cn --arg sha "$MOCK_SHA" --arg path "$path" '{head_sha:$sha,event:"pull_request",status:"completed",conclusion:"success",path:$path,pull_requests:[{number:78,head:{sha:$sha}}]}'
    ;;
  "$base/actions/runs/13")
    jq -cn --arg sha "$MOCK_SHA" '{head_sha:$sha,event:"pull_request",status:"completed",conclusion:"success",path:".github/workflows/validate-images.yml",pull_requests:[{number:78,head:{sha:$sha}}]}'
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
export TRIGGER_REPOSITORY=TipLink/centaur
export TRIGGER_SHA="$MOCK_SHA"

expect_reject() {
  local label=$1
  shift
  if "$@" >"$scratch/${label}.out" 2>&1; then
    echo "reviewed rollback release verifier unexpectedly accepted: $label" >&2
    exit 1
  fi
}

bash "$verifier" >"$scratch/valid.out"
grep -qF 'OK reviewed signed rollback bridge PR #78' "$scratch/valid.out"

for mode in \
  bad-signature \
  draft \
  fork-pr \
  wrong-base \
  closed-unmerged \
  duplicate-pr \
  failed-check \
  missing-check \
  wrong-workflow \
  wrong-pr-run \
  wrong-event \
  wrong-run-head; do
  export MOCK_MODE="$mode"
  expect_reject "$mode" bash "$verifier"
done

export MOCK_MODE=valid
TRIGGER_SHA="$(printf 'b%.0s' {1..40})"
export TRIGGER_SHA
expect_reject wrong-checkout bash "$verifier"

echo "reviewed rollback release gate tests passed"
