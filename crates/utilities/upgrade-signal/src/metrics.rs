//! Metrics for upgrade signal observers.

base_metrics::define_metrics! {
    base.upgrade_signal, struct = UpgradeSignalMetrics,
    #[describe("Configured activation timestamp read from L1")]
    #[label(hardfork)]
    activation_timestamp: gauge,
    #[describe("Expected protocol version read from L1")]
    #[label(hardfork)]
    expected_protocol_version: gauge,
    #[describe("Last L1 block number used for a successful upgrade signal read")]
    #[label(hardfork)]
    last_l1_read_block: gauge,
    #[describe("Total failed attempts to read the L1 upgrade signal")]
    #[label(hardfork)]
    l1_read_errors_total: counter,
    #[describe("Total failed attempts to read the L2 timestamp")]
    #[label(hardfork)]
    l2_timestamp_errors_total: counter,
    #[describe("Whether the upgrade activation timestamp has been observed locally")]
    #[label(hardfork)]
    activation_observed: gauge,
    #[describe("Total observed upgrade activations")]
    #[label(hardfork)]
    activation_observed_total: counter,
}
