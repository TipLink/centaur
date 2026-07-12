#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'

# An explicitly mounted org repository is the live source of truth. The baked
# home prompt remains a rollback-safe fallback for deployments that have not
# moved to repo-backed overlays yet.
repo_prompt="${CENTAUR_OVERLAY_DIR:-}/services/sandbox/SYSTEM_PROMPT.md"
image_overlay_prompt="${CENTAUR_IMAGE_OVERLAY_DIR:-}/services/sandbox/SYSTEM_PROMPT.md"
image_prompt="${HOME}/AGENTS_OVERLAY.md"

if [[ -n "${CENTAUR_OVERLAY_DIR:-}" && -d "${CENTAUR_OVERLAY_DIR}" ]]; then
    # An available repo root is authoritative as a whole. If it intentionally
    # omits an overlay prompt, do not resurrect stale image-baked instructions.
    if [[ -f "$repo_prompt" ]]; then
        printf '%s\n' "$repo_prompt"
    fi
elif [[ -n "${CENTAUR_IMAGE_OVERLAY_DIR:-}" && -f "$image_overlay_prompt" ]]; then
    printf '%s\n' "$image_overlay_prompt"
elif [[ -f "$image_prompt" ]]; then
    printf '%s\n' "$image_prompt"
fi
