#![doc = include_str!("../README.md")]

mod p2p;
#[cfg(test)]
pub use p2p::TEST_NODE_ID;
pub use p2p::{
    ClReachabilityRequest, ClReachabilityResponse, DEFAULT_P2P_REACHABILITY_MAX_CONCURRENT_PROBES,
    ElReachabilityRequest, ElReachabilityResponse, P2P_REACHABILITY_CL_PATH,
    P2P_REACHABILITY_EL_PATH, P2P_REACHABILITY_MAX_REQUEST_BYTES, P2pApiError, P2pErrorResponse,
    P2pRoutes, P2pState,
};

mod el_prober;
#[cfg(test)]
pub use el_prober::MockReachabilityProber;
pub use el_prober::{
    RLPX_PROBE_TIMEOUT, ReachabilityProber, RlpxProbeError, RlpxProbeOutcome, RlpxProbeResult,
    RlpxProbeStage, RlpxProbeTarget, RlpxProber,
};

mod cl_prober;
#[cfg(test)]
pub use cl_prober::MockClReachabilityProber;
pub use cl_prober::{
    ClReachabilityProber, LIBP2P_PROBE_TIMEOUT, Libp2pProbeError, Libp2pProbeOutcome,
    Libp2pProbeResult, Libp2pProbeStage, Libp2pProbeTarget, Libp2pProber,
};

mod ingest;
pub use ingest::{
    DEFAULT_NODE_REPORT_REQUESTS_PER_HOUR, IngestApiError, IngestErrorResponse, IngestRoutes,
    IngestState, NODE_REPORT_MAX_REQUEST_BYTES, NODE_REPORT_PATH,
};

mod recorder;
pub use recorder::{DEFAULT_RECORDER_QUEUE_CAPACITY, JsonlRecorder, ReportRecorder};

mod rate_limit;
pub use rate_limit::{
    CLIENT_IP_HEADER, DEFAULT_P2P_PROBE_REQUESTS_PER_MINUTE, IpRateLimiter, PerIpRateLimit,
    RATE_LIMIT_EVICTION_INTERVAL, RATE_LIMITED_BODY, RateLimitExceeded,
};

mod server;
pub use server::{BaseTelemetryServer, ServerConfig};

#[cfg(test)]
mod test_utils;
#[cfg(test)]
pub use test_utils::BlockingProber;
