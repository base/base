//! Authoritative terminal events for admitted candidate groups.
//!
//! The ledger is the only emitter. A retained group gets exactly one terminal event, when its
//! state becomes final: `TXPOOL_CANDIDATE_LANDED` on canonical inclusion, or
//! `TXPOOL_CANDIDATE_EVICTED` with a reason. Builder decision events are per-payload
//! observations and never terminal.

use alloy_primitives::{B256, TxHash};
use base_observability_events::{
    GlobalTransactionEventWriter, TransactionEventProducer, TransactionEventType, transaction_event,
};

use crate::EvictionReason;

/// How a group's lifecycle ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalOutcome {
    /// A sibling was mined in a canonical block.
    Landed {
        /// The mined sibling.
        winner: TxHash,
        /// Canonical block number.
        block: u64,
        /// Canonical block hash.
        block_hash: B256,
    },
    /// Replaced by another group of the same sender and nonce.
    Replaced,
    /// Left the pool without landing. `Dropped` is final only once the recovery window closes,
    /// because a canonical landing can still correct it.
    Evicted(EvictionReason),
}

impl TerminalOutcome {
    /// Returns the stable `terminal_reason` label.
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::Landed { .. } => "landed",
            Self::Replaced => "replaced",
            Self::Evicted(reason) => reason.as_label(),
        }
    }
}

/// One group's final state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupTerminal {
    /// Group identity.
    pub group_hash: B256,
    /// The representative's hash, the group's pool identity.
    pub representative: TxHash,
    /// How the lifecycle ended.
    pub outcome: TerminalOutcome,
}

impl GroupTerminal {
    /// Journals the transition. The event ID depends only on the group, so a replayed
    /// delivery deduplicates.
    pub fn journal(&self, instance: u64) {
        if GlobalTransactionEventWriter::get().is_none() {
            return;
        }
        let group_hash = format!("{:#x}", self.group_hash);
        let representative = format!("{:#x}", self.representative);
        let instance = format!("{instance:016x}");
        let _ = match self.outcome {
            TerminalOutcome::Landed { winner, block, block_hash } => transaction_event!(
                producer: TransactionEventProducer::BaseRethNode,
                event_type: TransactionEventType::TxpoolCandidateLanded,
                tx_hash: winner,
                block_hash: block_hash,
                block_number: block,
                id: { "group_hash" => &group_hash },
                data: {
                    "group_hash" => group_hash,
                    "representative_hash" => representative,
                    "candidate_instance" => instance,
                    "terminal_reason" => self.outcome.as_label(),
                },
            ),
            _ => transaction_event!(
                producer: TransactionEventProducer::BaseRethNode,
                event_type: TransactionEventType::TxpoolCandidateEvicted,
                tx_hash: self.representative,
                id: { "group_hash" => &group_hash },
                data: {
                    "group_hash" => group_hash,
                    "representative_hash" => representative,
                    "candidate_instance" => instance,
                    "terminal_reason" => self.outcome.as_label(),
                },
            ),
        };
    }
}
