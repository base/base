-- transaction_events_v2: the same class-then-UTC-day partition tree as
-- transaction_events, with an hour-leading primary key and binary hashes.
--
-- The legacy primary key leads with event_id, a random 66-character hex
-- string, so every insert lands on a random page of the current day's index.
-- Its working set is the whole day's primary key, which grows past the buffer
-- cache during the day and turns each insert into random reads. Leading with
-- the UTC hour of event_time confines inserts to the current hour's key range,
-- so the hot part of each day's primary key stays roughly one hour wide.
--
-- event_id stays TEXT because producers outside this repository may send
-- non-hex ids. COLLATE "C" compares bytes instead of running locale-aware
-- comparisons on every key. tx_hash and block_hash are already parsed as
-- 32-byte hashes at ingest, so they are stored as BYTEA, which halves their
-- index keys and removes the case and 0x-prefix variants from lookups.
--
-- audit-archiver writes only to this tree. The legacy tree keeps its name so
-- pods still running the previous release can insert during a rolling
-- deploy. It receives no new day partitions and drains as its existing days
-- age out of retention. Readers query both trees until then.

-- Postgres requires unique constraints to include every partition key, so the
-- primary key is (event_hour, retention_class, event_id). retention_class is
-- fixed per event_type, and event_hour is the UTC hour of event_time. A
-- producer that re-emits the same event_id with a new event_time in the same
-- UTC hour still dedupes; re-emissions that straddle an hour boundary store a
-- second row.
--
-- event_seq numbers rows in insertion order. Warehouse extraction splits each
-- incremental window into ranges of event_seq and fetches them in parallel,
-- which an ingested_at cutoff alone cannot do. Rows are inserted through the
-- parent, which assigns the value from one identity sequence shared by every
-- partition. CACHE lets each connection reserve 100 values per sequence
-- access, so concurrent ingest does not serialize on it; values are unique but
-- may be out of order across connections and have gaps.
CREATE TABLE transaction_events_v2 (
    event_id TEXT COLLATE "C" NOT NULL,
    event_seq BIGINT GENERATED ALWAYS AS IDENTITY (CACHE 100),
    schema_version TEXT NOT NULL,
    event_time TIMESTAMPTZ NOT NULL,
    event_hour TIMESTAMPTZ NOT NULL,
    ingested_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    retention_class TEXT NOT NULL,
    producer TEXT NOT NULL,
    event_type TEXT NOT NULL,
    network TEXT,
    tx_hash BYTEA,
    block_hash BYTEA,
    block_number BIGINT,
    payload_id TEXT,
    request_id TEXT,
    data JSONB NOT NULL,
    CONSTRAINT transaction_events_v2_event_hour_check
        CHECK (event_hour = date_trunc('hour', event_time, 'UTC')),
    CONSTRAINT transaction_events_v2_tx_hash_check
        CHECK (octet_length(tx_hash) = 32),
    CONSTRAINT transaction_events_v2_block_hash_check
        CHECK (octet_length(block_hash) = 32),
    PRIMARY KEY (event_hour, retention_class, event_id)
) PARTITION BY LIST (retention_class);

CREATE TABLE transaction_events_v2_hot PARTITION OF transaction_events_v2
    FOR VALUES IN ('hot') PARTITION BY RANGE (event_hour);
CREATE TABLE transaction_events_v2_warm PARTITION OF transaction_events_v2
    FOR VALUES IN ('warm') PARTITION BY RANGE (event_hour);
CREATE TABLE transaction_events_v2_cold PARTITION OF transaction_events_v2
    FOR VALUES IN ('cold') PARTITION BY RANGE (event_hour);

-- The same read-API indexes as the legacy tree, plus BRIN indexes on
-- ingested_at and event_seq for warehouse extraction. Both columns grow with
-- insertion order within a day partition, so block ranges summarize them
-- tightly. The tree is empty here, so the BRIN indexes are built directly
-- rather than through the separate `audit-archiver index` command.
CREATE INDEX transaction_events_v2_tx_hash_event_time_idx
    ON transaction_events_v2 (tx_hash, event_time)
    WHERE tx_hash IS NOT NULL;

CREATE INDEX transaction_events_v2_block_number_event_time_idx
    ON transaction_events_v2 (block_number, event_time)
    WHERE block_number IS NOT NULL;

CREATE INDEX transaction_events_v2_block_hash_event_time_idx
    ON transaction_events_v2 (block_hash, event_time)
    WHERE block_hash IS NOT NULL;

CREATE INDEX transaction_events_v2_rejected_event_time_idx
    ON transaction_events_v2 (event_type, event_time DESC)
    WHERE event_type IN ('SIMULATION_FAILED', 'BUILDER_REJECTED', 'BUILDER_EXPIRED');

CREATE INDEX transaction_events_v2_bundle_hash_event_time_idx
    ON transaction_events_v2 ((data->>'bundle_hash'), event_time)
    WHERE data ? 'bundle_hash';

CREATE INDEX transaction_events_v2_bundle_id_event_time_idx
    ON transaction_events_v2 ((data->>'bundle_id'), event_time)
    WHERE data ? 'bundle_id';

CREATE INDEX transaction_events_v2_ingested_at_idx
    ON transaction_events_v2 USING brin (ingested_at);

CREATE INDEX transaction_events_v2_event_seq_idx
    ON transaction_events_v2 USING brin (event_seq);

-- Day partition DDL for this tree, with the same SECURITY DEFINER contract as
-- the legacy functions in 001. Bounds are UTC midnights written as ISO
-- literals with an explicit offset, so they do not depend on the session's
-- TimeZone or DateStyle. INCLUDING CONSTRAINTS copies the CHECK constraints,
-- which ATTACH PARTITION requires. The LIKE omits INCLUDING IDENTITY because
-- ATTACH PARTITION rejects a table with its own identity column; once
-- attached, the partition takes event_seq from the parent's sequence.
CREATE FUNCTION transaction_events_v2_create_partition(p_class TEXT, p_day DATE)
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

    parent_name := 'transaction_events_v2_' || p_class;
    partition_name := parent_name || '_' || to_char(p_day, 'YYYYMMDD');
    IF to_regclass(format('public.%I', partition_name)) IS NOT NULL THEN
        RETURN FALSE;
    END IF;

    EXECUTE format(
        'CREATE TABLE public.%I (LIKE public.%I INCLUDING DEFAULTS INCLUDING CONSTRAINTS)',
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

CREATE FUNCTION transaction_events_v2_detach_partition(p_class TEXT, p_day DATE)
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

    parent_name := 'transaction_events_v2_' || p_class;
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

CREATE FUNCTION transaction_events_v2_drop_detached_partition(p_class TEXT, p_day DATE)
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

    partition_name := 'transaction_events_v2_' || p_class || '_' || to_char(p_day, 'YYYYMMDD');
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

REVOKE ALL ON FUNCTION transaction_events_v2_create_partition(TEXT, DATE) FROM PUBLIC;
REVOKE ALL ON FUNCTION transaction_events_v2_detach_partition(TEXT, DATE) FROM PUBLIC;
REVOKE ALL ON FUNCTION transaction_events_v2_drop_detached_partition(TEXT, DATE) FROM PUBLIC;

-- Seed each class's default retention window through three days ahead, as
-- 001 does for the legacy tree, so pods that go ready right after this
-- migration can store any event ingest admits under the default config.
DO $$
DECLARE
    class_name TEXT;
    window_days INTEGER;
    today DATE := (now() AT TIME ZONE 'UTC')::date;
    partition_day DATE;
BEGIN
    FOR class_name, window_days IN
        SELECT * FROM (VALUES ('hot', 3), ('warm', 7), ('cold', 30)) AS windows
    LOOP
        partition_day := ((now() - make_interval(days => window_days)) AT TIME ZONE 'UTC')::date;
        WHILE partition_day <= today + 3 LOOP
            PERFORM transaction_events_v2_create_partition(class_name, partition_day);
            partition_day := partition_day + 1;
        END LOOP;
    END LOOP;
END $$;

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'audit_archiver') THEN
        GRANT SELECT, INSERT, UPDATE, DELETE ON transaction_events_v2 TO audit_archiver;
        GRANT EXECUTE ON FUNCTION transaction_events_v2_create_partition(TEXT, DATE)
            TO audit_archiver;
        GRANT EXECUTE ON FUNCTION transaction_events_v2_detach_partition(TEXT, DATE)
            TO audit_archiver;
        GRANT EXECUTE ON FUNCTION transaction_events_v2_drop_detached_partition(TEXT, DATE)
            TO audit_archiver;
    END IF;

    -- DataPilot extracts to the warehouse as a read-only role. SELECT on the
    -- parent covers every leaf read through it, so leaves get no grants.
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'datapilot') THEN
        GRANT SELECT ON transaction_events_v2 TO datapilot;
    END IF;
END $$;
