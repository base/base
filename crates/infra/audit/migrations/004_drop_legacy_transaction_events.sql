-- Drops the legacy transaction_events tree created by 001 and 002.
--
-- audit-archiver has read, written, and maintained only transaction_events_v2
-- since the release before this migration. Apply this migration only after
-- every pod runs that release: earlier releases fail readiness without the
-- legacy tree.
--
-- Nothing touches the legacy tables anymore, so the drop's ACCESS EXCLUSIVE
-- locks should be granted at once. The timeout makes the migration fail
-- instead of queueing behind a forgotten session; rerun it once that session
-- ends.
SET LOCAL lock_timeout = '60s';

-- Dropping the parent drops its class partitions, attached day partitions,
-- indexes, and grants.
DROP TABLE IF EXISTS transaction_events;

-- A maintenance pass that failed between detach and drop leaves a legacy
-- day table detached. The pattern does not match v2 tables, whose names
-- continue with v2_ after the shared prefix.
DO $$
DECLARE
    table_name TEXT;
BEGIN
    FOR table_name IN
        SELECT c.relname
        FROM pg_class c
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = 'public'
          AND c.relkind = 'r'
          AND c.relname ~ '^transaction_events_(hot|warm|cold)_[0-9]{8}$'
    LOOP
        EXECUTE format('DROP TABLE public.%I', table_name);
    END LOOP;
END $$;

DROP FUNCTION IF EXISTS transaction_events_create_partition(TEXT, DATE);
DROP FUNCTION IF EXISTS transaction_events_detach_partition(TEXT, DATE);
DROP FUNCTION IF EXISTS transaction_events_drop_detached_partition(TEXT, DATE);
