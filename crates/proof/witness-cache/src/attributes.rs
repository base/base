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

#[cfg(test)]
mod tests {
    use alloy_network::Network;
    use alloy_primitives::{Address, B64, B256, Bytes, keccak256};
    use alloy_rpc_types::Block;
    use base_common_genesis::RollupConfig;
    use base_common_network::Base;
    use base_common_rpc_types_engine::BasePayloadAttributes;

    use super::PayloadAttributes;

    #[test]
    fn digest_matches_the_guest_encoding_of_the_reconstructed_attributes() {
        let timestamp = 1_700_000_000;
        let prev_randao = B256::repeat_byte(0x11);
        let suggested_fee_recipient = Address::repeat_byte(0x22);
        let parent_beacon_block_root = B256::repeat_byte(0x33);
        let gas_limit = 30_000_000;
        // Holocene extra data: version 0, denominator 2, elasticity 6.
        let extra_data = Bytes::from(vec![0, 0, 0, 0, 2, 0, 0, 0, 6]);

        let mut block = Block::<
            <Base as Network>::TransactionResponse,
            <Base as Network>::HeaderResponse,
        >::default();
        block.header.inner.timestamp = timestamp;
        block.header.inner.mix_hash = prev_randao;
        block.header.inner.beneficiary = suggested_fee_recipient;
        block.header.inner.parent_beacon_block_root = Some(parent_beacon_block_root);
        block.header.inner.gas_limit = gas_limit;
        block.header.inner.extra_data = extra_data;

        let mut rollup_config = RollupConfig::default();
        rollup_config.upgrades.holocene_time = Some(0);

        let reconstructed = PayloadAttributes::from_l2_block(&rollup_config, block).unwrap();
        let expected = BasePayloadAttributes {
            payload_attributes: {
                let mut payload_attributes = BasePayloadAttributes::default().payload_attributes;
                payload_attributes.timestamp = timestamp;
                payload_attributes.prev_randao = prev_randao;
                payload_attributes.suggested_fee_recipient = suggested_fee_recipient;
                payload_attributes.parent_beacon_block_root = Some(parent_beacon_block_root);
                payload_attributes
            },
            transactions: Some(vec![]),
            no_tx_pool: Some(true),
            gas_limit: Some(gas_limit),
            eip_1559_params: Some(B64::from([0, 0, 0, 2, 0, 0, 0, 6])),
            min_base_fee: None,
        };
        assert_eq!(reconstructed, expected);

        let encoded = serde_json::to_vec(&expected).unwrap();
        assert_eq!(PayloadAttributes::digest(&reconstructed).unwrap(), keccak256(encoded));
    }
}
