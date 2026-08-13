//! Metrics for the Base payload builder.

base_metrics::define_metrics! {
    payload_builder,
    struct = PayloadBuilderMetrics,
    #[describe("Payload build duration in seconds, from build start through state root")]
    build_duration: histogram,
}
