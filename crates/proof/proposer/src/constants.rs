//! Constants used throughout the proposer.

use std::time::Duration;

/// Maximum time to wait for a proposal to be included on-chain.
pub const PROPOSAL_TIMEOUT: Duration = Duration::from_mins(10);

/// Timeout for prover server RPC calls.
pub const PROVER_TIMEOUT: Duration = Duration::from_mins(30);

/// Sentinel value for the parent game index when creating the first game from
/// the anchor state registry (i.e., no parent game exists).
/// This is `uint32.max` per the `DisputeGameFactory` contract.
pub const NO_PARENT_INDEX: u32 = 0xFFFF_FFFF;
