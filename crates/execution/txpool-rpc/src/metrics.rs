//! Sequencer ingress metrics.

base_metrics::define_metrics! {
    base_builder,
    struct = ValidityMetrics,
    #[describe("Shadow validity injection decisions")]
    #[label(outcome)]
    shadow_validity_injection_total: counter,
}
