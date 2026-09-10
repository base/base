//! Builds Base execution environments from payload attributes.

use base_common_chain_config::{BaseChainSpec, Upgrades};
use base_common_types_chain::{BlockHeader, SealedHeader};
use base_common_types_payload::{BasePayloadBuilderAttributes, PayloadBuilderError};

use crate::BaseNextBlockEnvAttributes;

impl BaseNextBlockEnvAttributes {
    /// Builds the next Base execution environment from payload attributes.
    pub fn build_next_env(
        attributes: &BasePayloadBuilderAttributes,
        parent: &SealedHeader,
        chain_spec: &BaseChainSpec,
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
