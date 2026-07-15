# Session SQLx migration safety

The API embeds this crate's migrations at build time. SQLx validates every
applied version and SHA-384 checksum before the API starts, so an applied file
must never be edited, renamed, or removed.

## TipLink lineage

TipLink production has versions `0001` through `0032` applied. Those files are
the canonical history for the fork and are intentionally different from the
same version numbers in upstream Centaur. In particular, `0019`, `0023`, and
`0025` contain TipLink-specific behavior and checksums.

For the July 2026 upstream sync, upstream versions `0032` through `0038` are
shifted to TipLink versions `0033` through `0039`. Upstream versions `0040`
through `0042` retain their original numbers. Fork migration `0039` also
contains the forward-only reconciliation for Fineas public Slack company
context. Fork migration `0043` appends assignment-bound sandbox content
revision tracking; it is deliberately backward compatible with older binaries
that update `sandbox_id` without knowing the new nullable column.
Upstream versions `0043` through `0045` are shifted to TipLink versions `0044`
through `0046`: company-context projection checkpoints, Granola context
projection, and Slack private-channel OAuth synchronization respectively.

Migration `0046` is not compatible with an ordinary overlapping rolling
deployment. It renames the live `slack_dm_*` relations and rebuilds both BM25
indexes, so old API pods and already-running sandboxes must not continue using
the pre-migration relation names after it starts. Deploy this migration only
through a rehearsed zero-overlap cutover that accounts for existing sandboxes;
the prior image is not a functional rollback after the rename.

The checksum manifests checked by `.github/scripts/check-migration-order.sh`
lock the release migration tree. Append a manifest entry for a genuinely new
migration; never replace an existing entry.

The Rails migration
`20260624000100_add_password_grant_to_broker_credentials.rb` is also the
TipLink compatibility version. It converts the historical `credential_kind`
column before enforcing the upstream `grant` schema and must not be replaced
with upstream's simpler body. Rails does not validate migration checksums, so
the release manifest is the immutability boundary for that history.

## Rollback requirements

These migrations are forward-only. There are no SQLx down migrations, and a
failed later migration does not undo earlier successful versions.

After a new migration has applied, an older API image whose embedded migration
set does not contain that version will fail startup when `RUN_MIGRATIONS=true`.
Before rollout, prepare and test one of these application rollback paths:

1. A bridge image with the prior application behavior and the exact new
   migration files and checksums embedded.
2. The prior image with `RUN_MIGRATIONS=false`, tested against the forward
   schema and any new enum or state values created by the release.

Restoring a database snapshot is the only full schema rollback. It is a last
resort because it discards writes made after the snapshot. Apply console/Rails
migrations before rolling the API whenever new API behavior depends on the
console schema or routes.
