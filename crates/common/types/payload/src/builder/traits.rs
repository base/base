//! Core traits for working with execution payloads.

use alloc::{boxed::Box, sync::Arc, vec::Vec};

use alloy_primitives::B256;
use alloy_rlp::Encodable;
use base_common_types_chain::{RecoveredBlock, SealedHeader};
use base_execution_state_types::{BlockExecutionOutput, HashedPostState, updates::TrieUpdates};
use either::Either;

use crate::PayloadId;

/// Represents an executed block for payload building purposes.
///
/// This type captures the complete execution state of a built block,
/// including the recovered block, execution outcome, hashed state, and trie updates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuiltPayloadExecutedBlock {
    /// Recovered Block
    pub recovered_block: Arc<RecoveredBlock>,
    /// Block's execution outcome.
    pub execution_output: Arc<BlockExecutionOutput>,
    /// Block's hashed state (unsorted).
    pub hashed_state: Arc<HashedPostState>,
    /// Trie updates that result from calculating the state root for the block (unsorted).
    pub trie_updates: Arc<TrieUpdates>,
}

/// Factory trait for creating payload attributes.
///
/// Enables different strategies for generating payload attributes based on
/// contextual information. Useful for testing and specialized building.
pub trait PayloadAttributesBuilder<Attributes, Header = base_common_types_chain::Header>:
    Send + Sync + 'static
{
    /// Constructs new payload attributes for the given timestamp.
    fn build(&self, parent: &SealedHeader) -> Attributes;
}

impl<Attributes, Header, F> PayloadAttributesBuilder<Attributes, Header> for F
where
    Header: Clone,
    F: Fn(SealedHeader) -> Attributes + Send + Sync + 'static,
{
    fn build(&self, parent: &SealedHeader) -> Attributes {
        self(parent.clone())
    }
}

impl<Attributes, Header, L, R> PayloadAttributesBuilder<Attributes, Header> for Either<L, R>
where
    L: PayloadAttributesBuilder<Attributes, Header>,
    R: PayloadAttributesBuilder<Attributes, Header>,
{
    fn build(&self, parent: &SealedHeader) -> Attributes {
        match self {
            Self::Left(l) => l.build(parent),
            Self::Right(r) => r.build(parent),
        }
    }
}

impl<Attributes, Header> PayloadAttributesBuilder<Attributes, Header>
    for Box<dyn PayloadAttributesBuilder<Attributes, Header>>
where
    Header: 'static,
    Attributes: 'static,
{
    fn build(&self, parent: &SealedHeader) -> Attributes {
        self.as_ref().build(parent)
    }
}

/// Generates the payload id for the configured payload from the [`crate::PayloadAttributes`].
///
/// Returns an 8-byte identifier by hashing the payload components with sha256 hash.
pub fn payload_id(parent: &B256, attributes: &crate::PayloadAttributes) -> PayloadId {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(parent.as_slice());
    hasher.update(&attributes.timestamp.to_be_bytes()[..]);
    hasher.update(attributes.prev_randao.as_slice());
    hasher.update(attributes.suggested_fee_recipient.as_slice());
    if let Some(withdrawals) = &attributes.withdrawals {
        let mut buf = Vec::new();
        withdrawals.encode(&mut buf);
        hasher.update(buf);
    }

    if let Some(parent_beacon_block) = attributes.parent_beacon_block_root {
        hasher.update(parent_beacon_block);
    }

    if let Some(slot_number) = attributes.slot_number {
        hasher.update(slot_number.to_be_bytes());
    }

    if let Some(target_gas_limit) = attributes.target_gas_limit {
        hasher.update(target_gas_limit.to_be_bytes());
    }

    let out = hasher.finalize();

    #[allow(deprecated)] // generic-array 0.14 deprecated
    PayloadId::new(out.as_slice()[..8].try_into().expect("sufficient length"))
}

#[cfg(test)]
mod tests {
    use core::str::FromStr;

    use alloy_eips::eip4895::Withdrawal;
    use alloy_primitives::{Address, B64};

    use super::*;
    use crate::PayloadAttributes as EthPayloadAttributes;

    #[test]
    fn attributes_serde() {
        let attributes = r#"{"timestamp":"0x1235","prevRandao":"0xf343b00e02dc34ec0124241f74f32191be28fb370bb48060f5fa4df99bda774c","suggestedFeeRecipient":"0x0000000000000000000000000000000000000000","withdrawals":null,"parentBeaconBlockRoot":null}"#;
        let _attributes: EthPayloadAttributes = serde_json::from_str(attributes).unwrap();
    }

    #[test]
    fn test_payload_id_basic() {
        // Create a parent block and payload attributes
        let parent =
            B256::from_str("0x3b8fb240d288781d4aac94d3fd16809ee413bc99294a085798a589dae51ddd4a")
                .unwrap();
        let attributes = EthPayloadAttributes {
            timestamp: 0x5,
            prev_randao: B256::from_str(
                "0x0000000000000000000000000000000000000000000000000000000000000000",
            )
            .unwrap(),
            suggested_fee_recipient: Address::from_str(
                "0xa94f5374fce5edbc8e2a8697c15331677e6ebf0b",
            )
            .unwrap(),
            withdrawals: None,
            parent_beacon_block_root: None,
            slot_number: None,
            target_gas_limit: None,
        };

        // Verify that the generated payload ID matches the expected value
        assert_eq!(
            payload_id(&parent, &attributes),
            PayloadId(B64::from_str("0xa247243752eb10b4").unwrap())
        );
    }

    #[test]
    fn test_payload_id_with_withdrawals() {
        // Set up the parent and attributes with withdrawals
        let parent =
            B256::from_str("0x9876543210abcdef9876543210abcdef9876543210abcdef9876543210abcdef")
                .unwrap();
        let attributes = EthPayloadAttributes {
            timestamp: 1622553200,
            prev_randao: B256::from_slice(&[1; 32]),
            suggested_fee_recipient: Address::from_str(
                "0xb94f5374fce5edbc8e2a8697c15331677e6ebf0b",
            )
            .unwrap(),
            withdrawals: Some(vec![
                Withdrawal {
                    index: 1,
                    validator_index: 123,
                    address: Address::from([0xAA; 20]),
                    amount: 10,
                },
                Withdrawal {
                    index: 2,
                    validator_index: 456,
                    address: Address::from([0xBB; 20]),
                    amount: 20,
                },
            ]),
            parent_beacon_block_root: None,
            slot_number: None,
            target_gas_limit: None,
        };

        // Verify that the generated payload ID matches the expected value
        assert_eq!(
            payload_id(&parent, &attributes),
            PayloadId(B64::from_str("0xedddc2f84ba59865").unwrap())
        );
    }

    #[test]
    fn test_payload_id_with_parent_beacon_block_root() {
        // Set up the parent and attributes with a parent beacon block root
        let parent =
            B256::from_str("0x9876543210abcdef9876543210abcdef9876543210abcdef9876543210abcdef")
                .unwrap();
        let attributes = EthPayloadAttributes {
            timestamp: 1622553200,
            prev_randao: B256::from_str(
                "0x123456789abcdef123456789abcdef123456789abcdef123456789abcdef1234",
            )
            .unwrap(),
            suggested_fee_recipient: Address::from_str(
                "0xc94f5374fce5edbc8e2a8697c15331677e6ebf0b",
            )
            .unwrap(),
            withdrawals: None,
            parent_beacon_block_root: Some(
                B256::from_str(
                    "0x2222222222222222222222222222222222222222222222222222222222222222",
                )
                .unwrap(),
            ),
            slot_number: None,
            target_gas_limit: None,
        };

        // Verify that the generated payload ID matches the expected value
        assert_eq!(
            payload_id(&parent, &attributes),
            PayloadId(B64::from_str("0x0fc49cd532094cce").unwrap())
        );
    }

    #[test]
    fn test_payload_id_with_slot_number() {
        let parent =
            B256::from_str("0x9876543210abcdef9876543210abcdef9876543210abcdef9876543210abcdef")
                .unwrap();
        let mut attributes = EthPayloadAttributes {
            timestamp: 1622553200,
            prev_randao: B256::from_slice(&[1; 32]),
            suggested_fee_recipient: Address::from_str(
                "0xb94f5374fce5edbc8e2a8697c15331677e6ebf0b",
            )
            .unwrap(),
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::from_slice(&[2; 32])),
            slot_number: Some(1),
            target_gas_limit: None,
        };

        let first = payload_id(&parent, &attributes);
        attributes.slot_number = Some(2);

        assert_ne!(first, payload_id(&parent, &attributes));
    }

    #[test]
    fn test_payload_id_with_target_gas_limit() {
        let parent =
            B256::from_str("0x9876543210abcdef9876543210abcdef9876543210abcdef9876543210abcdef")
                .unwrap();
        let mut attributes = EthPayloadAttributes {
            timestamp: 1622553200,
            prev_randao: B256::from_slice(&[1; 32]),
            suggested_fee_recipient: Address::from_str(
                "0xb94f5374fce5edbc8e2a8697c15331677e6ebf0b",
            )
            .unwrap(),
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::from_slice(&[2; 32])),
            slot_number: Some(1),
            target_gas_limit: Some(30_000_000),
        };

        let first = payload_id(&parent, &attributes);
        attributes.target_gas_limit = Some(60_000_000);

        assert_ne!(first, payload_id(&parent, &attributes));
    }
}
