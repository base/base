//! Metrics emitted by the transaction event journal writer.

base_metrics::define_metrics! {
    transaction_events
    #[describe("Transaction event journal entries submitted to the writer")]
    submitted_events: counter,
    #[describe("Transaction event journal entries dropped before enqueue")]
    #[label(name = "reason", default = ["disabled", "backpressure", "serialization", "validation"])]
    dropped_events: counter,
    #[describe("Transaction event journal write or flush errors")]
    #[label(name = "operation", default = ["write", "flush"])]
    write_errors: counter,
    #[describe("Transaction event journal bytes written")]
    bytes_written: counter,
}
