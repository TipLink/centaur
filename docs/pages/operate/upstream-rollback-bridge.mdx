# Upstream-sync rollback bridge

This branch is an emergency, schema-forward bridge based on TipLink Centaur
`ba2c01f5`. It is not a general downgrade. Use it only through the audited
Fineas upstream-sync runbook in
`fineas-centaur-infra/docs/runbooks/centaur-upstream-sync-20260711.md`.

## Hard safety contract

- Keep `RUN_MIGRATIONS=false`. SQLx has no down migrations. This binary embeds
  byte-identical migrations 1–43 so its ledger matches the forward database,
  but the emergency rollback must not take ownership of schema migration. The
  bridge rejects `RUN_MIGRATIONS=true` before binding or database access; only
  the exact reviewed forward binary may migrate a database.
- Use the forward chart and provision a distinct `CENTAUR_CONTROL_API_KEY`
  before the bridge starts. Control, Slack, GitHub, Linear, Discord, Teams,
  workflow, and Slack-feedback service credentials must all be pairwise
  distinct. The bridge refuses startup on a missing control key or any reused
  service key; the old ba2c chart does not inject the control key.
- Make the API cutover with zero replica overlap. This bridge has release and
  assignment compare-and-swap fences, but it does not implement the forward
  runtime's cross-replica stdout-owner lease protocol.
- Do not run the branch image publisher until an operator has read the live
  child annotations and proved that Argo Image Updater cannot manage any of
  the four exact bridge repositories or the `reviewed-<full-bridge-sha>` tag
  namespace, and core review has frozen the exact forward commit. The current
  legacy updater manages only the separate Fineas overlay repository and its
  allow-list accepts only deploy-shaped `sha-<7>` tags. A human with Actions
  permission can use the manual path, which requires an explicit live-scope
  acknowledgement and the exact frozen forward commit. A restricted
  contents-write principal can instead create one new lightweight tag named
  `rollback-bridge-publish-live-scope-verified-<bridge40>-forward-<forward40>-at-<unix-seconds>`
  immediately after the same read-only live check. The release gate requires
  the tag object, checkout, event SHA, embedded bridge SHA, and frozen forward
  SHA to agree. It allows 120 seconds of future clock skew and fails closed
  once the attestation is 900 seconds old. If Actions queueing exceeds that
  window, rerun the helper and create a new timestamped tag; never delete or
  recreate an attestation ref. The 900-second window bounds admission to the
  package-writing workflow; it does not claim that a native multi-arch build
  finishes inside that window. Hold the verified updater scope unchanged until
  the descriptor completes. If it changes, supersede the run and repeat the
  live check with a new immutable attestation ref. Both paths emit only
  `reviewed-<full-bridge-sha>` tags for exactly four linux/arm64 bridge runtime
  rows: API, Slackbot v2, agent, and IronProxy. Other branch/tag events cannot
  publish. This namespace cannot match the legacy updater's deploy-shaped
  `sha-<7>` tags; Console web/worker remains on the forward image. The publisher
  has no Kubernetes credentials. Publication creates inert artifacts and a
  descriptor; it is not rollout authorization. The infra runbook still
  requires global Image Updater removal and the Argo automation freeze before
  consuming the descriptor in Git or changing any Argo pin. The separate
  pull-request workflow preserves the historical `Publish Images` check
  context but has read-only repository permissions, receives no registry
  credentials, and builds with `push: false`.
- Publication run `29175125487` at bridge head `a08bbe41` is superseded audit
  evidence. Its BuildKit outputs were attested OCI indexes, while the final
  tags resolved to runnable platform children; the old descriptor comparison
  used the wrong identity and failed. The tags remain immutable, but that run
  produced no approved descriptor and must never populate an infra lock. The
  corrected publisher records merge-index and runnable-child digests
  separately and requires the final arm64 child to equal this run's child.
- Set `CENTAUR_ROLLBACK_BRIDGE_PAUSE_WORKFLOWS=true` explicitly. The bridge
  refuses to start when the value is absent, false, or malformed. With the
  fence acknowledged, it starts no absurd workers, schedule ticks, metadata
  reconciler, or removed-workflow reaper. Workflow create/cancel/event
  mutations return 403. This preserves pending, running, and sleeping rows for
  re-forwarding. Startup is schema-read-only: all five forward absurd queues,
  including `centaur_workflow_schedules`, must already exist. A missing queue
  fails startup without creating it or changing the migration ledger.
- Do not treat cancellation as a workflow-drain strategy. Forward-only task
  params, retry policy, cancellation policy, checkpoints, and wait rows must
  survive byte-for-byte.

Migrations 33–43 do not change absurd's task, run, checkpoint, event, or wait
schema. Migrations 7–9, which define that contract, are byte-identical between
the ba2c baseline and the forward integration. Handler source is not
backward-compatible, however: a task created for a workflow absent from the
rollback overlay would be failed or reaped by active ba2c workers. The
workflow pause is therefore mandatory.

The forward chart value for the rollback infra state is exactly:

```yaml
apiRs:
  extraEnv:
    CENTAUR_ROLLBACK_BRIDGE_PAUSE_WORKFLOWS: "true"
```

The server validates pause and migration ownership before binding its listener
or touching the database. The workflow runtime then reads the existing absurd
queue registry and refuses incomplete forward state without running queue DDL.

## Protected routes

The bridge accepts `Authorization: Bearer $CENTAUR_CONTROL_API_KEY` for session
release, sandbox drain, global workflow routes, and admin routes. Anonymous and
Slackbot-key requests are rejected. The Slack archive workflow has one narrow
exception: its download-url request carries the exact workflow run and task
IDs, which must match the import row.

Call release with the caller's observed sandbox ID:

```bash
curl --fail-with-body \
  -H "Authorization: Bearer $CENTAUR_CONTROL_API_KEY" \
  -H 'Content-Type: application/json' \
  --data "{\"release_id\":\"rollback-window\",\"expected_sandbox_id\":\"$SANDBOX_ID\",\"cancel_inflight\":true}" \
  "$CENTAUR_API_URL/api/session/$THREAD_KEY/release"
```

A mismatch is a hard retry signal. The bridge must never stop the newly bound
sandbox from a stale observation. A backend stop failure returns HTTP 503 with
`ok: false`, `sandbox_released: false`, and `sandbox_release_error`; operators
must validate those fields and the returned null sandbox assignment rather than
trusting transport success alone.

Before replacing the forward API, quiesce ingress and call drain once. Drain
permanently closes this runtime's allocation gate, pauses the warm replenisher,
waits for in-flight replenish and session allocation work, and only then takes
the backend inventory and stops it. New warm claims, resumes, and cold creates
return 503 after the gate closes. Any stop or warm-row failure makes the drain
request itself return 503 with the partial report, so `curl --fail-with-body`
is a real hard gate.

```bash
curl --fail-with-body \
  -H "Authorization: Bearer $CENTAUR_CONTROL_API_KEY" \
  -X POST "$CENTAUR_API_URL/api/sandboxes/drain"
```

## Acceptance and re-forward

After the bridge is ready, verify all of the following before reopening Slack
ingress:

1. Exactly one bridge API replica exists and no forward API replica remains.
2. `RUN_MIGRATIONS=false` and workflow pause are effective.
3. Anonymous and Slackbot-key drain/release requests return 401; the distinct
   control key reaches the handler.
4. Workflow mutation requests return 403 and the counts plus JSON contents of
   all non-terminal absurd rows remain unchanged.
5. Session create, execute, expected-sandbox release, and one Slack turn pass.

Workflow processing remains unavailable throughout the rollback. Re-forward
to the reviewed integration image before resuming those workers. On the
forward image, verify that pending rows are claimed, expired running claims are
adopted after their lease, and sleeping rows retain their checkpoint/wait
state. Never set `CENTAUR_ROLLBACK_BRIDGE_PAUSE_WORKFLOWS=false` in the rollback
deployment.

The focused source checks are:

```bash
cd services/api-rs
cargo test -p centaur-api-server --lib
cargo test -p centaur-api-server --test rollback_bridge_startup
cargo test -p centaur-session-runtime adoption_tests::release_and_sandbox_assignment_race_has_exactly_one_winner
cargo test -p centaur-session-runtime adoption_tests::release_winning_allocation_race_stays_cancelled_not_failed
cargo test -p centaur-session-runtime adoption_tests::drain_waits_for_inflight_allocation_and_rejects_new_allocations
cargo test -p centaur-workflows --lib rollback_bridge_requires_workflow_pause_to_be_explicitly_enabled
```

CI fetches the pinned forward source commit, proves the test fixtures and
embedded migrations 33–43 are byte-identical to it, verifies both SHA-256 and
SQLx SHA-384 manifests, then applies the full embedded ledger 1–43 to a
disposable ParadeDB and seeds pending/running/sleeping tasks plus their runs,
retry/cancellation policy, checkpoints, events, waits, and migration ledger,
then runs the bridge with `RUN_MIGRATIONS=false` past worker/reaper intervals.
Another negative case omits the forward schedule queue and requires startup to
fail without changing the absurd schema or SQLx ledger.
The ordinary API integration database is likewise migrated by the exact
reviewed forward binary before the bridge integration server starts with
`RUN_MIGRATIONS=false`; the deployed-shape bridge integration server never
takes schema ownership.
It exercises read, create, cancel, event, webhook, and admin lanes and requires
an exact before/after JSON snapshot:

```bash
ROLLBACK_BRIDGE_FORWARD_TEST_DATABASE_URL="$PARADEDB_URL" \
  cargo test -p centaur-api-server --test rollback_bridge_forward_schema
```

The CI-only cross-version case also builds the exact pinned forward commit. It
uses that binary to create real pending, running, and sleeping tasks, stops it,
starts the bridge in pause mode, checks an exact durable snapshot, then starts
the same forward binary again. Acceptance requires expired-claim adoption,
event and timed-wait resumption, preserved checkpoints and task contracts, and
actual replacement/restamping of a rollback-era sandbox assignment.
