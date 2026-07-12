# TipLink Centaur upstream alignment (2026-07-11)

This branch integrates the exact Paradigm tree at `3c6e84d9` with TipLink
`ba2c01f5`. Conflict resolution used the upstream implementation as the
baseline, then reapplied only TipLink behavior that remains deployment- or
runtime-relevant. The reviewed aggregate tree is transported as GitHub-authored
verified commits, then joined to a second GitHub-verified branch rooted at the
upstream SHA. The final head must therefore contain both histories, reproduce
the tested tree exactly, and satisfy the fork's signature policy.

## Retained on the current upstream architecture

| TipLink behavior | Current implementation |
| --- | --- |
| Ordered tool overlays | `install_tool_shims.py` uses later-source replacement and matches package, project, and script identifiers for allow/block lists. |
| Warm first-turn context | `harness-server` derives `CENTAUR_THREAD_KEY` from the first blocks-mode user line before spawning Codex app-server. |
| Active Codex steering | A second user line during an active turn becomes `turn/steer`; its response is consumed rather than leaked into blocks output. |
| Reviewed runtime pins | The sandbox retains TipLink's tested Codex `0.144.1` and Claude Code `2.1.198` pins. Paradigm's Codex `0.144.0` already supports GPT-5.6 Sol, while Claude Code `2.1.197` is the documented Sonnet 5 floor; the retained next-patch pins include later fixes and are verified in the image build. |
| Sandbox development capabilities | Terraform and Playwright `1.58.0` with its native headless shell remain available on amd64 and arm64. `agent-browser` uses its installed Chrome on amd64 and an explicit Playwright-shell path on arm64, where its own installer and discovery do not work. Node tools opt into the sandbox's injected proxy without discarding existing `NODE_OPTIONS`. |
| Table-aware Codex configuration | The entrypoint transforms the copied operator config as TOML, disables both multi-agent feature forms without colliding with a `[features.multi_agent_v2]` table, applies deployment reasoning and Bedrock settings, then lets a valid operator overlay win. It validates before atomically replacing the config. |
| Claude model aliases on a Codex default | Slack, Linear, and GitHub aliases map Sonnet to `claude-sonnet-5`. A known Claude alias in `--model` selects Claude only when an explicit harness/provider flag has not already won; full model IDs remain pass-through escape hatches. |
| Canonical session release | `POST /api/session/{thread_key}/release` locks the session row, fences the expected sandbox, rejects active work unless cancellation is explicit, clears stdout ownership on cancellation, and stops only the snapshotted sandbox. |
| Ambient Slack channels | Configured root messages and replies execute without an explicit mention; messages outside the allowlist remain inert. |
| Slack event dedupe | The patched Chat dependency dedupes by actionable bucket so a non-actionable `message` event cannot suppress a later `app_mention`. |
| Durable terminal reconciliation | Slack compares streamed markdown with the durable terminal result and replaces divergent output. |
| Durable Slack delivery proof | After Slack confirms a primary, reconciled, fallback, or visible-error message, Slackbot records one idempotent `session.delivery_completed` event through a Slackbot-key-only route bound to the exact thread and execution. |
| Generic HTTP secret scopes | Method/path scopes are retained through discovery, permission translation, and iron-control registration. |
| GitHub App installation tokens | The grant is registered in upstream `Broker::CredentialGrants`; the model delegates validation and refresh to that registry. Helm bootstraps the canonical credential before api-rs starts, and the built-in infra role grants a scheme-preserving `GITHUB_TOKEN` replacement for `github.com` and `api.github.com` to sandbox principals. |
| Fork image publication | TipLink GHCR namespace, GitHub-hosted native builders, safe multi-arch assembly, and upstream `githubbot`/harness image inputs are combined. Pull requests build without registry credentials; only a reviewed `v*` tag or explicit dispatch can write packages and emit a deployable descriptor. Fineas promotion is owned by the separately reviewed infra PR DAG. |
| GitHub-hosted CI | TipLink's removal of Depot runners remains authoritative across core, Console, docs, audit, and chart workflows. Native arm64 publication continues on GitHub's arm runner. |
| Human-reviewed upstream tracking | The weekly `upstream-sync.yml` workflow opens a draft cross-repository PR directly from `paradigmxyz:main`, after verifying every upstream-only commit. It never copies unreviewed code into a trusted TipLink branch, so PR workflows retain the external-head token/secret boundary. A separate synchronize-time check fails if the moving upstream/base refs no longer match the SHAs and verified count recorded in the PR body. The PR is only an audit signal; an integration branch must pin that recorded upstream SHA. |
| Trusted publication split | PR docs, chart, and image validation have read-only/no-secret jobs. Cloudflare docs, chart, and runtime image publication live in non-PR workflows and require an explicit reviewed-main confirmation or release tag. |
| Trust-lane key separation | API and Console startup reject configured control, bot, workflow, feedback, or JWT signing credentials shorter than 32 bytes or reused across trust lanes without printing secret values. |

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
- TipLink's old managed-proxy patch is replaced by the current agent-k8s
  implementation, which injects
  `IRON_PROXY_UPSTREAM_RESPONSE_HEADER_TIMEOUT=120s` into every managed proxy.
  Obsolete line-oriented portions of the old Codex bootstrap were replaced by
  the retained table-aware transformer. Legacy GitHub identity enrichment,
  tool-specific MPP/Preqin/Drive changes, and stale generated docs/workflows
  were not carried into core; they are obsolete under the new architecture or
  belong in overlay/tool repositories.
- CodeQL findings in unchanged Paradigm code and inherited test fixtures are
  treated as upstream baseline, not fork patches. This sync carries neither
  analyzer-only rewrites nor `codeql[...]` suppression comments; review and
  rollout gates cover integration-owned behavior instead.
- Claude-as-default changes were not retained in base Centaur. Harness defaults
  remain upstream-owned; Fineas-specific defaults belong in the deployment
  overlay/configuration. This does not remove the newer, reviewed Claude Code
  pin used when a deployment explicitly selects Sonnet 5.

## TipLink-only commit inventory

`git cherry` identified 101 TipLink-only commits relative to the reviewed
Paradigm baseline. Every commit is classified below; no commit is implicitly
dropped.

- Retained directly or satisfied by an identified upstream equivalent (36):
  `91234d85 2a03d179 755b5f61 5ec8beb9 429e2be9 1d712a5e 6369763a
  26f9db98 4978561a 7aa8f772 d6dcdb4d 567d1abc 6c522c51 46788900
  65ce9902 9b5a4bbb f5636a0f 1882c8eb 7617924f 1e902a24 60f3272c
  c59a82ae ea51b3ee 26527258 226b9dcd 5aa259cb f65fdc0b 3617f569
  9d00faca 3d34dc7f 410d3769 abc0f356 b93bd640 b1a4569f e00ccb21
  28bb47ee`.
- Publication and upstream tracking retained through the redesigned trust
  lanes (11): `b52e85ed 6de8d862 d4c87aff 5599cc51 093717ef a6a2fb33
  1105c098 f30034f8 c3bf8c16 bbb543d2 cb100e05`.
- Mixed commits split by behavior (4): `7dcdf72e` retains overlay-image
  compatibility while dropping the old Python API; `163a7a88` retains GitHub
  App intent through `CredentialGrants`; `79b5926a` retains workflow behavior
  on the current host while dropping obsolete API shapes; `1f21791f` retains
  Sonnet 5 aliases while dropping Claude-as-default.
- Superseded, obsolete, or migrated out of core (39): `f54bace2 720fa29c
  63576812 c823cd59 fa7940f6 508861b2 8f458c87 95f20ab3 2f0fec99
  c154bde6 2d5fb4ed b8bbe0c1 c452b02b 2d999e0a 86e6ca2d 5da54a5e
  ad5f8d34 ed4f52d2 d844ebab 0d3e0c43 f2bb2e46 40d2837c 87f26b5e
  d058b5d3 4f007df0 e413aebc 86068044 1dc5130a 4215ada6 930b1a82
  9895becb f247bdfb ce677374 3254b0a2 cf5b6749 45bb36e9 036e35ed
  db630210 13ec857e`.
- Net-reverted add/revert pairs with no baseline tree effect (4): `2595dcf1
  f9d0b765 dda19688 7ac28097`.
- CodeQL-only history intentionally ignored as inherited upstream baseline
  (6): `a8adb1f8 9baa30cf 3a886f7f 2dfabef2 f58f1154 6b0c3b09`.
- Historical follow-up rather than active-baseline carry (1): `07bd5f08`.
  Its fallback-post retry was already absent from `ba2c01f5`; restoring it
  requires a separate exactly-once delivery/receipt decision and test.

Known one-for-one upstream equivalents include `d6dcdb4d` / `0691b1aa`,
`c59a82ae` / `cc0c4c0c`, `f5636a0f` / `f6664689`, `1882c8eb` /
`1c9a5d62`, `7617924f` / `e12b9d93`, `1e902a24` / `1f944521`,
`60f3272c` / `f51239ee`, `26527258` / `2a6b838d`, and `226b9dcd` /
`a90453b8`. The exact managed-proxy replacement for `9895becb` is the
agent-k8s injection of `IRON_PROXY_UPSTREAM_RESPONSE_HEADER_TIMEOUT=120s`.

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

1. After review, explicitly publish the fork image descriptor and chart; a PR
   run or merge alone does not enter the package/deployment lanes.
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
