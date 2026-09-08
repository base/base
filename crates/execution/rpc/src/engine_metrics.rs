use metrics::{Counter, Histogram};
use reth_metrics::Metrics;

/// All beacon consensus engine metrics
#[derive(Debug, Default)]
pub struct EngineApiMetrics {
    /// Engine API latency metrics
    pub latency: EngineApiLatencyMetrics,
    /// Blob-related metrics
    pub blob_metrics: BlobMetrics,
}

/// Beacon consensus engine latency metrics.
#[derive(Metrics)]
#[metrics(scope = "engine.rpc")]
pub struct EngineApiLatencyMetrics {
    /// Latency for `engine_newPayloadV1`
    pub new_payload_v1: Histogram,
    /// Latency for `engine_newPayloadV2`
    pub new_payload_v2: Histogram,
    /// Latency for `engine_newPayloadV3`
    pub new_payload_v3: Histogram,
    /// Latency for `engine_newPayloadV4`
    pub new_payload_v4: Histogram,
    /// Latency for `engine_newPayloadV5`
    pub new_payload_v5: Histogram,
    /// Latency for `engine_forkchoiceUpdatedV1`
    pub fork_choice_updated_v1: Histogram,
    /// Latency for `engine_forkchoiceUpdatedV2`
    pub fork_choice_updated_v2: Histogram,
    /// Latency for `engine_forkchoiceUpdatedV3`
    pub fork_choice_updated_v3: Histogram,
    /// Latency for `engine_forkchoiceUpdatedV4`
    pub fork_choice_updated_v4: Histogram,
    /// Latency for `engine_getPayloadV1`
    pub get_payload_v1: Histogram,
    /// Latency for `engine_getPayloadV2`
    pub get_payload_v2: Histogram,
    /// Latency for `engine_getPayloadV3`
    pub get_payload_v3: Histogram,
    /// Latency for `engine_getPayloadV4`
    pub get_payload_v4: Histogram,
    /// Latency for `engine_getPayloadV5`
    pub get_payload_v5: Histogram,
    /// Latency for `engine_getPayloadV6`
    pub get_payload_v6: Histogram,
    /// Latency for `engine_getPayloadBodiesByRangeV1`
    pub get_payload_bodies_by_range_v1: Histogram,
    /// Latency for `engine_getPayloadBodiesByRangeV2`
    pub get_payload_bodies_by_range_v2: Histogram,
    /// Latency for `engine_getPayloadBodiesByHashV1`
    pub get_payload_bodies_by_hash_v1: Histogram,
    /// Latency for `engine_getPayloadBodiesByHashV2`
    pub get_payload_bodies_by_hash_v2: Histogram,
    /// Latency for `engine_getBlobsV1`
    pub get_blobs_v1: Histogram,
    /// Latency for `engine_getBlobsV2`
    pub get_blobs_v2: Histogram,
    /// Latency for `engine_getBlobsV3`
    pub get_blobs_v3: Histogram,
    /// Latency for `engine_getBlobsV4`
    pub get_blobs_v4: Histogram,
    /// Latency for `engine_hasBlobs`
    pub has_blobs: Histogram,
}

#[derive(Metrics)]
#[metrics(scope = "engine.rpc.blobs")]
pub struct BlobMetrics {
    /// Count of blobs successfully retrieved
    pub blob_count: Counter,
    /// Count of blob misses
    pub blob_misses: Counter,
    /// Number of blobs requested via getBlobsV2
    pub get_blobs_requests_blobs_total: Counter,
    /// Number of blobs requested via getBlobsV2 that are present in the blobpool
    pub get_blobs_requests_blobs_in_blobpool_total: Counter,
    /// Number of times getBlobsV2 responded with “hit”
    pub get_blobs_requests_success_total: Counter,
    /// Number of times getBlobsV2 responded with “miss”
    pub get_blobs_requests_failure_total: Counter,
}
