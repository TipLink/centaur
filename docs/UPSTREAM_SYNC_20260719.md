# TipLink Centaur upstream alignment (2026-07-19)

This branch merges Paradigm `22a036b8` into TipLink `ad668981`. The common
ancestor is Paradigm PR #1094 at `6528295f`; the reviewed delta contains 24
upstream changes and no database migration. Conflict resolution starts from
the current upstream implementation and reapplies only active Fineas
deployment and security invariants.

## Upstream patterns adopted

- Workflow-scoped principals replace the shared workflow-host identity for
  workflows that call tools directly. The runtime derives the principal id
  from `WORKFLOW_NAME`, registers it through Iron Control, and fails closed
  when scoped execution is requested without workflow-host sandboxing.
- Slack message override strategies replace direct flag parsing in the
  execution path. Fineas channel defaults, sticky override behavior, ambient
  channels, active-execution steering, and terminal reconciliation remain on
  the strategy-based implementation.
- Slack Block Kit actions become authenticated, deduplicated durable workflow
  events. Global event emission remains service-only.
- Session context becomes platform-aware for Slack, Discord, Linear, and
  GitHub. Fineas authorization runs before any context is returned.
- Console login gains the upstream Slack OIDC flow and GitHub requester
  attribution. Personal chat discovery stays separate from direct-link read
  access.
- The sandbox gains the Google Cloud CLI and BigQuery CLI alongside the
  retained Terraform installation. Installing a CLI does not mount host ADC
  or grant a Google credential. The existing entrypoint still creates only its
  nonfunctional mock ADC file when no credential path is configured.
- Upstream image downscaling, Console transcript images, Linear fixes,
  telemetry cleanup, sanitizer coverage, and dependency bumps are adopted
  without fork-specific alternatives.

## Fineas boundaries retained

- Public Slack threads and explicitly shared Console chats are readable by
  direct link but remain read-only for non-owners. Upstream's broader writable
  behavior is intentionally not adopted. Both composer rendering and the POST
  endpoint enforce the owner scope.
- Session APIs remain authenticated for every platform. Tests cover anonymous
  Slack, Discord, Linear, GitHub, and CLI keys plus an authorized platform
  context response.
- Workflow event emission remains restricted to the trusted workflow service
  credential. Principal JWTs cannot emit global events.
- The workflow-host keeps Fineas task capability tokens, allocation fencing,
  and shared runtime drain inventory while using the upstream scoped-principal
  sandbox specification.
- The chart renders both `WORKFLOW_API_ALLOWED_NAMES` and the upstream
  `WORKFLOW_HOST_SANDBOX` setting.
- Slack ambient-channel execution, bot allowlists, API-owned channel defaults,
  stop handling, durable terminal reconciliation, and exact upload destination
  guidance remain active.
- The standard G Suite credential and the privileged compliance Drive
  credential remain separate tools, secrets, proxy hosts, and role grants.
  No Google credential is added to generic `infra` by this sync.
- Terraform, reviewed runtime pins, overlay workflow composition, and TipLink
  publication/signature gates remain active.

## Fineas legacy removed by the companion rollout

The Fineas compliance workflow declares `WORKFLOW_PRINCIPAL = True`, so the
upstream runtime creates `workflow-compliance-cdd-research`. The companion
infra reconciliation grants the compliance workflow role, isolated Drive
credential, Gemini credential, and upload-only Slack channel permission to
that principal. It refuses the shared `workflow-host`, removes old compliance
role and channel assignments from that identity, and enforces a single-role
model for the dedicated principal.

This replaces the manual shared-principal workaround. It is not maintained as
a fallback implementation.

## Required rollout order

1. Merge and publish this base Centaur sync after aggregate, Console, Rust,
   Slack, chart, and native image checks pass.
2. Merge the Fineas overlay change that opts the CDD workflow into the scoped
   principal. Publish the reviewed overlay image.
3. Deploy the base and overlay together with the existing credential boundary
   unchanged. Confirm api-rs registered `workflow-compliance-cdd-research`.
4. Apply the infra reconciliation from its reviewed exact head. Confirm the
   dedicated principal has exactly the workflow role and exact upload-only
   channel permission, and the shared `workflow-host` has neither compliance
   assignment.
5. Run a controlled CDD workflow and verify Drive publication, Slack postback,
   owner-only Console writes, and audit output before broad enablement.
