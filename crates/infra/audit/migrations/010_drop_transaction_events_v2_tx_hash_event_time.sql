-- Drops the (tx_hash, event_time) index now that 009's hour-leading index
-- serves lookups by hash. Dropping the partitioned index drops its leaf
-- indexes on every day partition.
--
-- Day partitions that held rows when 009 ran are left with no hash index.
-- Lookups skip those days instead of scanning them, so no event on those days
-- is findable by hash until retention drops their partitions. That includes
-- the rest of the day 009 ran on, since that day's partitions were populated.
--
-- DROP INDEX takes an ACCESS EXCLUSIVE lock on the parents and every day
-- partition. A long reader, such as a DataPilot extraction run, holds a lock
-- that conflicts with it, and inserts queue behind the waiting DROP. The lock
-- timeout bounds that wait: when a reader is active the migration fails, and
-- it applies on a later migrator run once the reader is gone. Pause the
-- extraction DAG between runs before running the migrator.
SET LOCAL lock_timeout = '5s';

DROP INDEX transaction_events_v2_tx_hash_event_time_idx;
