# Add indexes to the partitioned transaction-event schema

Use this guide when adding an index or deciding whether to backfill one on
`public.transaction_events`. Schema migration success, index metadata, and
complete index coverage are separate outcomes. Choose the coverage you need
before scheduling a build.

## Understand the partition tree

The table has two partition levels:

- `transaction_events` partitions by `retention_class` (`hot`, `warm`, `cold`).
- `transaction_events_hot`, `transaction_events_warm`, and
  `transaction_events_cold` partition by UTC `event_date`.
- Physical tables named `transaction_events_<class>_YYYYMMDD` contain rows for
  one UTC day, with an inclusive lower bound and exclusive next-day upper bound.

The class tables and their day tables are partitions of the main table, not
unrelated event tables. Queries through the root still read physical day tables.
An index on a different table needs its own demonstrated query requirement.

A partitioned index is metadata, not a physical index containing all rows.
`CREATE INDEX ... ON ONLY` on the root and class tables avoids recursively
building indexes on existing days. Attach the class indexes to the root index;
build and attach physical indexes separately when historical coverage is needed.
Postgres marks a partitioned index valid when its required child indexes are
attached and valid. A root definition alone does not establish usable coverage.

For the implemented BRIN example, see
[`002_transaction_events_ingested_at_index.sql`](../../crates/infra/audit/migrations/002_transaction_events_ingested_at_index.sql).
It registers `ingested_at` index metadata on the root and all three class tables.
New day tables acquire matching indexes when partition maintenance attaches them,
even while older missing indexes leave the parent invalid. This does not
retroactively index days that were already attached when migration 002 ran.

## Choose forward-only coverage or a backfill

**Forward-only:** apply the parent definitions and leave existing days without the
new index. Let normal partition creation index new days and retention remove old
days. This avoids scanning populated historical partitions, but accepts partial
coverage and potentially slower extraction until the old days disappear.
Do not run `audit-archiver index` for this choice: it backfills existing days.

Before choosing forward-only coverage:

- Check actual retention settings for every class the query reads. Defaults are
  hot **3 days**, warm **7 days**, and cold **30 days**; one three-day assumption
  does not cover the whole table. Retention is based on `event_time`/`event_date`,
  not the indexed `ingested_at` timestamp.
- Inspect existing partition bounds and index attachments. Maintenance pre-creates
  days through three days ahead, so even a future day may already exist without
  the new index. Those tables are not recreated merely because metadata changed.
- Confirm partition creation and expiry are running. Expiry waits until a whole
  day is older than its retention window plus a one-hour grace period, and a
  failed or blocked maintenance pass can delay removal. Do not promise completion
  after exactly the configured number of days.
- Agree on the temporary query cost and verify query plans on indexed leaves.
  A partially covered, invalid root is not proof that a root-table warehouse
  query gets the intended plan. Re-check full coverage after the old days expire;
  do not manually set catalog validity flags or shorten retention to force it.

**Historical backfill:** apply the parent definitions, then explicitly run the
supported native reconciler in a separate monitored operation. It scans populated
days, one index at a time; on a large database this can take hours and consume
read I/O and storage. Use this when the required query latency or coverage cannot
wait for retention, with an agreed resource budget and stop procedure.

These choices retain the index definition. Removing an already installed index
is a different schema change requiring review and a new migration, not an edit
to migration 002 or its recorded checksum.

## Run the existing `ingested_at` workflow

Use the reviewed binary for the target schema and a session-affine connection to
the intended network database. Provision `TIPS_AUDIT_POSTGRES_URL` through the
approved secret mechanism using the `audit_archiver_migration` credential; never
paste a credential URL into a command, log, or shell history. Verify the database
and role through the approved database interface before making changes.

```bash
# TIPS_AUDIT_POSTGRES_URL is supplied securely for the target database.
audit-archiver migrate up

# Only after an explicit decision to backfill existing ingested_at indexes:
audit-archiver index
```

Current `migrate up` applies schema migrations and exits. It does not run the
online backfill. The ingestion service's `/readyz` checks storage schema
readiness, not completion of the `ingested_at` index. A successful deployment or
migration log is not an indexing-complete signal.

`index` is a one-off command for this BRIN index, not a selector for arbitrary
indexes or a generic required-work registry. It does the following on one
connection while holding the SQLx migration advisory lock:

1. Checks for successful migration version 2 and the root index definition.
2. Enumerates physical public day tables under the three class parents. The
   supported leaf suffix is exactly eight digits (`YYYYMMDD`).
3. Skips valid indexes attached to the expected class index. Attaches a valid
   unattached index without rebuilding it. Drops an invalid unattached index
   concurrently, then rebuilds it with `CREATE INDEX CONCURRENTLY`.
4. Attaches each leaf index to its class index and checks root validity. It can
   re-enumerate up to three passes to account for partition maintenance; if the
   root is still invalid, it fails and asks for a later explicit retry.

An invalid **attached** index or an index attached to an unexpected parent causes
an error requiring investigation; the command does not automatically repair that
case. Do not assume `IF NOT EXISTS` makes an interrupted index valid. A retry
must inspect and repair catalog state, as this reconciler does.

### Locks, timeouts, and stopping

`CREATE INDEX CONCURRENTLY` and `DROP INDEX CONCURRENTLY` run outside a schema
transaction. Concurrent creation reduces write blocking but can still wait for
other transactions and consume resources. The command sets `statement_timeout`
and build-phase `lock_timeout` to zero, so a build has no statement deadline.
Index attachment uses a **5-second** lock timeout, then resets it to zero.
`TIPS_AUDIT_TRANSACTION_EVENT_PARTITION_LOCK_TIMEOUT_MS` controls partition
maintenance, not this index command's build timeout.

Do not start competing index runs or a migration while a backfill holds the
migration lock: another invocation can wait behind it. Partition maintenance has
its own coordination and can change the tree during a build. Use a direct or
session-affine database path; a transaction-pooling proxy is unsuitable for this
session advisory lock and session settings.

Monitor through the approved database interface, including DataHub's standard
**Index Creation Progress** view where available, rather than pod exec. Watch
`pg_stat_progress_create_index`, transaction/lock waits, read I/O, storage, ingest
latency, and partition-maintenance health. A progress view with no row alone is
not evidence of complete coverage or absence of other index statements.

The current command has no managed stop/resume status contract. Stopping its
client process is not proof that the server stopped working. Before restarting
or declaring a pause complete, have an authorized operator identify the exact
operation on the correct server using its PID, backend start time, database,
login role, and query. Cancel it through approved database tooling and verify
that the operation/progress and its locks are gone; a successful cancel request
alone is not proof. Re-check backend identity before escalation to termination;
never cancel all connections for a role or database. Preserve interrupted index
state for the reconciler's explicit retry after confirmed cleanup.

Keep the hours-long backfill out of the schema migrator init container and normal
deployment-completion gate. Arrange a separately approved operator workflow with
sufficient runtime, monitoring, and a cancellation owner. This separation avoids
making rollout wait for the backfill; it does not extend a deployment platform's
wait limit or add automatic retry, health hosting, or durable lifecycle control.

## Verify catalog state

Run these read-only queries through the approved database interface. Four expected
parent entries must exist and be valid before declaring the current BRIN tree
complete; a missing entry appears as `NULL` instead of silently disappearing:

```sql
WITH expected(name) AS (
    VALUES ('transaction_events_ingested_at_idx'),
           ('transaction_events_hot_ingested_at_idx'),
           ('transaction_events_warm_ingested_at_idx'),
           ('transaction_events_cold_ingested_at_idx')
)
SELECT expected.name, i.indisvalid
FROM expected
LEFT JOIN pg_index i
  ON i.indexrelid = to_regclass('public.' || expected.name)
ORDER BY expected.name;
```

Inspect every physical table's bounds and its index attachment to this root.
A missing attached index appears as `NULL`; an unattached index is not coverage
of the partitioned index tree:

```sql
WITH RECURSIVE index_tree(oid) AS (
    SELECT to_regclass('public.transaction_events_ingested_at_idx')::oid
    UNION ALL
    SELECT p.inhrelid
    FROM pg_inherits p
    JOIN index_tree parent ON parent.oid = p.inhparent
), attached AS (
    SELECT i.indrelid, i.indexrelid, i.indisvalid, i.indisready
    FROM pg_index i
    JOIN index_tree tree ON tree.oid = i.indexrelid
)
SELECT t.relid::regclass AS day_table,
       pg_get_expr(c.relpartbound, c.oid) AS partition_bounds,
       attached.indexrelid::regclass AS attached_index,
       attached.indisvalid, attached.indisready
FROM pg_partition_tree('public.transaction_events'::regclass) t
JOIN pg_class c ON c.oid = t.relid
LEFT JOIN attached ON attached.indrelid = t.relid
WHERE t.isleaf
ORDER BY day_table;
```

If using forward-only coverage, record which old days are deliberately missing
indexes and revisit until they expire. For either strategy, compare
`pg_get_indexdef` with the reviewed migration, check query plans and extraction
latency, and investigate missing or invalid entries. Root validity establishes
attachment completeness, not that the index serves every warehouse predicate.
The BRIN index targets `ingested_at` cutoffs; an epoch expression used for
parallel slicing needs separate plan validation before enabling `NUM_SLICES`.

## Add a future index

1. Establish the exact table, predicate, expression, method, and query-plan
   benefit. Choose forward-only coverage or historical backfill explicitly;
   include temporary latency and write-amplification costs in the review.
2. Add a **new**, unused migration version in
   [`crates/infra/audit/migrations/`](../../crates/infra/audit/migrations/).
   Never rewrite applied or legacy SQL, change a recorded checksum, or reset
   migration history to install an index. Follow migration 002's parent-only
   pattern for this two-level tree: root and each class definition, with class
   indexes attached to the root. Keep populated-leaf concurrent builds outside
   the schema transaction. Changing a unique index also requires checking
   Postgres's partition-key restrictions.
3. For forward-only coverage, verify partition creation inherits the new index
   while the root remains incomplete. Existing/pre-created days must remain
   distinguishable from newly attached days; retain an explicit coverage check.
4. For a backfill, implement and expose an index-specific native reconciliation
   path, following
   [`ingested_at_index.rs`](../../crates/infra/audit/src/ingested_at_index.rs).
   There is no generic registration API: `lib.rs` exports the current helper,
   and the binary's `Command::Index` dispatch calls only that helper. A new
   migration does not register another backfill, and the existing `index`
   command will not build an unrelated index. Review any new dispatch or
   extension explicitly; do not duplicate the algorithm in Python or a chart.
5. Test upgrades with already populated days, fresh schemas, valid and invalid
   unattached indexes, interrupted builds, repeated runs, attachment topology,
   and new days created while parents are incomplete. If names or bounds change,
   update the native reconciler's assumptions and tests together. The current
   [`Postgres tests`](../../crates/infra/audit/tests/postgres_transaction_events.rs)
   cover the existing BRIN recovery, idempotence, and future-day inheritance.
6. Regenerate and review the committed schema snapshot using the instructions
   in the [audit library README](../../crates/infra/audit/README.md#postgres-schema).
   Document the new explicit command, coverage checks, operating budget, and
   rollout decision. Do not equate successful schema application with completion
   of a separately scheduled backfill.
