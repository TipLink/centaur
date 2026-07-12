# Forward migration fixtures

These provisional test fixtures are migrations 0033–0043 from the reviewed
forward candidate. The final forward commit has not yet been frozen: replace
the sole value in `.github/rollback-bridge-reviewed-forward-commit` only after
re-copying and verifying these bytes against that exact commit. CI, the manual
publisher, and the Rust rehearsal all read that same file. The same files are vendored into
`centaur-session-sqlx/migrations`, so SQLx embeds the forward ledger through
0043. The emergency bridge still runs with
`RUN_MIGRATIONS=false`: schema ownership stays with the already-completed
forward rollout.

`SHA256SUMS` pins the copied source, and CI compares every byte to the frozen
central commit. Update the fixtures, central pin, and checksums together if the
reviewed forward migration set changes. CI fails closed while the pin is not a
lowercase 40-character commit SHA.
