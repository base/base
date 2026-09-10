-- Drops `shadow_metrics_cursor`. The shadow-metrics polling reader that owned
-- this cursor has been removed, so nothing reads or writes the table.
DROP TABLE IF EXISTS shadow_metrics_cursor;
