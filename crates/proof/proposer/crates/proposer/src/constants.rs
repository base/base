//! Constants used throughout the proposer.

use std::time::Duration;

/// Number of proofs to aggregate in a single batch.
pub const AGGREGATE_BATCH_SIZE: usize = 512;

/// Number of blocks the EVM can look back for blockhashes via EIP-2935.
///
/// EIP-2935 (introduced in Pectra) stores the last 8191 block hashes in a system
/// contract, significantly extending the verification window from the BLOCKHASH
/// opcode's 256 blocks.
pub const BLOCKHASH_WINDOW: u64 = 8191;

/// Safety margin to ensure the L1 origin blockhash is still accessible when the
/// proof is verified on-chain. With ~12 second blocks, 100 blocks is about
/// 20 minutes of buffer.
pub const BLOCKHASH_SAFETY_MARGIN: u64 = 100;

/// Maximum time to wait for a proposal to be included on-chain.
pub const PROPOSAL_TIMEOUT: Duration = Duration::from_secs(600);

/// Length of an ECDSA signature in bytes.
pub const ECDSA_SIGNATURE_LENGTH: usize = 65;

/// Offset to add to ECDSA v-value (0/1 -> 27/28).
pub const ECDSA_V_OFFSET: u8 = 27;

/// Default poll interval for checking new blocks.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(12);

/// Default LRU cache size for RPC responses.
pub const DEFAULT_CACHE_SIZE: usize = 1000;

/// Sentinel value for the parent game index when creating the first game from
/// the anchor state registry (i.e., no parent game exists).
/// This is `uint32.max` per the `DisputeGameFactory` contract.
pub const NO_PARENT_INDEX: u32 = 0xFFFF_FFFF;

/// Proof type byte for TEE proofs (matches `AggregateVerifier.ProofType.TEE`).
pub const PROOF_TYPE_TEE: u8 = 0;

/// Maximum number of games to scan backwards when recovering parent game state
/// on startup.
///
/// IMPORTANT: This value MUST always be greater than the maximum number of pending
/// (unresolved) dispute games that could exist at any given time. Since games take
/// 1-7 days to resolve depending on proof type (7 days TEE-only, 1 day TEE+ZK),
/// a significant backlog can build up during normal operation.
///
/// The default of 5000 is suitable for development and testnet environments.
/// For production deployments with high game volume, this should be increased
/// further to ensure the proposer can always find and resume from its most recent
/// game after a restart.
pub const MAX_GAME_RECOVERY_LOOKBACK: u64 = 5000;

// ============================================================================
// Retry Configuration Constants
// ============================================================================

/// Default maximum number of retry attempts for RPC operations.
pub const DEFAULT_RPC_MAX_RETRIES: u32 = 5;

/// Default initial delay for exponential backoff.
pub const DEFAULT_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(100);

/// Default maximum delay between retry attempts.
pub const DEFAULT_RETRY_MAX_DELAY: Duration = Duration::from_secs(10);

// ============================================================================
// Gas Estimation Constants
// ============================================================================

/// Gas limit multiplier numerator (120% = 6/5).
pub const GAS_LIMIT_MULTIPLIER_NUMERATOR: u64 = 6;

/// Gas limit multiplier denominator (120% = 6/5).
pub const GAS_LIMIT_MULTIPLIER_DENOMINATOR: u64 = 5;
