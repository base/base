-- Adds a btree index on (ingested_at, event_seq) so warehouse extraction can
-- plan its incremental window without reading the window's heap.
--
-- Before fetching, DataPilot's range split runs
--   SELECT MIN(event_seq), MAX(event_seq), COUNT(*) ... WHERE ingested_at >= $cutoff
-- over the whole incremental window. The BRIN index on ingested_at only
-- narrows which blocks to read, so that query still reads every row of the
-- window from the heap, about 70 GB per hour of events, before any slice
-- starts. With this index the query is an index-only scan over two 8-byte
-- columns. Pages that autovacuum has not yet marked all-visible still need a
-- heap check; those are the newest pages, which are likely still cached.
--
-- ingested_at is set from now() at insert, so new entries append at the right
-- edge of the index rather than landing on random pages like the read-API
-- hash indexes. The BRIN indexes stay: each extraction slice reads its
-- event_seq range through the event_seq BRIN index.
--
-- sqlx runs each migration in a transaction, which rules out CREATE INDEX
-- CONCURRENTLY. Building leaf indexes on populated day partitions here would
-- block inserts into them for the length of the build, so like 002 this
-- creates the partitioned indexes ON ONLY and builds leaf indexes only on day
-- partitions that are still empty, which takes milliseconds. Day partitions
-- created later get the index when they attach. Populated partitions stay
-- without it and age out through retention; until then the parent indexes
-- stay invalid, which does not stop the planner from using the leaf indexes.
--
-- CREATE INDEX on the parents takes a SHARE lock that blocks inserts through
-- them until this migration commits, so the lock timeout makes the migration
-- fail and leave ingest alone rather than queue behind a long transaction.
SET LOCAL lock_timeout = '5s';

CREATE INDEX transaction_events_v2_ingested_at_event_seq_idx
    ON ONLY transaction_events_v2 (ingested_at, event_seq);

DO $$
DECLARE
    class_name TEXT;
    parent_name TEXT;
    parent_index TEXT;
    partition_name TEXT;
    partition_index TEXT;
    is_empty BOOLEAN;
BEGIN
    FOREACH class_name IN ARRAY ARRAY['hot', 'warm', 'cold']
    LOOP
        parent_name := 'transaction_events_v2_' || class_name;
        parent_index := parent_name || '_ingested_at_event_seq_idx';
        EXECUTE format(
            'CREATE INDEX %I ON ONLY public.%I (ingested_at, event_seq)',
            parent_index,
            parent_name
        );
        EXECUTE format(
            'ALTER INDEX public.transaction_events_v2_ingested_at_event_seq_idx ATTACH PARTITION public.%I',
            parent_index
        );

        FOR partition_name IN
            SELECT c.relname
            FROM pg_inherits i
            JOIN pg_class c ON c.oid = i.inhrelid
            WHERE i.inhparent = format('public.%I', parent_name)::regclass
            ORDER BY c.relname
        LOOP
            EXECUTE format('SELECT NOT EXISTS (SELECT 1 FROM ONLY public.%I)', partition_name)
                INTO is_empty;
            CONTINUE WHEN NOT is_empty;

            partition_index := partition_name || '_ingested_at_event_seq_idx';
            EXECUTE format(
                'CREATE INDEX %I ON public.%I (ingested_at, event_seq)',
                partition_index,
                partition_name
            );
            EXECUTE format(
                'ALTER INDEX public.%I ATTACH PARTITION public.%I',
                parent_index,
                partition_index
            );
        END LOOP;
    END LOOP;
END $$;
