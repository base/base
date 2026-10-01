-- Register a BRIN index for incremental ETL's ingested_at range predicate.
--
-- Indexing the populated day partitions here would block audit ingest and make
-- the normal migrator rollout take hours. These ON ONLY statements create
-- partitioned index metadata without building leaf indexes. Run the separate
-- `audit-archiver index` command to build existing day indexes CONCURRENTLY,
-- one at a time, and attach them. Day partitions created in the meantime get
-- their index automatically when they attach to the indexed parent.
--
-- BRIN keeps write amplification small on this append-heavy, time-correlated
-- table. It accelerates the ingested_at cutoff; a DATE_PART('epoch', ingested_at)
-- split expression still needs separate query-plan validation before use.
CREATE INDEX transaction_events_ingested_at_idx
    ON ONLY transaction_events USING brin (ingested_at);

CREATE INDEX transaction_events_hot_ingested_at_idx
    ON ONLY transaction_events_hot USING brin (ingested_at);
CREATE INDEX transaction_events_warm_ingested_at_idx
    ON ONLY transaction_events_warm USING brin (ingested_at);
CREATE INDEX transaction_events_cold_ingested_at_idx
    ON ONLY transaction_events_cold USING brin (ingested_at);

ALTER INDEX transaction_events_ingested_at_idx
    ATTACH PARTITION transaction_events_hot_ingested_at_idx;
ALTER INDEX transaction_events_ingested_at_idx
    ATTACH PARTITION transaction_events_warm_ingested_at_idx;
ALTER INDEX transaction_events_ingested_at_idx
    ATTACH PARTITION transaction_events_cold_ingested_at_idx;
