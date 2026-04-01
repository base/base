//! Metrics for the discovery service.

base_metrics::define_metrics! {
    base_node_disc

    #[describe("Events received by the discv5 service")]
    #[label(name = "type", default = ["discovered", "session_established", "unverifiable_enr"])]
    discovery_event: gauge,

    #[describe("Requests made to find a node through the discv5 peer discovery service")]
    find_node_request: gauge,

    #[describe("Observations of elapsed time to store ENRs in the on-disk bootstore")]
    enr_store_time: histogram,

    #[describe("Number of peers connected to the discv5 service")]
    discovery_peer_count: gauge,
}
