//! Metrics for bundle metering.

base_metrics::define_metrics! {
    metering
    #[describe("Duration of meter_bundle in seconds")]
    meter_bundle_duration: histogram,
}
