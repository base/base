//! Optimism-specific payload attributes.

use crate::error::EIP1559ParamError;
use alloc::vec::Vec;
use alloy_eips::eip1559::BaseFeeParams;
use alloy_primitives::{Bytes, B64};
use alloy_rpc_types_engine::PayloadAttributes;
use op_alloy_protocol::{fee::decode_eip_1559_params, L2BlockInfo};

/// Optimism Payload Attributes
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct OpPayloadAttributes {
    /// The payload attributes
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub payload_attributes: PayloadAttributes,
    /// Transactions is a field for rollups: the transactions list is forced into the block
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub transactions: Option<Vec<Bytes>>,
    /// If true, the no transactions are taken out of the tx-pool, only transactions from the above
    /// Transactions list will be included.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub no_tx_pool: Option<bool>,
    /// If set, this sets the exact gas limit the block produced with.
    #[cfg_attr(
        feature = "serde",
        serde(skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")
    )]
    pub gas_limit: Option<u64>,
    /// If set, this sets the EIP-1559 parameters for the block.
    ///
    /// Prior to Holocene activation, this field should always be [None].
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub eip_1559_params: Option<B64>,
}

impl OpPayloadAttributes {
    /// Extracts the `eip1559` parameters for the payload.
    pub fn get_holocene_extra_data(
        &self,
        default_base_fee_params: BaseFeeParams,
    ) -> Result<Bytes, EIP1559ParamError> {
        let eip_1559_params = self.eip_1559_params.ok_or(EIP1559ParamError::NoEIP1559Params)?;

        let mut extra_data = [0u8; 9];
        // If eip 1559 params aren't set, use the canyon base fee param constants
        // otherwise use them
        if eip_1559_params.is_zero() {
            // Try casting max_change_denominator to u32
            let max_change_denominator: u32 = (default_base_fee_params.max_change_denominator)
                .try_into()
                .map_err(|_| EIP1559ParamError::DenominatorOverflow)?;

            // Try casting elasticity_multiplier to u32
            let elasticity_multiplier: u32 = (default_base_fee_params.elasticity_multiplier)
                .try_into()
                .map_err(|_| EIP1559ParamError::ElasticityOverflow)?;

            // Copy the values safely
            extra_data[1..5].copy_from_slice(&max_change_denominator.to_be_bytes());
            extra_data[5..9].copy_from_slice(&elasticity_multiplier.to_be_bytes());
        } else {
            let (elasticity, denominator) = decode_eip_1559_params(eip_1559_params);
            extra_data[1..5].copy_from_slice(&denominator.to_be_bytes());
            extra_data[5..9].copy_from_slice(&elasticity.to_be_bytes());
        }
        Ok(Bytes::copy_from_slice(&extra_data))
    }
}

/// Optimism Payload Attributes with parent block reference.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct OpAttributesWithParent {
    /// The payload attributes.
    pub attributes: OpPayloadAttributes,
    /// The parent block reference.
    pub parent: L2BlockInfo,
    /// Whether the current batch is the last in its span.
    pub is_last_in_span: bool,
}

impl OpAttributesWithParent {
    /// Create a new [OpAttributesWithParent] instance.
    pub const fn new(
        attributes: OpPayloadAttributes,
        parent: L2BlockInfo,
        is_last_in_span: bool,
    ) -> Self {
        Self { attributes, parent, is_last_in_span }
    }

    /// Returns the payload attributes.
    pub const fn attributes(&self) -> &OpPayloadAttributes {
        &self.attributes
    }

    /// Returns the parent block reference.
    pub const fn parent(&self) -> &L2BlockInfo {
        &self.parent
    }

    /// Returns whether the current batch is the last in its span.
    pub const fn is_last_in_span(&self) -> bool {
        self.is_last_in_span
    }
}

#[cfg(all(test, feature = "serde"))]
mod test {
    use super::*;
    use alloy_primitives::{b64, Address, B256};
    use alloy_rpc_types_engine::PayloadAttributes;
    use core::str::FromStr;

    #[test]
    fn test_serde_roundtrip_attributes_pre_holocene() {
        let attributes = OpPayloadAttributes {
            payload_attributes: PayloadAttributes {
                timestamp: 0x1337,
                prev_randao: B256::ZERO,
                suggested_fee_recipient: Address::ZERO,
                withdrawals: Default::default(),
                parent_beacon_block_root: Some(B256::ZERO),
            },
            transactions: Some(vec![b"hello".to_vec().into()]),
            no_tx_pool: Some(true),
            gas_limit: Some(42),
            eip_1559_params: None,
        };

        let ser = serde_json::to_string(&attributes).unwrap();
        let de: OpPayloadAttributes = serde_json::from_str(&ser).unwrap();

        assert_eq!(attributes, de);
    }

    #[test]
    fn test_serde_roundtrip_attributes_post_holocene() {
        let attributes = OpPayloadAttributes {
            payload_attributes: PayloadAttributes {
                timestamp: 0x1337,
                prev_randao: B256::ZERO,
                suggested_fee_recipient: Address::ZERO,
                withdrawals: Default::default(),
                parent_beacon_block_root: Some(B256::ZERO),
            },
            transactions: Some(vec![b"hello".to_vec().into()]),
            no_tx_pool: Some(true),
            gas_limit: Some(42),
            eip_1559_params: Some(b64!("0000dead0000beef")),
        };

        let ser = serde_json::to_string(&attributes).unwrap();
        let de: OpPayloadAttributes = serde_json::from_str(&ser).unwrap();

        assert_eq!(attributes, de);
    }

    #[test]
    fn test_get_extra_data_post_holocene() {
        let attributes = OpPayloadAttributes {
            eip_1559_params: Some(B64::from_str("0x0000000800000008").unwrap()),
            ..Default::default()
        };
        let extra_data = attributes.get_holocene_extra_data(BaseFeeParams::new(80, 60));
        assert_eq!(extra_data.unwrap(), Bytes::copy_from_slice(&[0, 0, 0, 0, 8, 0, 0, 0, 8]));
    }

    #[test]
    fn test_get_extra_data_post_holocene_default() {
        let attributes =
            OpPayloadAttributes { eip_1559_params: Some(B64::ZERO), ..Default::default() };
        let extra_data = attributes.get_holocene_extra_data(BaseFeeParams::new(80, 60));
        assert_eq!(extra_data.unwrap(), Bytes::copy_from_slice(&[0, 0, 0, 0, 80, 0, 0, 0, 60]));
    }
}
