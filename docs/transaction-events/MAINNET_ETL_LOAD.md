# Mainnet transaction-event ETL load

This note records how the hourly Snowflake load of mainnet transaction events
(the DataPilot ETL from the TIPS audit Postgres, table `transaction_events_v2`)
spends its time, how many rows each event type contributes, and how to measure
the effect of a change that removes events. It was written for the change that
journals each builder verdict once (no `TXPOOL_VALIDATED_INSERT_REJECTED`, no
`already imported` forward failures) and is meant to be re-run after any event
change ships. Row counts are per hour of `event_hour` unless stated otherwise.

## Where the hourly run spends its time

Each run of the Airflow DAG
`etl_POSTGRES_web3-shared-prod.protocols-tips-mainnet-transact-db-b39e28.transaction_observability_SNOWFLAKE`
has three phases, visible in the `..._load_s3_data` task log:

1. **Pre-scan**: `SELECT MIN(event_seq), MAX(event_seq), COUNT(*) FROM
   transaction_events_v2 WHERE ingested_at >= <cutoff>`, where the cutoff is
   Snowflake's `MAX(ingested_at)` minus 15 minutes. `ingested_at` has no index
   and the table is partitioned by `retention_class` and `event_hour`, so this
   scans every partition. Its duration tracks table size and daytime write
   load, not the rows it returns.
2. **Copy**: eight parallel slices over `event_seq` ranges copy the rows to S3.
   Throughput has been 45–55k rows/s in every run measured (Oct 21 to Nov 6),
   so this phase costs about 20 minutes per 60M rows.
3. **Load**: staging into Snowflake, 5–22 minutes, growing with rows.

Runs from 2026-11-02 17:00 UTC (after validity transactions stopped being
resent every two blocks) to 2026-11-06 04:00 UTC:

| Window (UTC)           | Rows/run   | Pre-scan   | Copy       | Total       |
| ---------------------- | ---------- | ---------- | ---------- | ----------- |
| Nov 3 01:00–06:00      | 44–63M     | 24–29 min  | 15–20 min  | 49–52 min   |
| Nov 3 07:00–22:00      | 53–136M    | 31–58 min  | 18–53 min  | 58–114 min  |
| Nov 4                  | 141–243M   | 35–90 min  | 47–83 min  | 103–177 min |
| Nov 5                  | 168–382M   | 45–105 min | 53–141 min | 118–279 min |

Only the overnight Nov 3 runs, at about 50M rows/h, finished inside the
60-minute budget, and half of each of those runs was the pre-scan. Once a run
overruns, the next scheduled hours are skipped and the following run carries
several hours of rows, which is why rows per run climb through each day.

What this means for optimisation, in order of effect:

- The pre-scan is 40–55% of every run and grows daily until cold partitions
  start dropping (about Nov 20). Making the incremental filter prunable removes
  it almost entirely: filter on `event_seq` (identity column, BRIN-indexed,
  already the slice column) instead of `ingested_at`, or add a BRIN index on
  `ingested_at`, or add an `event_hour` lower bound so Postgres prunes to the
  current day's partitions.
- The copy phase has a fixed ceiling of about 180M rows/h at eight slices. At
  the Nov 3–5 average of 68M rows/h it costs about 22 minutes; at the Nov 5
  peak of 105M rows/h, 35 minutes. More slices, or fewer rows, raise headroom.
- Row volume still matters once the pre-scan is fixed: copy plus load at
  today's volume is 30–45 minutes, so sequencer events (30–45M rows/h more
  after #6130) only fit with the pre-scan gone or with fewer rows.

## Rows per hour by event type

Averaged over 2026-11-03 00:00 to 2026-11-06 00:00 UTC (72 hours), sequencer
events disabled:

| Producer            | Event type                             | M rows/h | Share |
| ------------------- | -------------------------------------- | -------- | ----- |
| base-routing/proxyd | `PROXY_RECEIVED`                       | 17.7     | 26%   |
| base-routing/proxyd | `PROXY_BACKEND_SUCCESS`                | 12.3     | 18%   |
| base-reth-node      | `TXPOOL_SEND_RAW_TRANSACTION_VALIDITY` | 11.6     | 17%   |
| base-reth-node      | `TXPOOL_BUILDER_FORWARD_FAILURE`       | 7.3      | 11%   |
| base-reth-node      | `TXPOOL_REPLACED`                      | 6.3      | 9%    |
| base-reth-node      | `TXPOOL_PENDING`                       | 5.8      | 9%    |
| base-reth-node      | `TXPOOL_DROPPED`                       | 4.9      | 7%    |
| base-reth-node      | everything else                        | 2.4      | 3%    |

The one hour with sequencer events on (2026-10-20 17:00 UTC, before #6130 and
before the resend change) added 83.4M rows from the five `sequencer-*` pods:
`BUILDER_DEFERRED` 48.9M (leader only), `TXPOOL_VALIDATED_INSERT_ACCEPTED`
22.0M (4.4M per pod), `TXPOOL_VALIDATED_INSERT_REJECTED` 11.7M (2.3M per pod,
99.9% `already imported`), all other builder events 0.8M.

## Effect of journaling each builder verdict once

Rows removed per hour, and the copy time they cost at 50k rows/s:

| Scenario                                        | Rows removed/h  | Copy time/h   |
| ----------------------------------------------- | --------------- | ------------- |
| Mempool side, before Nov 3 (resend every 400ms) | 20–45M          | 7–15 min      |
| Mempool side, Nov 3 onward                      | 3–5k            | about 0       |
| Sequencer side, at Oct 20 rates                 | 11.7M           | 3.9 min       |
| Sequencer side, at Nov 3–5 rejection mix        | 3.3–7.3M        | 1.1–2.4 min   |

The mempool-side rows were `TXPOOL_BUILDER_FORWARD_FAILURE` with
`already imported`; #6401 removed their cause for validity transactions on
Nov 3, so the change now only guards that path. The sequencer-side rows are
`TXPOOL_VALIDATED_INSERT_REJECTED`, which is retired. The Nov 3–5 projection
counts the forward-failure reasons the builders were observed journaling 1:1
on Oct 20 (`insufficient gas price to replace`, 3.3M/h on Nov 4) and the pool
size rejections that appeared after it (1.7M/h); it excludes
`transaction nonce is not consistent` (3.1M/h), which the builders did not
journal on Oct 20. The upper bound, if every rejecting destination journals,
is the full forward-failure rate of 9.5M/h.

## Queries

Rows per hour by producer and event type, for a window:

```sql
SELECT producer, event_type,
       ROUND(COUNT(*) / COUNT(DISTINCT event_hour) / 1e6, 2) AS m_per_hour,
       ROUND(100 * COUNT(*) / SUM(COUNT(*)) OVER (), 1) AS pct
FROM ANALYTICS.TRANSACTION_OBSERVABILITY_BASE_MAINNET.TRANSACTION_EVENTS_V2
WHERE event_hour >= '2026-11-03' AND event_hour < '2026-11-06'
GROUP BY 1, 2
ORDER BY 3 DESC;
```

Forward failures and insert rejections by reason, with hashes and nonces
normalised:

```sql
SELECT event_type,
       REGEXP_REPLACE(REGEXP_REPLACE(data:error::string, '0x[0-9a-fA-F]{64}', '<hash>'),
                      'nonce [0-9]+', 'nonce N') AS reason,
       COUNT(*) AS n
FROM ANALYTICS.TRANSACTION_OBSERVABILITY_BASE_MAINNET.TRANSACTION_EVENTS_V2
WHERE event_hour >= '2026-11-04' AND event_hour < '2026-11-05'
  AND event_type IN ('TXPOOL_BUILDER_FORWARD_FAILURE', 'TXPOOL_VALIDATED_INSERT_REJECTED')
GROUP BY 1, 2
ORDER BY 3 DESC;
```

Per-pod split of sequencer events for one hour:

```sql
SELECT event_type, data:observability_source:kubernetes:pod_name::string AS pod, COUNT(*) AS n
FROM ANALYTICS.TRANSACTION_OBSERVABILITY_BASE_MAINNET.TRANSACTION_EVENTS_V2
WHERE event_hour = '2026-10-20 17:00:00' AND producer = 'base-builder'
GROUP BY 1, 2
ORDER BY 1, 2;
```

Run phases come from the Airflow task log of
`transaction_observability_base_mainnet.transaction_events_v2_load_s3_data`:
the pre-scan runs from `Generating slices with range strategy` to
`Range strategy - Min: ..., Count: ...`, and the copy from there to the last
`Completed future N/8` line.
