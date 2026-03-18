//! Constants used throughout the proposer.

use std::time::Duration;

/// Maximum time to wait for a proposal to be included on-chain.
pub const PROPOSAL_TIMEOUT: Duration = Duration::from_secs(600);

/// Timeout for prover server RPC calls.
pub const PROVER_TIMEOUT: Duration = Duration::from_secs(600);

/// Sentinel value for the parent game index when creating the first game from
/// the anchor state registry (i.e., no parent game exists).
/// This is `uint32.max` per the `DisputeGameFactory` contract.
pub const NO_PARENT_INDEX: u32 = 0xFFFF_FFFF;

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
