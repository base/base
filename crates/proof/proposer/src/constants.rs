//! Constants used throughout the proposer.

use std::time::Duration;

/// Maximum time to wait for a proposal to be included on-chain.
pub const PROPOSAL_TIMEOUT: Duration = Duration::from_mins(10);

/// Timeout for prover server RPC calls.
pub const PROVER_TIMEOUT: Duration = Duration::from_mins(30);

/// Maximum number of concurrent `game_at_index` RPC calls during the recovery
/// scan.
pub const RECOVERY_SCAN_CONCURRENCY: usize = 32;

/// Maximum number of factory entries to scan (from the most recent) on cold
/// start or when the incremental delta exceeds this threshold. This bounds
/// the factory scan phase only — the forward walk is unbounded and terminates
/// naturally at the first gap or chain break.
pub const MAX_FACTORY_SCAN_LOOKBACK: u64 = 5000;
