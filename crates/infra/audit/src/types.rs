use alloy_consensus::transaction::{SignerRecoverable, Transaction as ConsensusTransaction};
use alloy_primitives::{Address, B256, TxHash, U256};
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

pub type UserOpHash = B256;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UserOpDropReason {
    Invalid(String),
    Expired,
    ReplacedByHigherFee,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data")]
pub enum UserOpEvent {
    AddedToMempool {
        user_op_hash: UserOpHash,
        sender: Address,
        entry_point: Address,
        nonce: U256,
    },
    Dropped {
        user_op_hash: UserOpHash,
        reason: UserOpDropReason,
    },
    Included {
        user_op_hash: UserOpHash,
        block_number: u64,
        tx_hash: TxHash,
    },
}

impl UserOpEvent {
    pub fn user_op_hash(&self) -> UserOpHash {
        match self {
            UserOpEvent::AddedToMempool { user_op_hash, .. } => *user_op_hash,
            UserOpEvent::Dropped { user_op_hash, .. } => *user_op_hash,
            UserOpEvent::Included { user_op_hash, .. } => *user_op_hash,
        }
    }

    pub fn generate_event_key(&self) -> String {
        match self {
            UserOpEvent::Included {
                user_op_hash,
                tx_hash,
                ..
            } => {
                format!("{user_op_hash}-{tx_hash}")
            }
            _ => {
                format!("{}-{}", self.user_op_hash(), Uuid::new_v4())
            }
        }
    }
}

#[cfg(test)]
mod user_op_event_tests {
    use super::*;
    use alloy_primitives::{address, b256};

    fn create_test_user_op_hash() -> UserOpHash {
        b256!("1111111111111111111111111111111111111111111111111111111111111111")
    }

    #[test]
    fn test_user_op_event_added_to_mempool_serialization() {
        let event = UserOpEvent::AddedToMempool {
            user_op_hash: create_test_user_op_hash(),
            sender: address!("2222222222222222222222222222222222222222"),
            entry_point: address!("0000000071727De22E5E9d8BAf0edAc6f37da032"),
            nonce: U256::from(1),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"AddedToMempool\""));

        let deserialized: UserOpEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event.user_op_hash(), deserialized.user_op_hash());
    }

    #[test]
    fn test_user_op_event_dropped_serialization() {
        let event = UserOpEvent::Dropped {
            user_op_hash: create_test_user_op_hash(),
            reason: UserOpDropReason::Invalid("gas too low".to_string()),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"Dropped\""));
        assert!(json.contains("gas too low"));

        let deserialized: UserOpEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event.user_op_hash(), deserialized.user_op_hash());
    }

    #[test]
    fn test_user_op_event_included_serialization() {
        let event = UserOpEvent::Included {
            user_op_hash: create_test_user_op_hash(),
            block_number: 12345,
            tx_hash: b256!("3333333333333333333333333333333333333333333333333333333333333333"),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"Included\""));
        assert!(json.contains("\"block_number\":12345"));

        let deserialized: UserOpEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event.user_op_hash(), deserialized.user_op_hash());
    }

    #[test]
    fn test_user_op_hash_accessor() {
        let hash = create_test_user_op_hash();

        let added = UserOpEvent::AddedToMempool {
            user_op_hash: hash,
            sender: address!("2222222222222222222222222222222222222222"),
            entry_point: address!("0000000071727De22E5E9d8BAf0edAc6f37da032"),
            nonce: U256::from(1),
        };
        assert_eq!(added.user_op_hash(), hash);

        let dropped = UserOpEvent::Dropped {
            user_op_hash: hash,
            reason: UserOpDropReason::Expired,
        };
        assert_eq!(dropped.user_op_hash(), hash);

        let included = UserOpEvent::Included {
            user_op_hash: hash,
            block_number: 100,
            tx_hash: b256!("4444444444444444444444444444444444444444444444444444444444444444"),
        };
        assert_eq!(included.user_op_hash(), hash);
    }

    #[test]
    fn test_generate_event_key_included() {
        let user_op_hash =
            b256!("1111111111111111111111111111111111111111111111111111111111111111");
        let tx_hash = b256!("2222222222222222222222222222222222222222222222222222222222222222");

        let event = UserOpEvent::Included {
            user_op_hash,
            block_number: 100,
            tx_hash,
        };

        let key = event.generate_event_key();
        assert!(key.contains(&format!("{user_op_hash}")));
        assert!(key.contains(&format!("{tx_hash}")));
    }

    #[test]
    fn test_user_op_drop_reason_variants() {
        let invalid = UserOpDropReason::Invalid("test reason".to_string());
        let json = serde_json::to_string(&invalid).unwrap();
        assert!(json.contains("Invalid"));
        assert!(json.contains("test reason"));

        let expired = UserOpDropReason::Expired;
        let json = serde_json::to_string(&expired).unwrap();
        assert!(json.contains("Expired"));

        let replaced = UserOpDropReason::ReplacedByHigherFee;
        let json = serde_json::to_string(&replaced).unwrap();
        assert!(json.contains("ReplacedByHigherFee"));
    }
}
