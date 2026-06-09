//! Metrics for bundle metering.

base_metrics::define_metrics! {
    reth_metering
    #[describe("Number of storage slots modified")]
    storage_slots_modified: histogram,
    #[describe("Number of accounts modified")]
    accounts_modified: histogram,
}
