#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'

chart_dir=${1:-contrib/chart}
scratch=$(mktemp -d -t centaur-overlay-chart.XXXXXXXXXX)
trap 'rm -rf "$scratch"' EXIT

common=(
    --set apiRs.enabled=true
    --set console.enabled=false
    --set slackbotv2.enabled=false
    --set ironProxy.enabled=false
)

helm template test "$chart_dir" "${common[@]}" >"$scratch/default.yaml"
if grep -q 'name: overlay-bootstrap' "$scratch/default.yaml"; then
    echo "overlay image bootstrap must stay disabled by default" >&2
    exit 1
fi
if ! grep -qF 'name: test-centaur-legacy-managed-by-api-server' "$scratch/default.yaml"; then
    echo "legacy sandbox API access must remain enabled during staged rollout" >&2
    exit 1
fi
if [[ "$(grep -cF 'operator: DoesNotExist' "$scratch/default.yaml")" -lt 2 ]]; then
    echo "legacy API access must exclude new capability-labeled restricted pods" >&2
    exit 1
fi
if grep -qF 'name: test-centaur-otlp-egress' "$scratch/default.yaml"; then
    echo "OTLP egress must remain disabled by default" >&2
    exit 1
fi

helm template test "$chart_dir" "${common[@]}" \
    --set networkPolicy.otlpEgress.enabled=true \
    --set networkPolicy.otlpEgress.namespace=laminar \
    --set networkPolicy.otlpEgress.port=8000 >"$scratch/otlp.yaml"
for expected in \
    'name: test-centaur-otlp-egress' \
    'centaur.ai/observability-enabled: "true"' \
    'kubernetes.io/metadata.name: "laminar"' \
    'port: 8000'; do
    if ! grep -qF "$expected" "$scratch/otlp.yaml"; then
        echo "enabled OTLP policy is missing: $expected" >&2
        exit 1
    fi
done

helm template test "$chart_dir" "${common[@]}" \
    --set networkPolicy.legacyManagedByApiServerAccess=false >"$scratch/no-legacy.yaml"
if grep -qF 'legacy-managed-by-api-server' "$scratch/no-legacy.yaml"; then
    echo "legacy sandbox API access flag did not disable the transition policy" >&2
    exit 1
fi

helm template test "$chart_dir" "${common[@]}" \
    --set repoCache.enabled=false \
    --set overlay.image.repository=ghcr.io/tiplink/overlay \
    --set overlay.image.tag=sha-test >"$scratch/overlay.yaml"

for expected in \
    'name: overlay-bootstrap' \
    'image: "ghcr.io/tiplink/overlay:sha-test"' \
    'name: CENTAUR_OVERLAY_IMAGE' \
    'name: CENTAUR_SANDBOX_OVERLAY_DIR' \
    'name: TOOLS_OVERLAY_PATH' \
    'name: overlay-root'; do
    if ! grep -qF "$expected" "$scratch/overlay.yaml"; then
        echo "rendered chart is missing transitional overlay contract: $expected" >&2
        exit 1
    fi
done

if ! grep -qF 'value: "/app/tools:/app/overlay/org/tools"' "$scratch/overlay.yaml"; then
    echo "legacy image tools must remain available when no repo-cache tool source is configured" >&2
    exit 1
fi
if ! grep -qF 'value: "/app/workflows:/app/overlay/org/workflows"' "$scratch/overlay.yaml"; then
    echo "legacy image workflows must remain available when no repo-cache workflow source is configured" >&2
    exit 1
fi
if ! grep -qF 'value: "/home/agent/overlay/org/workflows"' "$scratch/overlay.yaml"; then
    echo "legacy sandbox image workflows must remain available when no repo-cache workflow source is configured" >&2
    exit 1
fi

helm template test "$chart_dir" "${common[@]}" \
    --set repoCache.enabled=true \
    --set-string 'apiRs.workflowApiAllowedNames=reminder\,compliance_cdd_research' \
    --set overlay.image.repository=ghcr.io/tiplink/overlay \
    --set overlay.image.tag=sha-test \
    --set-string 'overlays.sources[0].repo=paradigmxyz/centaur' \
    --set-string 'overlays.sources[1].repo=TipLink/fineas-centaur-overlay' \
    >"$scratch/repo-and-image-overlay.yaml"

if ! grep -qF 'value: "reminder,compliance_cdd_research"' "$scratch/repo-and-image-overlay.yaml"; then
    echo "sandbox workflow API allowlist must render into api-rs" >&2
    exit 1
fi

if ! grep -qF 'value: "/app/overlay/org/tools:/var/lib/centaur/repos/paradigmxyz/centaur/tools:/var/lib/centaur/repos/TipLink/fineas-centaur-overlay/tools"' "$scratch/repo-and-image-overlay.yaml"; then
    echo "repo-cache tools must follow and therefore override transitional image tools" >&2
    exit 1
fi
if ! grep -qF 'value: "/var/lib/centaur/repos/paradigmxyz/centaur/workflows:/var/lib/centaur/repos/TipLink/fineas-centaur-overlay/workflows"' "$scratch/repo-and-image-overlay.yaml"; then
    echo "API workflow discovery must use repo-cache sources exclusively when they are configured" >&2
    exit 1
fi
if ! grep -qF 'value: "/home/agent/github/paradigmxyz/centaur/workflows:/home/agent/github/TipLink/fineas-centaur-overlay/workflows"' "$scratch/repo-and-image-overlay.yaml"; then
    echo "sandbox workflow discovery must use repo-cache sources exclusively when they are configured" >&2
    exit 1
fi
if grep -F 'value: "/var/lib/centaur/repos/' "$scratch/repo-and-image-overlay.yaml" | grep -qF '/app/overlay/org/workflows'; then
    echo "API workflow discovery must not combine repo-cache and duplicate image workflow trees" >&2
    exit 1
fi
if grep -F 'value: "/home/agent/github/' "$scratch/repo-and-image-overlay.yaml" | grep -qF '/home/agent/overlay/org/workflows'; then
    echo "sandbox workflow discovery must not combine repo-cache and duplicate image workflow trees" >&2
    exit 1
fi

# A skills-only source never enters KUBERNETES_TOOLS_*, but its immutable ref
# must still rotate the sandbox content revision and therefore the warm key.
for revision in 1111111111111111111111111111111111111111 2222222222222222222222222222222222222222; do
    helm template test "$chart_dir" "${common[@]}" \
        --set repoCache.enabled=true \
        --set-string 'overlays.sources[0].repo=TipLink/fin-skills' \
        --set-string "overlays.sources[0].ref=$revision" \
        --set-string 'overlays.sources[0].toolsSubdir=' \
        --set-string 'overlays.sources[0].workflowsSubdir=' \
        --set-string 'overlays.sources[0].skillsSubdir=centaur-skills' \
        >"$scratch/skills-$revision.yaml"
done
content_revision() {
    awk '
      $1 == "-" && $2 == "name:" && $3 == "CENTAUR_SANDBOX_CONTENT_REVISION" {
        getline
        value = $2
        gsub(/^"|"$/, "", value)
        print value
        exit
      }
    ' "$1"
}
first_content_revision="$(content_revision "$scratch/skills-1111111111111111111111111111111111111111.yaml")"
second_content_revision="$(content_revision "$scratch/skills-2222222222222222222222222222222222222222.yaml")"
if [[ ! "$first_content_revision" =~ ^[0-9a-f]{64}$ || ! "$second_content_revision" =~ ^[0-9a-f]{64}$ ]]; then
    echo "sandbox content revisions must be rendered SHA-256 values" >&2
    exit 1
fi
if [[ "$first_content_revision" == "$second_content_revision" ]]; then
    echo "skills-only source ref did not rotate the sandbox content revision" >&2
    exit 1
fi

helm template test "$chart_dir" "${common[@]}" \
    --set console.enabled=true >"$scratch/control-api-auth.yaml"
if ! grep -qF 'name: CENTAUR_CONTROL_API_KEY' "$scratch/control-api-auth.yaml"; then
    echo "api-rs must receive the trusted Console control key" >&2
    exit 1
fi
if [[ "$(grep -cF 'name: CENTAUR_CONSOLE_CENTAUR_API_KEY' "$scratch/control-api-auth.yaml")" -lt 2 ]]; then
    echo "Console web and worker must receive the authenticated Centaur API client key" >&2
    exit 1
fi

helm template test "$chart_dir" "${common[@]}" \
    --set console.enabled=true \
    --set-string secretManager.envPrefix=PREFIX_ >"$scratch/prefixed-control-api-auth.yaml"
for expected in \
    'name: CENTAUR_CONTROL_API_KEY' \
    'key: PREFIX_CENTAUR_CONTROL_API_KEY' \
    'name: SLACKBOT_API_KEY' \
    'key: PREFIX_SLACKBOT_API_KEY' \
    'name: SLACK_FEEDBACK_API_KEY' \
    'key: PREFIX_SLACK_FEEDBACK_API_KEY' \
    'name: WORKFLOW_API_KEY' \
    'key: PREFIX_WORKFLOW_API_KEY' \
    'name: FIREWALL_MANAGER_SECRET_ENV_PREFIX' \
    'value: "PREFIX_"' \
    'name: CENTAUR_CONSOLE_CENTAUR_API_KEY'; do
    if ! grep -qF "$expected" "$scratch/prefixed-control-api-auth.yaml"; then
        echo "prefixed control API auth render is missing: $expected" >&2
        exit 1
    fi
done

helm template test "$chart_dir" "${common[@]}" \
    --set console.enabled=true \
    --set tokenBroker.githubApp.enabled=true \
    --set tokenBroker.githubApp.existingSecretName=fineas-github-app \
    >"$scratch/github-app.yaml"
for expected in \
    'name: github-app-broker-bootstrap' \
    'grant: "github_app_installation"' \
    'name: GITHUB_APP_ID' \
    'value: "github-app"' \
    'name: github-app-broker-secret' \
    'secretName: "fineas-github-app"'; do
    if ! grep -qF "$expected" "$scratch/github-app.yaml"; then
        echo "rendered chart is missing GitHub App broker bootstrap contract: $expected" >&2
        exit 1
    fi
done
