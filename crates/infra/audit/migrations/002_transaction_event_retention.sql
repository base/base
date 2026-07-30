-- Replace the heap transaction_events table with retention-class/list partitions
-- whose leaves are daily UTC ingested_at ranges. Existing rows are discarded;
-- pause producers before applying this migration.
DROP TABLE IF EXISTS transaction_events;

CREATE TABLE transaction_events (
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
    PRIMARY KEY (event_id, retention_class, ingested_at),
    CONSTRAINT transaction_events_retention_class_check
        CHECK (retention_class IN ('hot', 'warm', 'cold'))
) PARTITION BY LIST (retention_class);

CREATE TABLE transaction_events_hot
    PARTITION OF transaction_events FOR VALUES IN ('hot')
    PARTITION BY RANGE (ingested_at);
CREATE TABLE transaction_events_warm
    PARTITION OF transaction_events FOR VALUES IN ('warm')
    PARTITION BY RANGE (ingested_at);
CREATE TABLE transaction_events_cold
    PARTITION OF transaction_events FOR VALUES IN ('cold')
    PARTITION BY RANGE (ingested_at);

CREATE TABLE transaction_event_partition_registry (
    retention_class TEXT NOT NULL,
    partition_day DATE NOT NULL,
    partition_name NAME NOT NULL UNIQUE,
    PRIMARY KEY (retention_class, partition_day),
    CONSTRAINT transaction_event_partition_registry_class_check
        CHECK (retention_class IN ('hot', 'warm', 'cold'))
);

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
        INSERT INTO public.transaction_event_partition_registry (
            retention_class,
            partition_day,
            partition_name
        )
        VALUES (p_retention_class, p_partition_day, child_name)
        ON CONFLICT (retention_class, partition_day) DO NOTHING;
        RETURN FALSE;
    END IF;

    EXECUTE format(
        'CREATE TABLE public.%I PARTITION OF public.%I FOR VALUES FROM (%L) TO (%L)',
        child_name,
        parent_name,
        lower_bound,
        upper_bound
    );
    -- The parent key cannot enforce event_id uniqueness without both partition
    -- keys. This leaf index preserves retry dedupe within a UTC ingest day.
    EXECUTE format(
        'CREATE UNIQUE INDEX %I ON public.%I (event_id)',
        child_index_name,
        child_name
    );
    INSERT INTO public.transaction_event_partition_registry (
        retention_class,
        partition_day,
        partition_name
    )
    VALUES (p_retention_class, p_partition_day, child_name);
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

CREATE INDEX transaction_events_partitioned_tx_hash_event_time_idx
    ON transaction_events (tx_hash, event_time)
    WHERE tx_hash IS NOT NULL;
CREATE INDEX transaction_events_partitioned_block_number_event_time_idx
    ON transaction_events (block_number, event_time)
    WHERE block_number IS NOT NULL;
CREATE INDEX transaction_events_partitioned_block_hash_event_time_idx
    ON transaction_events (block_hash, event_time)
    WHERE block_hash IS NOT NULL;
CREATE INDEX transaction_events_partitioned_payload_id_event_time_idx
    ON transaction_events (payload_id, event_time)
    WHERE payload_id IS NOT NULL;
CREATE INDEX transaction_events_partitioned_producer_event_type_event_time_idx
    ON transaction_events (producer, event_type, event_time);
CREATE INDEX transaction_events_partitioned_rejected_event_time_idx
    ON transaction_events (event_type, event_time DESC)
    WHERE event_type IN (
        'PROXY_REJECTED',
        'PROXY_VALIDATION_REJECTED',
        'SIMULATION_FAILED',
        'TXPOOL_VALIDATED_INSERT_REJECTED',
        'BUILDER_REJECTED'
    );
CREATE INDEX transaction_events_partitioned_bundle_hash_event_time_idx
    ON transaction_events ((data->>'bundle_hash'), event_time)
    WHERE data ? 'bundle_hash';
CREATE INDEX transaction_events_partitioned_bundle_id_event_time_idx
    ON transaction_events ((data->>'bundle_id'), event_time)
    WHERE data ? 'bundle_id';

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
    oldest_partition_start TIMESTAMPTZ
)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
DECLARE
    class_name TEXT;
    retention_days INTEGER;
    day_offset INTEGER;
    partition_record RECORD;
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

    -- All audit-archiver replicas may run the worker. Only one performs DDL in
    -- a maintenance transaction; the others safely return no rows.
    IF NOT pg_try_advisory_xact_lock(744697762131337711) THEN
        RETURN;
    END IF;

    FOREACH class_name IN ARRAY ARRAY['hot', 'warm', 'cold']
    LOOP
        retention_days := CASE class_name
            WHEN 'hot' THEN p_hot_days
            WHEN 'warm' THEN p_warm_days
            ELSE p_cold_days
        END;
        retention_class := class_name;
        partitions_created := 0;
        partitions_dropped := 0;

        FOR day_offset IN 0..p_premake_days
        LOOP
            IF public.create_transaction_event_partition(
                class_name,
                (p_now AT TIME ZONE 'UTC')::DATE + day_offset
            ) THEN
                partitions_created := partitions_created + 1;
            END IF;
        END LOOP;

        FOR partition_record IN
            SELECT registry.partition_day, registry.partition_name
            FROM public.transaction_event_partition_registry AS registry
            WHERE registry.retention_class = class_name
              AND (
                  (registry.partition_day + 1)::TIMESTAMP AT TIME ZONE 'UTC'
              ) <= p_now - make_interval(days => retention_days)
            ORDER BY registry.partition_day
        LOOP
            EXECUTE format('DROP TABLE public.%I', partition_record.partition_name);
            DELETE FROM public.transaction_event_partition_registry AS registry
            WHERE registry.retention_class = class_name
              AND registry.partition_day = partition_record.partition_day;
            partitions_dropped := partitions_dropped + 1;
        END LOOP;

        SELECT
            min(registry.partition_day)::TIMESTAMP AT TIME ZONE 'UTC'
        INTO oldest_partition_start
        FROM public.transaction_event_partition_registry AS registry
        WHERE registry.retention_class = class_name;

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
        GRANT SELECT, INSERT, UPDATE, DELETE ON transaction_events TO audit_archiver;
        GRANT SELECT ON transaction_event_partition_registry TO audit_archiver;
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
