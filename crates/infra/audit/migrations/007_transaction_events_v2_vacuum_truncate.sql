-- Stops VACUUM from truncating v2 day partitions.
--
-- When a vacuum finds empty pages at the end of a table, it truncates them
-- under an ACCESS EXCLUSIVE lock, which blocks inserts into that partition
-- while it is held. A busy day partition only takes inserts, so autovacuum
-- reaches it on the insert threshold, about every half hour. Inserts that
-- wait out their 1s lock_timeout behind a truncation fail after three
-- attempts. Day partitions are dropped whole by retention and their freed
-- tail pages are refilled by the next inserts, so truncation reclaims
-- nothing worth the lock.
--
-- vacuum_truncate is a per-table storage parameter that partitioned tables do
-- not pass down, so this sets it on every attached day partition and its
-- TOAST table, and transaction_events_v2_create_partition sets it on new
-- ones. Setting it takes a SHARE UPDATE EXCLUSIVE lock, which does not block
-- inserts or reads.
DO $$
DECLARE
    partition_name TEXT;
BEGIN
    FOR partition_name IN
        SELECT c.relname
        FROM pg_inherits i
        JOIN pg_class c ON c.oid = i.inhrelid
        WHERE i.inhparent IN (
            'public.transaction_events_v2_hot'::regclass,
            'public.transaction_events_v2_warm'::regclass,
            'public.transaction_events_v2_cold'::regclass
        )
        ORDER BY c.relname
    LOOP
        EXECUTE format(
            'ALTER TABLE public.%I SET (vacuum_truncate = false, toast.vacuum_truncate = false)',
            partition_name
        );
    END LOOP;
END $$;

CREATE OR REPLACE FUNCTION transaction_events_v2_create_partition(p_class TEXT, p_day DATE)
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
        'CREATE TABLE public.%I (LIKE public.%I INCLUDING DEFAULTS INCLUDING CONSTRAINTS) WITH (vacuum_truncate = false, toast.vacuum_truncate = false)',
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
