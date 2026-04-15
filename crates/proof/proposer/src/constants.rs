//! Constants used throughout the proposer.

use std::time::Duration;

/// Maximum time to wait for a proposal to be included on-chain.
pub const PROPOSAL_TIMEOUT: Duration = Duration::from_mins(10);

/// Timeout for prover server RPC calls.
pub const PROVER_TIMEOUT: Duration = Duration::from_mins(30);

/// Default maximum number of concurrent RPC calls during the recovery scan.
pub const RECOVERY_SCAN_CONCURRENCY: usize = 8;

/// Maximum retries for a single proof range before a full pipeline reset.
pub const MAX_PROOF_RETRIES: u32 = 3;
