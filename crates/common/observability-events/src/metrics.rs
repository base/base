//! Metrics emitted by the transaction event journal writer.

base_metrics::define_metrics! {
    transaction_events
    #[describe("Transaction event journal entries accepted by the writer")]
    emitted_events: counter,
    #[describe("Transaction event journal entries dropped before enqueue")]
    #[label(name = "reason", default = ["disabled", "backpressure", "closed", "serialization", "validation"])]
    dropped_events: counter,
    #[describe("Transaction event journal write or flush errors")]
    #[label(name = "operation", default = ["write", "flush"])]
    write_errors: counter,
    #[describe("Transaction event journal queue depth")]
    queue_depth: gauge,
    #[describe("Transaction event journal bytes written")]
    bytes_written: counter,
}
