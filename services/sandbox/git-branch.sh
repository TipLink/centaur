#!/usr/bin/env bash
# git-branch — create a writable working copy from a read-only mounted repo.
#
# Usage:  git-branch <org/repo> <branch slug>
# Example: git-branch owner/centaur fix-flaky-slack-delivery
#
# Creates ~/branches/<org>/<repo> as a --shared clone from ~/github/<org>/<repo>
# with a unique agent branch checked out. The resulting directory is fully writable
# and supports commit, push, and PR workflows.

set -euo pipefail
IFS=$'\n\t'

usage() {
    echo "Usage: git-branch <org/repo> <branch slug>" >&2
    echo "Example: git-branch owner/centaur fix-flaky-slack-delivery" >&2
}

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    usage
    exit 0
fi

if [ $# -ne 2 ]; then
    usage
    echo "Error: branch slug is required; choose a short descriptive kebab-case name." >&2
    exit 1
fi

REPO="$1"
SLUG="$2"
SRC="$HOME/github/$REPO"
DEST="$HOME/branches/$REPO"

# Match commit authorship to the account that will publish the PR so GitHub does
# not preserve a separate sandbox identity as a squash-merge co-author.
configure_git_identity() {
    local name="${CENTAUR_GIT_USER_NAME:-}"
    local email="${CENTAUR_GIT_USER_EMAIL:-}"

    if [ -n "$name" ] || [ -n "$email" ]; then
        if [ -z "$name" ] || [ -z "$email" ]; then
            echo "Error: CENTAUR_GIT_USER_NAME and CENTAUR_GIT_USER_EMAIL" \
                "must be set together" >&2
            return 1
        fi
    fi

    if [ -n "$name" ] && [ -n "$email" ]; then
        git -C "$DEST" config user.name "$name"
        git -C "$DEST" config user.email "$email"
    elif ! git -C "$DEST" var GIT_AUTHOR_IDENT >/dev/null 2>&1; then
        echo "Warning: no Git author identity is configured; set" \
            "CENTAUR_GIT_USER_NAME and CENTAUR_GIT_USER_EMAIL before committing" >&2
    fi
}

# Sign commits locally when the runner provides a dedicated private key. Push
# authentication remains independent and continues to use GITHUB_TOKEN.
configure_git_signing() {
    local signing_key="${CENTAUR_GIT_SIGNING_KEY:-}"

    if [ -z "$signing_key" ]; then
        return
    fi
    if [ ! -f "$signing_key" ] || [ ! -r "$signing_key" ]; then
        echo "Error: CENTAUR_GIT_SIGNING_KEY must name a readable private key file" >&2
        return 1
    fi

    git -C "$DEST" config gpg.format ssh
    git -C "$DEST" config user.signingkey "$signing_key"
    git -C "$DEST" config commit.gpgsign true
}

configure_git() {
    configure_git_identity
    configure_git_signing
}

if [[ ! "$SLUG" =~ ^[a-z0-9][a-z0-9-]*[a-z0-9]$|^[a-z0-9]$ ]]; then
    usage
    echo "Error: branch slug must be lowercase kebab-case using only a-z, 0-9, and hyphens." >&2
    exit 1
fi

if [ ! -d "$SRC/.git" ] && ! git -C "$SRC" rev-parse --git-dir >/dev/null 2>&1; then
    echo "Error: $SRC is not a valid git repository" >&2
    exit 1
fi

if [ -d "$DEST/.git" ]; then
    echo "$DEST already exists — reusing" >&2
    configure_git
    echo "$DEST"
    exit 0
fi

mkdir -p "$(dirname "$DEST")"

if ! git clone --quiet --shared "$SRC" "$DEST"; then
    echo "shared clone failed; retrying with regular clone" >&2
    rm -rf "$DEST"
    git clone --quiet "$SRC" "$DEST"
fi

# --shared clones set origin to the local path; fix it to the upstream URL
# so that git push and gh pr create target the real GitHub remote.
UPSTREAM_URL=$(git -C "$SRC" config --get remote.origin.url 2>/dev/null || echo "")
if [ -n "$UPSTREAM_URL" ]; then
    git -C "$DEST" remote set-url origin "$UPSTREAM_URL"
fi

BRANCH="centaur/$SLUG-$(date +%s)"
git -C "$DEST" checkout -q -b "$BRANCH"

configure_git

echo "$DEST"
