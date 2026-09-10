use std::time::Duration;

use base_execution_state_types::{EtlConfig, ExecutionStageThresholds};

/// Configuration for each stage in the pipeline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct StageConfig {
    /// Header stage configuration.
    pub headers: HeadersConfig,
    /// Body stage configuration.
    pub bodies: BodiesConfig,
    /// Sender Recovery stage configuration.
    pub sender_recovery: SenderRecoveryConfig,
    /// Execution stage configuration.
    pub execution: ExecutionConfig,
    /// Prune stage configuration.
    pub prune: PruneStageConfig,
    /// Account Hashing stage configuration.
    pub account_hashing: HashingConfig,
    /// Storage Hashing stage configuration.
    pub storage_hashing: HashingConfig,
    /// Merkle stage configuration.
    pub merkle: MerkleConfig,
    /// Transaction Lookup stage configuration.
    pub transaction_lookup: TransactionLookupConfig,
    /// Index Account History stage configuration.
    pub index_account_history: IndexHistoryConfig,
    /// Index Storage History stage configuration.
    pub index_storage_history: IndexHistoryConfig,
    /// Common ETL related configuration.
    pub etl: EtlConfig,
}

impl StageConfig {
    /// The highest threshold (in number of blocks) for switching between incremental and full
    /// calculations across `MerkleStage`, `AccountHashingStage` and `StorageHashingStage`. This is
    /// required to figure out if can prune or not changesets on subsequent pipeline runs during
    /// `ExecutionStage`
    pub fn execution_external_clean_threshold(&self) -> u64 {
        self.merkle
            .incremental_threshold
            .max(self.account_hashing.clean_threshold)
            .max(self.storage_hashing.clean_threshold)
    }
}

/// Header stage configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct HeadersConfig {
    /// The maximum number of requests to send concurrently.
    ///
    /// Default: 100
    pub downloader_max_concurrent_requests: usize,
    /// The minimum number of requests to send concurrently.
    ///
    /// Default: 5
    pub downloader_min_concurrent_requests: usize,
    /// Maximum amount of responses to buffer internally.
    /// The response contains multiple headers.
    pub downloader_max_buffered_responses: usize,
    /// The maximum number of headers to request from a peer at a time.
    pub downloader_request_limit: u64,
    /// The maximum number of headers to download before committing progress to the database.
    pub commit_threshold: u64,
}

impl Default for HeadersConfig {
    fn default() -> Self {
        Self {
            commit_threshold: 10_000,
            downloader_request_limit: 1_000,
            downloader_max_concurrent_requests: 100,
            downloader_min_concurrent_requests: 5,
            downloader_max_buffered_responses: 100,
        }
    }
}

/// Body stage configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct BodiesConfig {
    /// The batch size of non-empty blocks per one request
    ///
    /// Default: 200
    pub downloader_request_limit: u64,
    /// The maximum number of block bodies returned at once from the stream
    ///
    /// Default: `1_000`
    pub downloader_stream_batch_size: usize,
    /// The size of the internal block buffer in bytes.
    ///
    /// Default: 2GB
    pub downloader_max_buffered_blocks_size_bytes: usize,
    /// The minimum number of requests to send concurrently.
    ///
    /// Default: 5
    pub downloader_min_concurrent_requests: usize,
    /// The maximum number of requests to send concurrently.
    /// This is equal to the max number of peers.
    ///
    /// Default: 100
    pub downloader_max_concurrent_requests: usize,
}

impl Default for BodiesConfig {
    fn default() -> Self {
        Self {
            downloader_request_limit: 200,
            downloader_stream_batch_size: 1_000,
            downloader_max_buffered_blocks_size_bytes: 2 * 1024 * 1024 * 1024, // ~2GB
            downloader_min_concurrent_requests: 5,
            downloader_max_concurrent_requests: 100,
        }
    }
}

/// Sender recovery stage configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct SenderRecoveryConfig {
    /// The maximum number of transactions to process before committing progress to the database.
    pub commit_threshold: u64,
}

impl Default for SenderRecoveryConfig {
    fn default() -> Self {
        Self { commit_threshold: 5_000_000 }
    }
}

/// Execution stage configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct ExecutionConfig {
    /// The maximum number of blocks to process before the execution stage commits.
    pub max_blocks: Option<u64>,
    /// The maximum number of state changes to keep in memory before the execution stage commits.
    pub max_changes: Option<u64>,
    /// The maximum cumulative amount of gas to process before the execution stage commits.
    pub max_cumulative_gas: Option<u64>,
    /// The maximum time spent on blocks processing before the execution stage commits.
    #[cfg_attr(
        feature = "serde",
        serde(
            serialize_with = "humantime_serde::serialize",
            deserialize_with = "ExecutionConfig::deserialize_duration"
        )
    )]
    pub max_duration: Option<Duration>,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            max_blocks: Some(500_000),
            max_changes: Some(5_000_000),
            // 50k full blocks of 30M gas
            max_cumulative_gas: Some(30_000_000 * 50_000),
            // 10 minutes
            max_duration: Some(Duration::from_secs(10 * 60)),
        }
    }
}

impl From<ExecutionConfig> for ExecutionStageThresholds {
    fn from(config: ExecutionConfig) -> Self {
        Self {
            max_blocks: config.max_blocks,
            max_changes: config.max_changes,
            max_cumulative_gas: config.max_cumulative_gas,
            max_duration: config.max_duration,
        }
    }
}

/// Prune stage configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct PruneStageConfig {
    /// The maximum number of entries to prune before committing progress to the database.
    pub commit_threshold: usize,
}

impl Default for PruneStageConfig {
    fn default() -> Self {
        Self { commit_threshold: 1_000_000 }
    }
}

/// Hashing stage configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct HashingConfig {
    /// The threshold (in number of blocks) for switching between
    /// incremental hashing and full hashing.
    pub clean_threshold: u64,
    /// The maximum number of entities to process before committing progress to the database.
    pub commit_threshold: u64,
    /// The maximum number of changeset entries to process before committing progress. The stage
    /// commits after either `commit_threshold` blocks or `commit_entries` entries, whichever
    /// comes first. This bounds memory usage when blocks contain many state changes.
    pub commit_entries: u64,
}

impl Default for HashingConfig {
    fn default() -> Self {
        Self { clean_threshold: 500_000, commit_threshold: 100_000, commit_entries: 30_000_000 }
    }
}

/// Merkle stage configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct MerkleConfig {
    /// The number of blocks we will run the incremental root method for when we are catching up on
    /// the merkle stage for a large number of blocks.
    ///
    /// When we are catching up for a large number of blocks, we can only run the incremental root
    /// for a limited number of blocks, otherwise the incremental root method may cause the node to
    /// OOM. This number determines how many blocks in a row we will run the incremental root
    /// method for.
    pub incremental_threshold: u64,
    /// The threshold (in number of blocks) for switching from incremental trie building of changes
    /// to whole rebuild.
    pub rebuild_threshold: u64,
}

impl Default for MerkleConfig {
    fn default() -> Self {
        Self { incremental_threshold: 7_000, rebuild_threshold: 100_000 }
    }
}

/// Transaction Lookup stage configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct TransactionLookupConfig {
    /// The maximum number of transactions to process before writing to disk.
    pub chunk_size: u64,
}

impl Default for TransactionLookupConfig {
    fn default() -> Self {
        Self { chunk_size: 5_000_000 }
    }
}

/// History stage configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct IndexHistoryConfig {
    /// The maximum number of blocks to process before committing progress to the database.
    pub commit_threshold: u64,
}

impl Default for IndexHistoryConfig {
    fn default() -> Self {
        Self { commit_threshold: 100_000 }
    }
}

/// Serialized execution duration supporting human-readable and legacy duration values.
#[cfg(feature = "serde")]
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
pub enum ExecutionDuration {
    /// Human-readable duration.
    Human(#[serde(deserialize_with = "humantime_serde::deserialize")] Option<Duration>),
    /// Legacy seconds and nanoseconds duration.
    Duration(Option<Duration>),
}

#[cfg(feature = "serde")]
impl ExecutionConfig {
    /// Deserialize a human-readable or legacy execution duration.
    pub fn deserialize_duration<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        <ExecutionDuration as serde::Deserialize>::deserialize(deserializer).map(|duration| {
            match duration {
                ExecutionDuration::Human(duration) | ExecutionDuration::Duration(duration) => {
                    duration
                }
            }
        })
    }
}
