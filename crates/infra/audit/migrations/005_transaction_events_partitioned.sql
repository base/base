-- Replace the unpartitioned transaction_events table with one partitioned by
-- retention class, then by UTC day of event_time.
--
-- THIS MIGRATION DISCARDS EVERY EXISTING transaction_events ROW. Postgres is
-- the operational query window, not the archive, and copying a bloated
-- multi-terabyte table into partitions would cost more I/O than the database
-- can spare. Only audit-owned objects are dropped: the database, schema,
-- roles, and _sqlx_migrations history stay intact.
--
-- Retention drops whole day partitions instead of deleting rows, so expiry
-- no longer produces dead tuples, index bloat, or long vacuums. Each day's
-- random-key indexes (event_id, tx_hash, ...) stay small enough to cache.
--
-- Bound the DROP's lock wait so a long-running vacuum or query on the old
-- table fails this migration quickly instead of queueing ingest behind it.
-- The migrator can simply be retried.
SET LOCAL lock_timeout = '60s';

DROP TABLE IF EXISTS transaction_events;

-- event_time and retention_class are in the primary key because Postgres
-- requires unique constraints to include the partition key. Both are fixed
-- per event (event_time comes from the producer, retention_class from
-- event_type), so a retried event still conflicts with its first insert.
CREATE TABLE transaction_events (
    event_id TEXT NOT NULL,
    schema_version TEXT NOT NULL,
    event_time TIMESTAMPTZ NOT NULL,
    ingested_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    retention_class TEXT NOT NULL,
    producer TEXT NOT NULL,
    event_type TEXT NOT NULL,
    network TEXT,
    tx_hash TEXT,
    block_hash TEXT,
    block_number BIGINT,
    payload_id TEXT,
    request_id TEXT,
    data JSONB NOT NULL,
    PRIMARY KEY (event_id, retention_class, event_time)
) PARTITION BY LIST (retention_class);

CREATE TABLE transaction_events_hot PARTITION OF transaction_events
    FOR VALUES IN ('hot') PARTITION BY RANGE (event_time);
CREATE TABLE transaction_events_warm PARTITION OF transaction_events
    FOR VALUES IN ('warm') PARTITION BY RANGE (event_time);
CREATE TABLE transaction_events_cold PARTITION OF transaction_events
    FOR VALUES IN ('cold') PARTITION BY RANGE (event_time);

-- Only indexes used by audit-archiver read APIs. The unused payload_id and
-- producer/event_type indexes from 001 and the retention index from 002 are
-- not recreated; each index is write amplification on every insert.
CREATE INDEX transaction_events_tx_hash_event_time_idx
    ON transaction_events (tx_hash, event_time)
    WHERE tx_hash IS NOT NULL;

CREATE INDEX transaction_events_block_number_event_time_idx
    ON transaction_events (block_number, event_time)
    WHERE block_number IS NOT NULL;

CREATE INDEX transaction_events_block_hash_event_time_idx
    ON transaction_events (block_hash, event_time)
    WHERE block_hash IS NOT NULL;

CREATE INDEX transaction_events_rejected_event_time_idx
    ON transaction_events (event_type, event_time DESC)
    WHERE event_type IN ('SIMULATION_FAILED', 'BUILDER_REJECTED', 'BUILDER_EXPIRED');

CREATE INDEX transaction_events_bundle_hash_event_time_idx
    ON transaction_events ((data->>'bundle_hash'), event_time)
    WHERE data ? 'bundle_hash';

CREATE INDEX transaction_events_bundle_id_event_time_idx
    ON transaction_events ((data->>'bundle_id'), event_time)
    WHERE data ? 'bundle_id';

-- Partition DDL requires owning the partitioned table, which the runtime role
-- does not. These SECURITY DEFINER functions run as the migration role and
-- only touch day partitions whose names they derive from a validated
-- retention class and date.
--
-- Create uses CREATE TABLE + ATTACH PARTITION so the parent only takes a
-- SHARE UPDATE EXCLUSIVE lock and concurrent inserts keep flowing.
CREATE FUNCTION transaction_events_create_partition(p_class TEXT, p_day DATE)
RETURNS BOOLEAN
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = public, pg_temp
AS $$
DECLARE
    parent_name TEXT;
    partition_name TEXT;
BEGIN
    IF p_class IS NULL OR p_class NOT IN ('hot', 'warm', 'cold') THEN
        RAISE EXCEPTION 'unknown transaction event retention class: %', p_class;
    END IF;
    IF p_day IS NULL THEN
        RAISE EXCEPTION 'transaction event partition day is required';
    END IF;

    parent_name := 'transaction_events_' || p_class;
    partition_name := parent_name || '_' || to_char(p_day, 'YYYYMMDD');
    IF to_regclass(format('public.%I', partition_name)) IS NOT NULL THEN
        RETURN FALSE;
    END IF;

    EXECUTE format(
        'CREATE TABLE public.%I (LIKE public.%I INCLUDING DEFAULTS)',
        partition_name,
        parent_name
    );
    EXECUTE format(
        'ALTER TABLE public.%I ATTACH PARTITION public.%I FOR VALUES FROM (%L) TO (%L)',
        parent_name,
        partition_name,
        to_char(p_day, 'YYYY-MM-DD') || ' 00:00:00+00',
        to_char(p_day + 1, 'YYYY-MM-DD') || ' 00:00:00+00'
    );
    RETURN TRUE;
END;
$$;

-- Detach and drop are separate calls so the parent's ACCESS EXCLUSIVE lock
-- from DETACH is released before DROP unlinks the partition's files.
CREATE FUNCTION transaction_events_detach_partition(p_class TEXT, p_day DATE)
RETURNS BOOLEAN
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = public, pg_temp
AS $$
DECLARE
    parent_name TEXT;
    partition_name TEXT;
BEGIN
    IF p_class IS NULL OR p_class NOT IN ('hot', 'warm', 'cold') THEN
        RAISE EXCEPTION 'unknown transaction event retention class: %', p_class;
    END IF;
    IF p_day IS NULL THEN
        RAISE EXCEPTION 'transaction event partition day is required';
    END IF;

    parent_name := 'transaction_events_' || p_class;
    partition_name := parent_name || '_' || to_char(p_day, 'YYYYMMDD');
    IF NOT EXISTS (
        SELECT 1
        FROM pg_inherits
        WHERE inhrelid = to_regclass(format('public.%I', partition_name))
          AND inhparent = to_regclass(format('public.%I', parent_name))
    ) THEN
        RETURN FALSE;
    END IF;

    EXECUTE format(
        'ALTER TABLE public.%I DETACH PARTITION public.%I',
        parent_name,
        partition_name
    );
    RETURN TRUE;
END;
$$;

CREATE FUNCTION transaction_events_drop_detached_partition(p_class TEXT, p_day DATE)
RETURNS BOOLEAN
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = public, pg_temp
AS $$
DECLARE
    partition_name TEXT;
    partition_oid REGCLASS;
BEGIN
    IF p_class IS NULL OR p_class NOT IN ('hot', 'warm', 'cold') THEN
        RAISE EXCEPTION 'unknown transaction event retention class: %', p_class;
    END IF;
    IF p_day IS NULL THEN
        RAISE EXCEPTION 'transaction event partition day is required';
    END IF;

    partition_name := 'transaction_events_' || p_class || '_' || to_char(p_day, 'YYYYMMDD');
    partition_oid := to_regclass(format('public.%I', partition_name));
    IF partition_oid IS NULL THEN
        RETURN FALSE;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_class WHERE oid = partition_oid AND relispartition) THEN
        RAISE EXCEPTION 'transaction event partition % is still attached', partition_name;
    END IF;

    EXECUTE format('DROP TABLE public.%I', partition_name);
    RETURN TRUE;
END;
$$;

REVOKE ALL ON FUNCTION transaction_events_create_partition(TEXT, DATE) FROM PUBLIC;
REVOKE ALL ON FUNCTION transaction_events_detach_partition(TEXT, DATE) FROM PUBLIC;
REVOKE ALL ON FUNCTION transaction_events_drop_detached_partition(TEXT, DATE) FROM PUBLIC;

-- Seed yesterday through three days ahead so ingest works before the first
-- runtime maintenance pass. Runtime maintenance backfills the rest of each
-- retention window and keeps creating days ahead.
DO $$
DECLARE
    retention_class TEXT;
    day_offset INTEGER;
BEGIN
    FOREACH retention_class IN ARRAY ARRAY['hot', 'warm', 'cold'] LOOP
        FOR day_offset IN -1..3 LOOP
            PERFORM transaction_events_create_partition(
                retention_class,
                (now() AT TIME ZONE 'UTC')::date + day_offset
            );
        END LOOP;
    END LOOP;
END $$;

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'audit_archiver') THEN
        GRANT SELECT ON _sqlx_migrations TO audit_archiver;
        GRANT SELECT, INSERT, UPDATE, DELETE ON transaction_events TO audit_archiver;
        GRANT EXECUTE ON FUNCTION transaction_events_create_partition(TEXT, DATE)
            TO audit_archiver;
        GRANT EXECUTE ON FUNCTION transaction_events_detach_partition(TEXT, DATE)
            TO audit_archiver;
        GRANT EXECUTE ON FUNCTION transaction_events_drop_detached_partition(TEXT, DATE)
            TO audit_archiver;
    END IF;
END $$;
