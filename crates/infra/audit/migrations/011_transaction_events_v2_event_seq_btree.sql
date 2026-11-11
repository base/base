-- Adds a btree index on event_seq so each warehouse extraction slice reads only
-- its own event_seq range.
--
-- DataPilot splits an incremental window into event_seq ranges and fetches
-- each with
--   ... WHERE ingested_at >= $cutoff AND event_seq >= $lo AND event_seq < $hi
-- The only event_seq index is BRIN, and the planner prices a BRIN bitmap scan
-- as scattered page reads. On a large day partition that came out above a
-- sequential scan, so on mainnet every slice read the whole hot partition
-- (about 90M pages) to return its own few percent of it. event_seq follows
-- physical order almost exactly (correlation 0.99999), so the planner prices a
-- range scan on a btree as reading just that slice of the heap in order, far
-- below a sequential scan.
--
-- event_seq is assigned at insert, so new entries land at the right edge of
-- the index (spread over a few pages by the identity sequence's CACHE 100)
-- and stay cached, like the (ingested_at, event_seq) index from 008. The BRIN
-- index stays; it is small.
--
-- sqlx runs each migration in a transaction, which rules out CREATE INDEX
-- CONCURRENTLY. Like 008 and 009, this creates the partitioned indexes ON ONLY
-- and builds leaf indexes only on day partitions that are still empty, which
-- takes milliseconds. Day partitions created later get the index when they
-- attach. Populated partitions stay without it until retention drops them.
--
-- CREATE INDEX on the parents takes a SHARE lock that blocks inserts through
-- them until this migration commits, so the lock timeout makes the migration
-- fail and leave ingest alone rather than queue behind a long transaction.
SET LOCAL lock_timeout = '5s';

CREATE INDEX transaction_events_v2_event_seq_btree_idx
    ON ONLY transaction_events_v2 (event_seq);

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
        parent_index := parent_name || '_event_seq_btree_idx';
        EXECUTE format(
            'CREATE INDEX %I ON ONLY public.%I (event_seq)',
            parent_index,
            parent_name
        );
        EXECUTE format(
            'ALTER INDEX public.transaction_events_v2_event_seq_btree_idx ATTACH PARTITION public.%I',
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

            partition_index := partition_name || '_event_seq_btree_idx';
            EXECUTE format(
                'CREATE INDEX %I ON public.%I (event_seq)',
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
