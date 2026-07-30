-- Add three independent retention tables, each range-partitioned by UTC
-- ingested_at day. Keep the legacy transaction_events heap intact; a future
-- migration may drop it and create a union view under that name.

-- Define the row and index shape once, then copy it into each independent
-- partitioned parent. LIKE copies definitions only; the template stays empty.
CREATE TABLE transaction_events_template (
    event_id TEXT NOT NULL,
    retention_class TEXT NOT NULL,
    schema_version TEXT NOT NULL,
    event_time TIMESTAMPTZ NOT NULL,
    ingested_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    producer TEXT NOT NULL,
    event_type TEXT NOT NULL,
    network TEXT,
    tx_hash TEXT,
    block_hash TEXT,
    block_number BIGINT,
    payload_id TEXT,
    request_id TEXT,
    data JSONB NOT NULL,
    PRIMARY KEY (event_id, ingested_at)
);
CREATE INDEX ON transaction_events_template (tx_hash, event_time)
    WHERE tx_hash IS NOT NULL;
CREATE INDEX ON transaction_events_template (block_number, event_time)
    WHERE block_number IS NOT NULL;
CREATE INDEX ON transaction_events_template (block_hash, event_time)
    WHERE block_hash IS NOT NULL;
CREATE INDEX ON transaction_events_template (payload_id, event_time)
    WHERE payload_id IS NOT NULL;
CREATE INDEX ON transaction_events_template (producer, event_type, event_time);
CREATE INDEX ON transaction_events_template (event_type, event_time DESC)
    WHERE event_type IN (
        'PROXY_REJECTED',
        'PROXY_VALIDATION_REJECTED',
        'SIMULATION_FAILED',
        'TXPOOL_VALIDATED_INSERT_REJECTED',
        'BUILDER_REJECTED'
    );
CREATE INDEX ON transaction_events_template ((data->>'bundle_hash'), event_time)
    WHERE data ? 'bundle_hash';
CREATE INDEX ON transaction_events_template ((data->>'bundle_id'), event_time)
    WHERE data ? 'bundle_id';

CREATE TABLE transaction_events_hot (
    LIKE transaction_events_template INCLUDING ALL
) PARTITION BY RANGE (ingested_at);
ALTER TABLE transaction_events_hot ADD CHECK (retention_class = 'hot');
CREATE TABLE transaction_events_warm (
    LIKE transaction_events_template INCLUDING ALL
) PARTITION BY RANGE (ingested_at);
ALTER TABLE transaction_events_warm ADD CHECK (retention_class = 'warm');
CREATE TABLE transaction_events_cold (
    LIKE transaction_events_template INCLUDING ALL
) PARTITION BY RANGE (ingested_at);
ALTER TABLE transaction_events_cold ADD CHECK (retention_class = 'cold');
DROP TABLE transaction_events_template;

CREATE FUNCTION create_transaction_event_partition(
    p_retention_class TEXT,
    p_partition_day DATE
)
RETURNS BOOLEAN
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
DECLARE
    parent_name NAME;
    child_name NAME;
    child_index_name NAME;
    lower_bound TIMESTAMPTZ;
    upper_bound TIMESTAMPTZ;
BEGIN
    IF p_retention_class NOT IN ('hot', 'warm', 'cold') THEN
        RAISE EXCEPTION 'invalid transaction event retention class: %', p_retention_class;
    END IF;

    parent_name := ('transaction_events_' || p_retention_class)::NAME;
    child_name :=
        ('transaction_events_' || p_retention_class || '_' || to_char(p_partition_day, 'YYYYMMDD'))::NAME;
    child_index_name := (child_name::TEXT || '_event_id_uidx')::NAME;
    lower_bound := p_partition_day::TIMESTAMP AT TIME ZONE 'UTC';
    upper_bound := (p_partition_day + 1)::TIMESTAMP AT TIME ZONE 'UTC';

    IF to_regclass('public.' || quote_ident(child_name::TEXT)) IS NOT NULL THEN
        RETURN FALSE;
    END IF;

    EXECUTE format(
        'CREATE TABLE public.%I PARTITION OF public.%I FOR VALUES FROM (%L) TO (%L)',
        child_name,
        parent_name,
        lower_bound,
        upper_bound
    );
    -- Parent uniqueness must include ingested_at. Preserve retry dedupe within
    -- each UTC ingest day with a leaf-local event_id index.
    EXECUTE format(
        'CREATE UNIQUE INDEX %I ON public.%I (event_id)',
        child_index_name,
        child_name
    );
    RETURN TRUE;
END
$$;

DO $$
DECLARE
    first_day DATE := (now() AT TIME ZONE 'UTC')::DATE;
    last_day DATE := first_day + 7;
    class_name TEXT;
    partition_day DATE;
BEGIN
    FOREACH class_name IN ARRAY ARRAY['hot', 'warm', 'cold']
    LOOP
        FOR partition_day IN
            SELECT generate_series(first_day, last_day, INTERVAL '1 day')::DATE
        LOOP
            PERFORM create_transaction_event_partition(class_name, partition_day);
        END LOOP;
    END LOOP;
END
$$;

CREATE FUNCTION maintain_transaction_event_partitions(
    p_now TIMESTAMPTZ,
    p_premake_days INTEGER,
    p_hot_days INTEGER,
    p_warm_days INTEGER,
    p_cold_days INTEGER
)
RETURNS TABLE (
    retention_class TEXT,
    partitions_created BIGINT,
    partitions_dropped BIGINT,
    oldest_partition_start TIMESTAMPTZ,
    partitions_ahead_days BIGINT
)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
DECLARE
    class_name TEXT;
    parent_name NAME;
    retention_days INTEGER;
    day_offset INTEGER;
    partition_record RECORD;
    today DATE;
    newest_partition_day DATE;
BEGIN
    IF p_premake_days < 1 OR p_premake_days > 31 THEN
        RAISE EXCEPTION 'premake days must be between 1 and 31';
    END IF;
    IF p_hot_days < 1 OR p_warm_days < 1 OR p_cold_days < 1 THEN
        RAISE EXCEPTION 'retention days must be positive';
    END IF;
    IF p_hot_days > p_warm_days OR p_warm_days > p_cold_days THEN
        RAISE EXCEPTION 'retention days must satisfy hot <= warm <= cold';
    END IF;

    -- Every audit-archiver replica may run the worker. Only one performs DDL.
    IF NOT pg_try_advisory_xact_lock(744697762131337711) THEN
        RETURN;
    END IF;

    today := (p_now AT TIME ZONE 'UTC')::DATE;

    FOREACH class_name IN ARRAY ARRAY['hot', 'warm', 'cold']
    LOOP
        parent_name := ('transaction_events_' || class_name)::NAME;
        retention_days := CASE class_name
            WHEN 'hot' THEN p_hot_days
            WHEN 'warm' THEN p_warm_days
            ELSE p_cold_days
        END;
        retention_class := class_name;
        partitions_created := 0;
        partitions_dropped := 0;
        partitions_ahead_days := NULL;
        oldest_partition_start := NULL;

        FOR day_offset IN 0..p_premake_days
        LOOP
            IF public.create_transaction_event_partition(
                class_name,
                today + day_offset
            ) THEN
                partitions_created := partitions_created + 1;
            END IF;
        END LOOP;

        FOR partition_record IN
            SELECT
                child.relname AS partition_name,
                to_date(right(child.relname, 8), 'YYYYMMDD') AS partition_day
            FROM pg_inherits AS inheritance
            JOIN pg_class AS parent ON parent.oid = inheritance.inhparent
            JOIN pg_class AS child ON child.oid = inheritance.inhrelid
            WHERE parent.oid = to_regclass('public.' || quote_ident(parent_name::TEXT))
              AND child.relname ~ ('^' || parent_name::TEXT || '_[0-9]{8}$')
              AND (
                  to_date(right(child.relname, 8), 'YYYYMMDD') + 1
              )::TIMESTAMP AT TIME ZONE 'UTC'
                  <= p_now - make_interval(days => retention_days)
            ORDER BY partition_day
        LOOP
            EXECUTE format('DROP TABLE public.%I', partition_record.partition_name);
            partitions_dropped := partitions_dropped + 1;
        END LOOP;

        SELECT
            min(to_date(right(child.relname, 8), 'YYYYMMDD'))::TIMESTAMP AT TIME ZONE 'UTC',
            max(to_date(right(child.relname, 8), 'YYYYMMDD'))
        INTO oldest_partition_start, newest_partition_day
        FROM pg_inherits AS inheritance
        JOIN pg_class AS parent ON parent.oid = inheritance.inhparent
        JOIN pg_class AS child ON child.oid = inheritance.inhrelid
        WHERE parent.oid = to_regclass('public.' || quote_ident(parent_name::TEXT))
          AND child.relname ~ ('^' || parent_name::TEXT || '_[0-9]{8}$');

        IF newest_partition_day IS NOT NULL THEN
            partitions_ahead_days := newest_partition_day - today;
        END IF;

        RETURN NEXT;
    END LOOP;
END
$$;

REVOKE ALL ON FUNCTION create_transaction_event_partition(TEXT, DATE) FROM PUBLIC;
REVOKE ALL ON FUNCTION maintain_transaction_event_partitions(
    TIMESTAMPTZ,
    INTEGER,
    INTEGER,
    INTEGER,
    INTEGER
) FROM PUBLIC;

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'audit_archiver') THEN
        GRANT SELECT, INSERT, UPDATE, DELETE
            ON transaction_events_hot, transaction_events_warm, transaction_events_cold
            TO audit_archiver;
        GRANT EXECUTE ON FUNCTION maintain_transaction_event_partitions(
            TIMESTAMPTZ,
            INTEGER,
            INTEGER,
            INTEGER,
            INTEGER
        ) TO audit_archiver;
    END IF;
END
$$;
