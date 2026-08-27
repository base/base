//! Backpressure and throughput metrics for the shadow indexer writer and `ExEx`.

base_metrics::define_metrics! {
    shadow_indexer.writer, struct = ShadowWriterMetrics,
    #[describe("Rows currently queued in the writer channel awaiting processing")]
    channel_depth: gauge,
    #[describe("Rows currently buffered in the writer awaiting the next flush")]
    buffer_size: gauge,
    #[describe("Duration in seconds of a flush to the shadow indexer database, including retries")]
    flush_duration_seconds: histogram,
    #[describe("Total rows successfully inserted into the shadow indexer database")]
    rows_inserted: counter,
    #[describe("Total rows dropped after exhausting flush retries")]
    rows_dropped: counter,
    #[describe("Total flush attempts that returned an error")]
    flush_failures: counter,
    #[describe("Total flushes performed, labeled by trigger reason")]
    #[label(trigger)]
    flushes: counter,
}

base_metrics::define_metrics! {
    shadow_indexer.exex, struct = ShadowExExMetrics,
    #[describe("Duration in seconds to handle one ExEx notification, labeled by kind")]
    #[label(kind)]
    notification_duration_seconds: histogram,
    #[describe("Duration in seconds to build one shadow block row (block clone + receipts copy)")]
    build_row_duration_seconds: histogram,
    #[describe("Duration in seconds spent awaiting the writer channel send (backpressure wait)")]
    send_blocked_seconds: histogram,
}
