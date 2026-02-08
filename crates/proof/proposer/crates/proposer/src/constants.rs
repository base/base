//! Constants used throughout the proposer.

use std::time::Duration;

/// Number of proofs to aggregate in a single batch.
pub const AGGREGATE_BATCH_SIZE: u64 = 512;

/// Number of blocks the EVM can look back for blockhashes.
pub const BLOCKHASH_WINDOW: u64 = 256;

/// Safety margin to ensure blockhash is still accessible when proof is verified.
pub const BLOCKHASH_SAFETY_MARGIN: u64 = 10;

/// Maximum time to wait for a proposal to be included on-chain.
pub const PROPOSAL_TIMEOUT: Duration = Duration::from_secs(600);

/// Length of an ECDSA signature in bytes.
pub const ECDSA_SIGNATURE_LENGTH: usize = 65;

/// Offset to add to ECDSA v-value (0/1 -> 27/28).
pub const ECDSA_V_OFFSET: u8 = 27;

/// Default poll interval for checking new blocks.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(12);

/// Default minimum proposal interval in blocks.
pub const DEFAULT_MIN_PROPOSAL_INTERVAL: u64 = 512;

/// Default LRU cache size for RPC responses.
pub const DEFAULT_CACHE_SIZE: usize = 1000;
