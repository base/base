# `audit-archiver-lib`

Audit library for Postgres-backed transaction observability events.

## Overview

Accepts Vector's `transaction-event/v1` batches over HTTP, persists them
idempotently in Postgres, and provides JSON-RPC queries by transaction, block,
bundle, and rejection. Postgres schema readiness is checked before the
`audit-archiver` binary starts serving; see the binary README for the
S3 removal and rollout prerequisites.

## Postgres schema

`migrations/` holds the transaction event schema. `schema.sql` is a committed
`pg_dump` of the schema those migrations produce, without dated day partitions.
The Postgres integration tests fail if a fresh or upgraded database differs
from it. After changing a migration, regenerate the snapshot and review the
diff:

```bash
UPDATE_SCHEMA_SNAPSHOT=1 cargo test -p audit-archiver-lib \
  --test postgres_transaction_events postgres_schema_matches_committed_snapshot
```

`legacy_migrations/` holds the pre-partition migrations 001-004. They are never
applied. The migrator recognizes their recorded rows by version and checksum,
then drops the old table and resets that history in the same transaction that
applies the partitioned baseline.

## Opt-in hourly HOT/WARM partitions

Migration `003_transaction_events_hourly_bridge.sql` is **expansion only**.
Ordinary `migrate up` remains schema-only: it neither activates hourly routing
nor runs historical index backfill. There is no managed migration listener, automatic
index lifecycle or dependency on PR5497. Applied migrations 001/002 are unchanged.
Verify deployed applied history before using this branch: a non-main migration
003 (for example, an experimental native ledger) is not this migration. Stop on
a checksum/history mismatch for owner review; do not erase history or reset data.
The unchanged baseline migrator's destructive legacy-upgrade path still requires
its existing export/retention/data-loss approvals and is not an hourly transition.

After a separately approved transition, HOT/WARM use UTC **event-hour** leaves
inside their event-date branches; COLD and retained pre-cutover leaves remain
daily. The root stays the same partitioned TABLE/OID. Arbitrary nonempty TEXT
event IDs retain the first row per class/hour even if a retry changes the
timestamp within that hour. Cross-hour re-emission may add a row. COLD/pre-cutover
deduplication remains per day. Original request-level ID filtering, admission/TTL,
`event_time` and `ingested_at` meanings are unchanged.

Before deploying the targetless bridge, inventory all additional unique constraints:
`ON CONFLICT DO NOTHING` considers every unique arbiter, not just event identity.
All writers must use that bridge before activation. Inventory PK/FK consumers,
CDC and catalog/OID introspection too; old targeted writers cannot be rolled
back in after activation. Full RPC/UI/ETL reads keep all valid history; no guessed
event-time cutoff is added to an `ingested_at` extraction.

Identity PKs must be non-deferrable (`pg_index.indimmediate`): even a deferrable
constraint that is initially immediate cannot arbitrate `ON CONFLICT`. Catalog
guards refuse this drift without automatic repair.

### Forward-only indexing and the atomic transition

Historical leaves may remain unindexed and age out according to their actual
class retention windows (defaults HOT 3 days, WARM 7, COLD 30—not all three days).
New leaves inherit BRIN indexes. An invalid partitioned parent index can mean
incomplete historical coverage, not failed forward routing or failed schema
migration. `HourlyTransactionEventPartitions::validate_indexes(..., false)`
checks definitions/ownership/edges without claiming full historical coverage.
The `true` mode requires complete valid coverage and belongs to the explicit
manual `audit-archiver index` command, which still builds leaves CONCURRENTLY
outside SQL transactions and repairs only the expected invalid unattached leaf.
Wrong definitions, ownership or attached invalid indexes are refused, not normalized.
The six existing baseline read-index trees must remain valid and unchanged; an
altered/incomplete baseline tree is refused before activation, never rebuilt
under its root lock. This does not require historical BRIN backfill.
Neither ingest maintenance nor ordinary migrate up invokes that command.

To change the parent PK contract without rebuilding retained heaps/indexes, the
owner-only atomic transition detaches the existing class/day branches under the
root lock, temporarily removes the **three known empty partitioned BRIN metadata**
indexes (root/HOT/WARM), reattaches the tables before restoring those definitions
with `ON ONLY`, then rebinds only the recorded existing physical-index edges.
Those three metadata OIDs change. The root TABLE OID, all historical heap/physical
index identities and the COLD BRIN parent identity remain. Missing historical
indexes stay missing; invalid unattached old indexes are retained for an approved
manual repair. No historical index/heap build or table copy happens during this
transition. Exact schema, owner, kind, BRIN definition/options, edge and empty-child
proofs gate removal. Unexpected PK/FK/parent-index inventory refuses the entire
transaction; names alone never authorize dropping or adopting a relation.

Preparation is a separate owner-approved window: validated exact class/day CHECKs
may scan populated heaps, **before** activation's root lock. Forged tautologies,
wrong predicates or non-CHECK constraints are not accepted. Runtime retention
uses private same-transaction detach provenance and locked full subtree shape
proofs before DROP; changed names, bounds, topology or keys retain all data and
proof records. New writes to detached buckets are bound by their original checks.
Logical restores changing OIDs need owner review, never automatic name-only rebinding.

`HourlyTransactionEventPartitions::activate` uses a dedicated sqlx-locked session,
then the retention lock, with bounded DDL timeouts. Its boolean is an explicit
all-writers-bridged assertion, not permission to bypass an approval. Select no
production date during normal migration: approved UTC T must exceed every
existing HOT/WARM daily partition (including empty ones) and database now +73h.
The actual hourly look-ahead must be attached before admission reaches T -1h.
All detach/rebind work is one transaction, so root readers see no committed
coverage gap; reader lock waits can abort it without changing policy/history.

Existing Datadog sizing supports this hourly target, not demonstrated production
IO improvement. A separately approved rollout must verify IO/WAL/storage, ingest
and pool/lock latency, complete read plans and the UI's per-RPC timeout budget.
Local fixtures establish correctness/catalog behavior, not production load benefit.

### Owned `PostgreSQL` validation

Docker remains the default existing Postgres test fixture. For explicit local
PG17 testing without Docker, set `TIPS_AUDIT_TEST_PG_BIN` to PG17 utilities under
this checkout's `.tmp/`. The source-owned fixtures create a separate private
loopback cluster per test, verify its exact directory/version, and stop only that
directory on drop. Disposable trust credentials are not external DB authorization.

```bash
export TIPS_AUDIT_TEST_PG_BIN="$PWD/.tmp/<owned-pg17>/bin"
cargo test -j 2 -p audit-archiver-lib --test postgres_transaction_events \
  -- --include-ignored --test-threads=1
cargo test -j 2 -p audit-archiver-lib --test hourly_partitions \
  -- --ignored --test-threads=1
```

Regenerate `schema.sql` using the existing snapshot test and
`UPDATE_SCHEMA_SNAPSHOT=1`, then compare it again without the update flag.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
