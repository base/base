use alloy_consensus::transaction::{SignerRecoverable, Transaction as ConsensusTransaction};
use alloy_primitives::{Address, TxHash, U256};
use alloy_provider::network::eip2718::Decodable2718;
use alloy_rpc_types_mev::EthSendBundle;
use bytes::Bytes;
use op_alloy_consensus::OpTxEnvelope;
use serde::{Deserialize, Serialize};
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    pub id: BundleId,
    pub transactions: Vec<Transaction>,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    pub id: TransactionId,
    pub data: Bytes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data")]
pub enum MempoolEvent {
    Created {
        bundle_id: BundleId,
        bundle: EthSendBundle,
    },
    Updated {
        bundle_id: BundleId,
        bundle: EthSendBundle,
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
    FlashblockIncluded {
        bundle_id: BundleId,
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

impl MempoolEvent {
    pub fn bundle_id(&self) -> BundleId {
        match self {
            MempoolEvent::Created { bundle_id, .. } => *bundle_id,
            MempoolEvent::Updated { bundle_id, .. } => *bundle_id,
            MempoolEvent::Cancelled { bundle_id, .. } => *bundle_id,
            MempoolEvent::BuilderIncluded { bundle_id, .. } => *bundle_id,
            MempoolEvent::FlashblockIncluded { bundle_id, .. } => *bundle_id,
            MempoolEvent::BlockIncluded { bundle_id, .. } => *bundle_id,
            MempoolEvent::Dropped { bundle_id, .. } => *bundle_id,
        }
    }

    pub fn transaction_ids(&self) -> Vec<TransactionId> {
        match self {
            MempoolEvent::Created { bundle, .. } | MempoolEvent::Updated { bundle, .. } => {
                bundle
                    .txs
                    .iter()
                    .filter_map(|tx_bytes| {
                        match OpTxEnvelope::decode_2718_exact(tx_bytes.iter().as_slice()) {
                            Ok(envelope) => {
                                match envelope.recover_signer() {
                                    Ok(sender) => Some(TransactionId {
                                        sender,
                                        nonce: U256::from(envelope.nonce()),
                                        hash: *envelope.hash(),
                                    }),
                                    Err(_) => None, // Skip invalid transactions
                                }
                            }
                            Err(_) => None, // Skip malformed transactions
                        }
                    })
                    .collect()
            }
            MempoolEvent::Cancelled { .. } => vec![],
            MempoolEvent::BuilderIncluded { .. } => vec![],
            MempoolEvent::FlashblockIncluded { .. } => vec![],
            MempoolEvent::BlockIncluded { .. } => vec![],
            MempoolEvent::Dropped { .. } => vec![],
        }
    }
}
