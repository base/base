//! Metrics for the discovery service.

base_metrics::define_metrics! {
    base_node
    #[describe("Events received by the discv5 service")]
    #[label(kind)]
    discovery_events: gauge,
    #[describe("Number of FIND_NODE requests made through the discv5 peer discovery service")]
    find_node_requests: gauge,
    #[describe("Observations of elapsed time to store ENRs in the on-disk bootstore")]
    enr_store_time: histogram,
    #[describe("Number of peers connected to the discv5 service")]
    discovery_peer_count: gauge,
}

impl Metrics {
    /// Initializes metrics for the discovery service.
    ///
    /// This does two things:
    /// * Describes various metrics.
    /// * Initializes metrics to 0 so they can be queried immediately.
    #[cfg(feature = "metrics")]
    pub fn init() {
        Self::describe();
        Self::zero();
    }

    /// Initializes metrics to `0` so they can be queried immediately by consumers of prometheus
    /// metrics.
    pub fn zero() {
        Self::discovery_events("discovered").set(0.0);
        Self::discovery_events("session_established").set(0.0);
        Self::discovery_events("unverifiable_enr").set(0.0);
        Self::find_node_requests().set(0.0);
        Self::discovery_peer_count().set(0.0);
    }
}
