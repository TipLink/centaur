# TipLink Centaur upstream alignment (2026-07-11)

This branch merges TipLink `ba2c01f5` with Paradigm `3c6e84d9` while preserving
both histories. Conflict resolution used the upstream implementation as the
baseline, then reapplied only TipLink behavior that remains deployment- or
runtime-relevant.

## Retained on the current upstream architecture

| TipLink behavior | Current implementation |
| --- | --- |
| Ordered tool overlays | `install_tool_shims.py` uses later-source replacement and matches package, project, and script identifiers for allow/block lists. |
| Warm first-turn context | `harness-server` derives `CENTAUR_THREAD_KEY` from the first blocks-mode user line before spawning Codex app-server. |
| Active Codex steering | A second user line during an active turn becomes `turn/steer`; its response is consumed rather than leaked into blocks output. |
| Canonical session release | `POST /api/session/{thread_key}/release` locks the session row, fences the expected sandbox, rejects active work unless cancellation is explicit, clears stdout ownership on cancellation, and stops only the snapshotted sandbox. |
| Ambient Slack channels | Configured root messages and replies execute without an explicit mention; messages outside the allowlist remain inert. |
| Slack event dedupe | The patched Chat dependency dedupes by actionable bucket so a non-actionable `message` event cannot suppress a later `app_mention`. |
| Durable terminal reconciliation | Slack compares streamed markdown with the durable terminal result and replaces divergent output. |
| Durable Slack delivery proof | After Slack confirms a primary, reconciled, fallback, or visible-error message, Slackbot records one idempotent `session.delivery_completed` event through a Slackbot-key-only route bound to the exact thread and execution. |
| Generic HTTP secret scopes | Method/path scopes are retained through discovery, permission translation, and iron-control registration. |
| GitHub App installation tokens | The grant is registered in upstream `Broker::CredentialGrants`; the model delegates validation and refresh to that registry. Helm bootstraps the canonical credential before api-rs starts, and the built-in infra role grants a scheme-preserving `GITHUB_TOKEN` replacement for `github.com` and `api.github.com` to sandbox principals. |
| Fork image publication | TipLink GHCR namespace, GitHub-hosted native builders, safe multi-arch assembly, GitHub App-authored Fineas infra promotion, and upstream `githubbot`/harness image inputs are combined. |

## Transitional deployment compatibility

- `overlay.image` is deprecated for new deployments but remains functional in
  both the api-rs pod and Agent Sandbox pods. It provides an init-copy,
  read-only mount, tool/workflow wiring, and sandbox prompt path so staged
  rollout and rollback do not require an atomic repo-cache cutover.
- When `CENTAUR_OVERLAY_DIR` identifies an available repository root, that root
  is authoritative for both prompts and skills. Its
  `services/sandbox/SYSTEM_PROMPT.md` wins when present; intentionally omitting
  the file disables the overlay prompt rather than resurrecting stale image
  instructions. Image-baked prompt/skill fallbacks apply only when the repo
  root itself is unavailable.
- `networkPolicy.legacyManagedByApiServerAccess` defaults to `true`. It keeps
  API ingress and egress available to pre-capability-label pods carrying only
  `centaur.ai/managed-by=api-rs`. New pods always project
  `centaur.ai/api-server-enabled` as `true` or `false`, and the legacy selector
  requires that label to be absent, so it cannot grant access to a new
  capability-disabled pod. Disable it only after legacy ready sandboxes and
  assigned sessions have drained; the schema-forward rollback stage restores
  it for bridge-created unlabeled pods.
- Warm-pool reconciliation atomically reserves only `status='ready'` rows with
  a workload key different from the current spec before stopping their backend
  sandboxes. Claimed or otherwise bound work cannot enter that eviction set.
- Every sandbox assignment is stamped with a digest binding the deployment's
  full default-spec generation to that sandbox ID. The first owned turn on an
  older assigned thread replaces its stale sandbox; the ID binding prevents a
  rollback-era reassignment from inheriting a trusted forward stamp.

## Upstream replacements and dropped patches

- Upstream's current Slack render/activity pipeline replaces the old
  renderer-specific Thinking patches. The retained terminal mismatch check is
  layered onto the durable upstream pipeline.
- Upstream's in-process Slack handoff retry replaces TipLink's older dedupe-key
  deletion/retry mechanics.
- Upstream stdout-owner leases, adoption, shutdown handoff, sandbox capacity,
  capability labels, and API routing are authoritative. Session release was
  redesigned around those ownership fences rather than replaying the old
  sequential release patch.
- Upstream repo-cache overlay sources are the target architecture. Image
  overlays are explicitly transitional, not a competing long-term source.
- TipLink's managed proxy timeout patch, old Codex config bootstrap helper,
  legacy GitHub identity enrichment, tool-specific MPP/Preqin/Drive changes,
  and stale generated docs/workflows were not carried into core. Those are
  either fixed upstream, obsolete under the new architecture, or belong in the
  overlay/tool repositories.
- Claude-as-default changes were not retained in base Centaur. Harness defaults
  remain upstream-owned; Fineas-specific defaults belong in the deployment
  overlay/configuration.

## Migration boundary

TipLink SQLx migrations `0001` through `0032` retain their deployed identities.
Upstream migrations that formerly occupied `0032` through `0038` are shifted to
`0033` through `0039`; `0033`–`0038` keep their upstream bodies, while `0039`
adds the independently reviewed fail-closed Fineas privacy/RLS reconciliation.
Upstream `0040`–`0042` retain both numbering and bodies. Rails migration
`20260624000100_add_password_grant_to_broker_credentials.rb` remains compatible
with TipLink's deployed migration record. New fork migration `0043` adds the
nullable, rollback-compatible sandbox content-revision stamp. Immutable
SQLx/Rails checksum manifests and CI guards are included in this branch.

## Rollout order

1. Publish the merged fork images and chart.
2. Establish a zero-overlap delivery-writer boundary before the full runtime
   sync: disable cloudflared ingress, drain active work, and scale every old
   api-rs and Slackbot replica to zero. Do not allow old and receipt-writing
   Slackbot replicas to serve concurrently; the silent-trace scanner uses the
   earliest durable receipt as its rollout cutoff.
3. Deploy the complete api-rs and Slackbot revision with legacy network-policy
   access and `overlay.image` compatibility enabled, then restore ingress.
4. Run one controlled Slack turn. Confirm its primary or fallback message is
   visible and verify exactly one `session.delivery_completed` event exists for
   its `(thread_key, execution_id)` before enabling or accepting silent-trace
   scanning. With no receipt, the scanner must report
   `delivery_receipt_writer_not_activated` and start no captures.
5. Let workload-key reconciliation retire stale unclaimed warm sandboxes;
   existing assigned sessions replace their sandbox on their next owned turn,
   while explicit cancellation still uses canonical release.
6. Verify new ready pods carry capability labels, repo-backed prompts, and the
   expected workload key.
7. Drain all legacy sessions, then disable
   `networkPolicy.legacyManagedByApiServerAccess` and eventually remove the
   image-overlay values from the Fineas deployment.
