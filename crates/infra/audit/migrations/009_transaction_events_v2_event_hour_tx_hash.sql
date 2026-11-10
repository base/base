-- Adds an hour-leading index for transaction hash lookups, to replace the
-- (tx_hash, event_time) index that 010 drops.
--
-- Transaction hashes are random, so every insert into a hash-leading btree
-- lands on a random leaf page and the whole day's index is the insert working
-- set. On mainnet that index reached about 190 GB across one day's three
-- partitions by mid-afternoon UTC, past what stays cached, and it accounted
-- for about 260 times the leaf reads of the hour-leading primary key. Leading
-- with event_hour confines inserts to the current hour's slice of the index,
-- the same way the primary key already does.
--
-- Lookups by hash then name the hours to search (event_hour = ANY(...)); see
-- events_by_transaction_hash.
--
-- sqlx runs each migration in a transaction, which rules out CREATE INDEX
-- CONCURRENTLY. Like 008, this creates the partitioned indexes ON ONLY and
-- builds leaf indexes only on day partitions that are still empty, which
-- takes milliseconds. Day partitions created later get the index when they
-- attach. Populated partitions stay without it until retention drops them;
-- the lookup skips them rather than scan them.
--
-- CREATE INDEX on the parents takes a SHARE lock that blocks inserts through
-- them until this migration commits, so the lock timeout makes the migration
-- fail and leave ingest alone rather than queue behind a long transaction.
SET LOCAL lock_timeout = '5s';

CREATE INDEX transaction_events_v2_event_hour_tx_hash_idx
    ON ONLY transaction_events_v2 (event_hour, tx_hash)
    WHERE tx_hash IS NOT NULL;

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
        parent_index := parent_name || '_event_hour_tx_hash_idx';
        EXECUTE format(
            'CREATE INDEX %I ON ONLY public.%I (event_hour, tx_hash) WHERE tx_hash IS NOT NULL',
            parent_index,
            parent_name
        );
        EXECUTE format(
            'ALTER INDEX public.transaction_events_v2_event_hour_tx_hash_idx ATTACH PARTITION public.%I',
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

            partition_index := partition_name || '_event_hour_tx_hash_idx';
            EXECUTE format(
                'CREATE INDEX %I ON public.%I (event_hour, tx_hash) WHERE tx_hash IS NOT NULL',
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
