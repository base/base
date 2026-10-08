-- Lets bitmap heap scans prefetch heap pages for every new session in the
-- audit database.
--
-- Warehouse extracts read transaction_events_v2 through BRIN bitmap scans.
-- With the default effective_io_concurrency of 1, each scan waits for one
-- page read at a time, so an extract slice whose rows are spread across a day
-- partition runs at one storage round trip per page while the volume sits
-- mostly idle. A depth of 32 lets a few concurrent slices together keep enough
-- reads in flight to use the provisioned IOPS without each slice
-- monopolizing them.
--
-- The setting is applied to the database rather than to individual roles
-- because the migration role owns the database but cannot alter other roles.
-- It only takes effect for sessions that connect after this migration.
--
-- The migration is skipped with a notice where the migration role does not
-- own the database, or where the platform lacks posix_fadvise and rejects a
-- nonzero value, since prefetching is a performance hint and must not block
-- the schema migrations that follow.
DO $$
BEGIN
    EXECUTE format(
        'ALTER DATABASE %I SET effective_io_concurrency = 32',
        current_database()
    );
EXCEPTION
    WHEN insufficient_privilege OR invalid_parameter_value THEN
        RAISE NOTICE 'effective_io_concurrency not set for database %: %',
            current_database(), SQLERRM;
END $$;
