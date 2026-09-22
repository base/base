//! Payload attributes reconstructed the same way the proof host prefetcher does.

use alloy_eips::eip2718::Encodable2718;
use alloy_network::Network;
use alloy_primitives::{B64, B256, keccak256};
use alloy_rpc_types::Block;
use base_common_consensus::{HoloceneExtraData, JovianExtraData};
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use base_common_rpc_types_engine::BasePayloadAttributes;

/// Builds the payload attributes a prover sends to `debug_executePayload`.
#[derive(Debug)]
pub struct PayloadAttributes;

impl PayloadAttributes {
    /// Reconstructs payload attributes from a canonical L2 block.
    ///
    /// The digest of this value is the cache key. It must match
    /// `keccak256(serde_json::to_vec(BasePayloadAttributes))` for the attributes
    /// the guest puts in an `L2PayloadWitness` hint.
    pub fn from_l2_block(
        rollup_config: &RollupConfig,
        block: Block<<Base as Network>::TransactionResponse, <Base as Network>::HeaderResponse>,
    ) -> Result<BasePayloadAttributes, PayloadAttributesError> {
        let timestamp = block.header.inner.timestamp;
        let mut payload_attributes = BasePayloadAttributes::default();
        payload_attributes.payload_attributes.timestamp = timestamp;
        payload_attributes.payload_attributes.prev_randao = block.header.inner.mix_hash;
        payload_attributes.payload_attributes.suggested_fee_recipient =
            block.header.inner.beneficiary;
        payload_attributes.payload_attributes.parent_beacon_block_root =
            block.header.inner.parent_beacon_block_root;
        payload_attributes.payload_attributes.withdrawals =
            block.withdrawals.as_ref().map(|withdrawals| withdrawals.0.clone());
        payload_attributes.transactions = Some(
            block
                .transactions
                .into_transactions()
                .map(|tx| tx.as_ref().encoded_2718().into())
                .collect(),
        );
        payload_attributes.no_tx_pool = Some(true);
        payload_attributes.gas_limit = Some(block.header.inner.gas_limit);

        if rollup_config.is_jovian_active(timestamp) {
            let (elasticity, denominator, min_base_fee) =
                JovianExtraData::decode(&block.header.inner.extra_data)?;
            payload_attributes.eip_1559_params =
                Some(Self::encode_eip_1559_params(elasticity, denominator));
            payload_attributes.min_base_fee = Some(min_base_fee);
        } else if rollup_config.is_holocene_active(timestamp) {
            let (elasticity, denominator) =
                HoloceneExtraData::decode(&block.header.inner.extra_data)?;
            payload_attributes.eip_1559_params =
                Some(Self::encode_eip_1559_params(elasticity, denominator));
        }

        Ok(payload_attributes)
    }

    /// Digest of the JSON encoding stored and fetched by the witness cache.
    pub fn digest(
        payload_attributes: &BasePayloadAttributes,
    ) -> Result<B256, PayloadAttributesError> {
        Ok(keccak256(serde_json::to_vec(payload_attributes)?))
    }

    fn encode_eip_1559_params(elasticity: u32, denominator: u32) -> B64 {
        let mut encoded = [0u8; 8];
        encoded[..4].copy_from_slice(&denominator.to_be_bytes());
        encoded[4..].copy_from_slice(&elasticity.to_be_bytes());
        B64::from(encoded)
    }
}

/// Failure to reconstruct or hash payload attributes.
#[derive(Debug, thiserror::Error)]
pub enum PayloadAttributesError {
    /// Block extra data did not match the active fork's encoding.
    #[error(transparent)]
    ExtraData(#[from] base_common_consensus::EIP1559ParamError),
    /// Payload attributes could not be serialized to the digest preimage.
    #[error("failed to serialize payload attributes: {0}")]
    Serialize(#[from] serde_json::Error),
}
