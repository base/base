use alloy_eips::eip7685::EMPTY_REQUESTS_HASH;
use alloy_primitives::{B256, Signature, U256};
use alloy_rpc_types_engine::{CancunPayloadFields, PraguePayloadFields};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_consensus::BaseBlock;
use base_common_rpc_types_engine::{
    BaseExecutionPayload, BaseExecutionPayloadEnvelope, BaseExecutionPayloadSidecar,
    NetworkPayloadEnvelope, PayloadHash,
};

use super::L2SequencerError;

/// Converts between execution payload envelopes and action-harness block/gossip types.
#[derive(Debug)]
pub struct ExecutionPayloadConverter;

impl ExecutionPayloadConverter {
    /// Convert a sealed execution payload envelope into a [`BaseBlock`].
    pub fn block_from_envelope(
        envelope: &BaseExecutionPayloadEnvelope,
    ) -> Result<BaseBlock, L2SequencerError> {
        let pbbr = envelope.parent_beacon_block_root;
        let sidecar = match &envelope.execution_payload {
            BaseExecutionPayload::V4(_) => BaseExecutionPayloadSidecar::v4(
                CancunPayloadFields {
                    parent_beacon_block_root: pbbr.unwrap_or_default(),
                    versioned_hashes: vec![],
                },
                PraguePayloadFields::new(EMPTY_REQUESTS_HASH),
            ),
            _ => pbbr.map_or_else(BaseExecutionPayloadSidecar::default, |pbbr| {
                BaseExecutionPayloadSidecar::v3(CancunPayloadFields {
                    parent_beacon_block_root: pbbr,
                    versioned_hashes: vec![],
                })
            }),
        };
        envelope
            .execution_payload
            .clone()
            .try_into_block_with_sidecar(&sidecar)
            .map_err(|e| L2SequencerError::PayloadConversion(format!("{e}")))
    }

    /// Convert a [`BaseBlock`] into a gossip network envelope, signing when a key is supplied.
    pub fn network_envelope(
        block: &BaseBlock,
        signer: Option<&PrivateKeySigner>,
        chain_id: u64,
    ) -> NetworkPayloadEnvelope {
        let block_hash = block.header.hash_slow();
        let (execution_payload, _) = BaseExecutionPayload::from_block_unchecked(block_hash, block);
        let parent_beacon_block_root = block.header.parent_beacon_block_root;

        let (signature, payload_hash) = signer.map_or_else(
            || (Signature::new(U256::ZERO, U256::ZERO, false), PayloadHash(B256::ZERO)),
            |signer| {
                let envelope = BaseExecutionPayloadEnvelope {
                    execution_payload: execution_payload.clone(),
                    parent_beacon_block_root,
                };
                let ph = envelope.payload_hash();
                let msg = ph.signature_message(chain_id);
                let sig = signer.sign_hash_sync(&msg).expect("unsafe block signing must not fail");
                (sig, ph)
            },
        );

        NetworkPayloadEnvelope {
            payload: execution_payload,
            signature,
            payload_hash,
            parent_beacon_block_root,
        }
    }
}
