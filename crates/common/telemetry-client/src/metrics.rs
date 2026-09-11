//! Metrics for the telemetry client's own delivery path.

base_metrics::define_metrics! {
    base_telemetry_client
    #[describe("Reports handed to the reporter queue")]
    reports_enqueued: counter,
    #[describe("Reports dropped because the reporter queue was full")]
    reports_dropped: counter,
    #[describe("Reports accepted by the ingest endpoint")]
    reports_sent: counter,
    #[describe("Reports abandoned after exhausting retries")]
    reports_failed: counter,
}
