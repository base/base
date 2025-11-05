use alloy_consensus::transaction::{SignerRecoverable, Transaction as ConsensusTransaction};
use alloy_primitives::{Address, TxHash, U256};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tips_core::AcceptedBundle;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TransactionId {
    pub sender: Address,
    pub nonce: U256,
    pub hash: TxHash,
}

pub type BundleId = Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DropReason {
    TimedOut,
    Reverted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    pub id: TransactionId,
    pub data: Bytes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data")]
pub enum BundleEvent {
    Received {
        bundle_id: BundleId,
        bundle: Box<AcceptedBundle>,
    },
    Cancelled {
        bundle_id: BundleId,
    },
    BuilderIncluded {
        bundle_id: BundleId,
        builder: String,
        block_number: u64,
        flashblock_index: u64,
    },
    BlockIncluded {
        bundle_id: BundleId,
        block_number: u64,
        block_hash: TxHash,
    },
    Dropped {
        bundle_id: BundleId,
        reason: DropReason,
    },
}

impl BundleEvent {
    pub fn bundle_id(&self) -> BundleId {
        match self {
            BundleEvent::Received { bundle_id, .. } => *bundle_id,
            BundleEvent::Cancelled { bundle_id, .. } => *bundle_id,
            BundleEvent::BuilderIncluded { bundle_id, .. } => *bundle_id,
            BundleEvent::BlockIncluded { bundle_id, .. } => *bundle_id,
            BundleEvent::Dropped { bundle_id, .. } => *bundle_id,
        }
    }

    pub fn transaction_ids(&self) -> Vec<TransactionId> {
        match self {
            BundleEvent::Received { bundle, .. } => {
                bundle
                    .txs
                    .iter()
                    .filter_map(|envelope| {
                        match envelope.recover_signer() {
                            Ok(sender) => Some(TransactionId {
                                sender,
                                nonce: U256::from(envelope.nonce()),
                                hash: *envelope.hash(),
                            }),
                            Err(_) => None, // Skip invalid transactions
                        }
                    })
                    .collect()
            }
            BundleEvent::Cancelled { .. } => vec![],
            BundleEvent::BuilderIncluded { .. } => vec![],
            BundleEvent::BlockIncluded { .. } => vec![],
            BundleEvent::Dropped { .. } => vec![],
        }
    }

    pub fn generate_event_key(&self) -> String {
        match self {
            BundleEvent::BlockIncluded {
                bundle_id,
                block_hash,
                ..
            } => {
                format!("{bundle_id}-{block_hash}")
            }
            _ => {
                format!("{}-{}", self.bundle_id(), Uuid::new_v4())
            }
        }
    }
}
