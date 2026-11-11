//! Messages received by the run coordinator, the single owner of run bookkeeping.
//!
//! Submission workers, chain watchers, and the receipt sampler never share state with the
//! coordinator. They report through one bounded FIFO inbox, so the coordinator applies every
//! observation in arrival order. A transaction is registered before any block containing it
//! can be matched, because the worker sends `Sending` before it sends the RPC.

use std::{
    borrow::Cow,
    time::{Instant, SystemTime},
};

use alloy_primitives::{Address, TxHash};

use super::{BlockObservation, BlockPulse, BlockReceipt, SentTransaction};

/// Final submission outcome of one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxOutcome {
    /// A submission RPC accepted the transaction in an attempt started at `sent_at`.
    Accepted {
        /// Wall-clock start of the accepting attempt.
        sent_at: SystemTime,
    },
    /// The node already held the transaction or had already consumed its nonce.
    Known,
    /// The node accepted the transaction under a different hash.
    Rehashed {
        /// Hash returned by the submission RPC.
        server: TxHash,
        /// Wall-clock start of the accepting attempt.
        sent_at: SystemTime,
    },
    /// Submission gave up on the transaction.
    Failed {
        /// Failure reason reported in the summary.
        reason: Cow<'static, str>,
        /// Set when the node definitively does not hold the transaction, so it can never land.
        discard: bool,
    },
}

/// Submission outcome of one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxResult {
    /// Locally computed transaction hash.
    pub tx_hash: TxHash,
    /// Sender address.
    pub from: Address,
    /// Calibrated execution gas held in local queued accounting.
    pub estimated_gas: u64,
    /// Whether this transaction belongs to the measured cohort.
    pub measured: bool,
    /// Final outcome.
    pub outcome: TxOutcome,
}

/// Newest canonical block read by the block watcher.
#[derive(Debug, Clone)]
pub struct CanonicalBlock {
    /// Block number, observation time, timestamp, and base fee.
    pub observation: BlockObservation,
    /// Transaction hashes in the block.
    pub tx_hashes: Vec<TxHash>,
    /// Gas consumed by the entire block.
    pub gas_used: u64,
    /// Block gas limit.
    pub gas_limit: u64,
    /// Predicted canonical boundary for this block.
    pub expected_boundary: Instant,
}

/// Message to the run coordinator.
#[derive(Debug)]
pub enum RunEvent {
    /// A sender worker picked up a batch and is about to send it.
    Sending {
        /// Transactions entering submission.
        txs: Vec<SentTransaction>,
        /// Local pickup time, the start of landing latency.
        at: Instant,
        /// Wall-clock pickup time, comparable with block timestamps.
        at_wall: SystemTime,
    },
    /// Transactions that reached a final submission outcome in one attempt.
    Settled(Vec<TxResult>),
    /// Every transaction in a batch has a final outcome.
    BatchCompleted {
        /// Batch identifier.
        id: u64,
        /// Local completion time.
        completed_at: Instant,
    },
    /// Newest canonical block.
    Canonical(CanonicalBlock),
    /// A skipped canonical block recovered after a newer one was published.
    Recovered {
        /// Block number, observation time, timestamp, and base fee.
        observation: BlockObservation,
        /// Transaction hashes in the block.
        tx_hashes: Vec<TxHash>,
    },
    /// Canonical head number from a header subscription.
    Head(u64),
    /// Canonical receipts sampled for gas calibration.
    Receipts(Vec<BlockReceipt>),
}

/// Why the coordinator runs a refill cycle.
#[derive(Debug, Clone, Copy)]
pub enum RefillTrigger {
    /// A new canonical block was observed.
    Canonical(BlockPulse),
    /// The safety timer fired at the given time because no timely block arrived, or steady
    /// submission is configured.
    Safety(Instant),
}

impl RefillTrigger {
    /// Returns when the trigger was observed.
    pub const fn observed_at(&self) -> Instant {
        match self {
            Self::Canonical(block) => block.observed_at,
            Self::Safety(at) => *at,
        }
    }

    /// Returns the canonical block, when this trigger came from one.
    pub const fn block(&self) -> Option<BlockPulse> {
        match self {
            Self::Canonical(block) => Some(*block),
            Self::Safety(_) => None,
        }
    }
}
