//! Metrics for the discovery service.

base_metrics::define_metrics! {
    base_node_disc

    #[describe("Events received by the discv5 service")]
    #[label(r#type)]
    discovery_event: gauge,

    #[describe("Requests made to find a node through the discv5 peer discovery service")]
    find_node_request: gauge,

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
    pub fn init() {
        Self::describe();
        Self::zero();
    }

    /// Initializes metrics to `0` so they can be queried immediately by consumers of prometheus
    /// metrics.
    pub fn zero() {
        // Discovery Event
        Self::discovery_event("discovered").set(0);
        Self::discovery_event("session_established").set(0);
        Self::discovery_event("unverifiable_enr").set(0);

        // Peer Counts
        Self::discovery_peer_count().set(0);
        Self::find_node_request().set(0);
    }
}
