-- Summarizes the BRIN indexes of one v2 day partition.
--
-- A BRIN index only covers a block range once that range is summarized, and
-- without autosummarize that happens only when VACUUM processes the table.
-- Day partitions only take inserts, so autovacuum reaches them rarely and a
-- busy day partition can go hours with most of its blocks unsummarized. A
-- bitmap scan must read every unsummarized block, so each event_seq slice of
-- a warehouse extract reads the whole unsummarized tail instead of its own
-- range. Once a range is summarized, later inserts into it keep its summary
-- current, so summarizing every minute leaves at most a minute of new blocks
-- uncovered.
--
-- autosummarize is not used: it queues one request per filled range into a
-- fixed-size autovacuum work list, which drops requests at production insert
-- rates.
--
-- brin_summarize_new_values requires ownership of the index, which the
-- runtime role lacks, so this follows the SECURITY DEFINER contract of the
-- partition functions in 003. It takes a SHARE UPDATE EXCLUSIVE lock on the
-- partition, which conflicts with VACUUM on that partition but not with
-- inserts or reads.
CREATE FUNCTION transaction_events_v2_summarize_brin(p_class TEXT, p_day DATE)
RETURNS BIGINT
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = public, pg_temp
AS $$
DECLARE
    partition_oid REGCLASS;
    index_oid OID;
    summarized BIGINT := 0;
BEGIN
    IF p_class IS NULL OR p_class NOT IN ('hot', 'warm', 'cold') THEN
        RAISE EXCEPTION 'unknown transaction event retention class: %', p_class;
    END IF;
    IF p_day IS NULL THEN
        RAISE EXCEPTION 'transaction event partition day is required';
    END IF;

    partition_oid := to_regclass(format(
        'public.%I',
        'transaction_events_v2_' || p_class || '_' || to_char(p_day, 'YYYYMMDD')
    ));
    IF partition_oid IS NULL THEN
        RETURN 0;
    END IF;

    FOR index_oid IN
        SELECT i.indexrelid
        FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am a ON a.oid = c.relam
        WHERE i.indrelid = partition_oid
          AND a.amname = 'brin'
        ORDER BY i.indexrelid
    LOOP
        summarized := summarized + brin_summarize_new_values(index_oid::regclass);
    END LOOP;
    RETURN summarized;
END;
$$;

REVOKE ALL ON FUNCTION transaction_events_v2_summarize_brin(TEXT, DATE) FROM PUBLIC;

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'audit_archiver') THEN
        GRANT EXECUTE ON FUNCTION transaction_events_v2_summarize_brin(TEXT, DATE)
            TO audit_archiver;
    END IF;
END $$;
