//! Builds Base execution environments from payload attributes.

use alloy_consensus::BlockHeader;
use base_common_chains::Upgrades;
use reth_chainspec::EthChainSpec;
use reth_payload_primitives::{BasePayloadBuilderAttributes, BuildNextEnv, PayloadBuilderError};
use reth_primitives_traits::{SealedHeader, SignedTransaction};

use crate::BaseNextBlockEnvAttributes;

impl<H, T, ChainSpec> BuildNextEnv<BasePayloadBuilderAttributes<T>, H, ChainSpec>
    for BaseNextBlockEnvAttributes
where
    H: BlockHeader,
    T: SignedTransaction,
    ChainSpec: EthChainSpec + Upgrades,
{
    fn build_next_env(
        attributes: &BasePayloadBuilderAttributes<T>,
        parent: &SealedHeader<H>,
        chain_spec: &ChainSpec,
    ) -> Result<Self, PayloadBuilderError> {
        let extra_data =
            if chain_spec.is_jovian_active_at_timestamp(attributes.payload_attributes.timestamp) {
                attributes
                    .get_jovian_extra_data(
                        chain_spec
                            .base_fee_params_at_timestamp(attributes.payload_attributes.timestamp),
                    )
                    .map_err(PayloadBuilderError::other)?
            } else if chain_spec
                .is_holocene_active_at_timestamp(attributes.payload_attributes.timestamp)
            {
                attributes
                    .get_holocene_extra_data(
                        chain_spec
                            .base_fee_params_at_timestamp(attributes.payload_attributes.timestamp),
                    )
                    .map_err(PayloadBuilderError::other)?
            } else {
                Default::default()
            };

        Ok(Self {
            timestamp: attributes.payload_attributes.timestamp,
            suggested_fee_recipient: attributes.payload_attributes.suggested_fee_recipient,
            prev_randao: attributes.payload_attributes.prev_randao,
            gas_limit: attributes.gas_limit.unwrap_or_else(|| parent.gas_limit()),
            parent_beacon_block_root: attributes.payload_attributes.parent_beacon_block_root,
            extra_data,
        })
    }
}
