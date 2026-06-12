//! Constants used throughout the proposer.

use std::time::Duration;

/// Default maximum time for the transaction manager to wait for a proposal
/// transaction to be included on-chain.
pub const PROPOSAL_TIMEOUT: Duration = Duration::from_mins(10);

const SUBMIT_TIMEOUT_SLACK_SECS: u64 = 2 * 60;

/// Extra time allowed around the transaction manager's send timeout for JIT
/// validation RPCs before the pipeline restarts the submit session.
pub const SUBMIT_TIMEOUT_SLACK: Duration = Duration::from_secs(SUBMIT_TIMEOUT_SLACK_SECS);

/// Default maximum time for a single inline submit attempt
/// (validation + L1 transaction). Allows [`PROPOSAL_TIMEOUT`] for the
/// transaction itself plus a 2-minute slack for JIT validation RPCs.
pub const SUBMIT_TIMEOUT: Duration =
    Duration::from_secs(PROPOSAL_TIMEOUT.as_secs() + SUBMIT_TIMEOUT_SLACK_SECS);

/// Default maximum number of concurrent RPC calls during the recovery scan.
pub const RECOVERY_SCAN_CONCURRENCY: usize = 8;

/// Maximum retries for a single proof range before a full pipeline reset.
pub const MAX_PROOF_RETRIES: u32 = 8;
