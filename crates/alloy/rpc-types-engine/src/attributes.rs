//! Optimism-specific payload attributes.

use alloc::vec::Vec;
use alloy_eips::{
    Decodable2718,
    eip1559::BaseFeeParams,
    eip2718::{Eip2718Result, WithEncoded},
};
use alloy_primitives::{B64, Bytes};
use alloy_rlp::Result;
use alloy_rpc_types_engine::PayloadAttributes;
use op_alloy_consensus::{
    EIP1559ParamError, OpTxEnvelope, decode_eip_1559_params, encode_holocene_extra_data,
    encode_jovian_extra_data,
};

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
    /// If set, this sets the minimum base fee for the block.
    ///
    /// Prior to Jovian activation, this field should always be [None].
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub min_base_fee: Option<u64>,
}

impl OpPayloadAttributes {
    /// Encodes the `eip1559` parameters for the payload.
    pub fn get_holocene_extra_data(
        &self,
        default_base_fee_params: BaseFeeParams,
    ) -> Result<Bytes, EIP1559ParamError> {
        if self.min_base_fee.is_some() {
            return Err(EIP1559ParamError::MinBaseFeeMustBeNone);
        }
        self.eip_1559_params
            .map(|params| encode_holocene_extra_data(params, default_base_fee_params))
            .ok_or(EIP1559ParamError::NoEIP1559Params)?
    }

    /// Extracts the Holocene 1599 parameters from the encoded form:
    /// <https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/holocene/exec-engine.md#eip1559params-encoding>
    ///
    /// Returns (`elasticity`, `denominator`)
    pub fn decode_eip_1559_params(&self) -> Option<(u32, u32)> {
        self.eip_1559_params.map(decode_eip_1559_params)
    }

    /// Encodes the `eip1559` parameters for the payload along with the minimum base fee.
    pub fn get_jovian_extra_data(
        &self,
        default_base_fee_params: BaseFeeParams,
    ) -> Result<Bytes, EIP1559ParamError> {
        if self.min_base_fee.is_none() {
            return Err(EIP1559ParamError::MinBaseFeeNotSet);
        }
        self.eip_1559_params
            .map(|params| {
                encode_jovian_extra_data(
                    params,
                    default_base_fee_params,
                    self.min_base_fee.unwrap(),
                )
            })
            .ok_or(EIP1559ParamError::NoEIP1559Params)?
    }

    /// Returns an iterator over the decoded [`OpTxEnvelope`] in this attributes.
    ///
    /// This iterator will be empty if there are no transactions in the attributes.
    pub fn decoded_transactions(&self) -> impl Iterator<Item = Eip2718Result<OpTxEnvelope>> + '_ {
        self.transactions.iter().flatten().map(|tx_bytes| {
            let mut buf = tx_bytes.as_ref();
            let tx = OpTxEnvelope::decode_2718(&mut buf).map_err(alloy_rlp::Error::from)?;
            if !buf.is_empty() {
                return Err(alloy_rlp::Error::UnexpectedLength.into());
            }
            Ok(tx)
        })
    }

    /// Returns iterator over decoded transactions with their original encoded bytes.
    ///
    /// This iterator will be empty if there are no transactions in the attributes.
    pub fn decoded_transactions_with_encoded(
        &self,
    ) -> impl Iterator<Item = Eip2718Result<WithEncoded<OpTxEnvelope>>> + '_ {
        self.transactions
            .iter()
            .flatten()
            .cloned()
            .zip(self.decoded_transactions())
            .map(|(tx_bytes, result)| result.map(|op_tx| WithEncoded::new(tx_bytes, op_tx)))
    }

    /// Returns an iterator over the recovered [`OpTxEnvelope`] in this attributes.
    ///
    /// This iterator will be empty if there are no transactions in the attributes.
    #[cfg(feature = "k256")]
    pub fn recovered_transactions(
        &self,
    ) -> impl Iterator<
        Item = Result<
            alloy_consensus::transaction::Recovered<OpTxEnvelope>,
            alloy_consensus::crypto::RecoveryError,
        >,
    > + '_ {
        use alloy_consensus::transaction::SignerRecoverable;

        self.decoded_transactions().map(|res| {
            res.map_err(alloy_consensus::crypto::RecoveryError::from_source)
                .and_then(|tx| tx.try_into_recovered())
        })
    }

    /// Returns an iterator over the recovered [`OpTxEnvelope`] in this attributes with their
    /// original encoded bytes.
    ///
    /// This iterator will be empty if there are no transactions in the attributes.
    #[cfg(feature = "k256")]
    pub fn recovered_transactions_with_encoded(
        &self,
    ) -> impl Iterator<
        Item = Result<
            WithEncoded<alloy_consensus::transaction::Recovered<OpTxEnvelope>>,
            alloy_consensus::crypto::RecoveryError,
        >,
    > + '_ {
        self.transactions
            .iter()
            .flatten()
            .cloned()
            .zip(self.recovered_transactions())
            .map(|(tx_bytes, result)| result.map(|op_tx| WithEncoded::new(tx_bytes, op_tx)))
    }
}

#[cfg(all(test, feature = "serde"))]
mod test {
    use super::*;
    use alloc::vec;
    use alloy_primitives::{Address, B256, b64};
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
            min_base_fee: None,
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
            min_base_fee: None,
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

    #[test]
    fn test_serde_roundtrip_attributes_pre_jovian() {
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
            min_base_fee: None,
        };

        let ser = serde_json::to_string(&attributes).unwrap();
        let de: OpPayloadAttributes = serde_json::from_str(&ser).unwrap();

        assert_eq!(attributes, de);
    }

    #[test]
    fn test_serde_roundtrip_attributes_post_jovian() {
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
            min_base_fee: Some(1),
        };

        let ser = serde_json::to_string(&attributes).unwrap();
        let de: OpPayloadAttributes = serde_json::from_str(&ser).unwrap();

        assert_eq!(attributes, de);
    }

    #[test]
    fn test_get_extra_data_post_jovian() {
        let attributes = OpPayloadAttributes {
            eip_1559_params: Some(B64::from_str("0x0000000800000008").unwrap()),
            min_base_fee: Some(257),
            ..Default::default()
        };

        let extra_data = attributes.get_jovian_extra_data(BaseFeeParams::new(80, 60));
        assert_eq!(
            extra_data.unwrap(),
            Bytes::copy_from_slice(&[1, 0, 0, 0, 8, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 1, 1])
        );
    }

    #[test]
    fn test_get_extra_data_post_jovian_default() {
        let attributes = OpPayloadAttributes {
            eip_1559_params: Some(B64::ZERO),
            min_base_fee: Some(0),
            ..Default::default()
        };

        let extra_data = attributes.get_jovian_extra_data(BaseFeeParams::new(80, 60));
        assert_eq!(
            extra_data.unwrap(),
            Bytes::copy_from_slice(&[1, 0, 0, 0, 80, 0, 0, 0, 60, 0, 0, 0, 0, 0, 0, 0, 0])
        );
    }

    #[test]
    fn test_get_jovian_extra_data_fails_without_min_base_fee() {
        let attributes = OpPayloadAttributes {
            eip_1559_params: Some(B64::from_str("0x0000000800000008").unwrap()),
            min_base_fee: None,
            ..Default::default()
        };

        let result = attributes.get_jovian_extra_data(BaseFeeParams::new(80, 60));
        assert_eq!(result.unwrap_err(), EIP1559ParamError::MinBaseFeeNotSet);
    }

    #[test]
    fn test_min_base_fee_must_be_none_before_jovian() {
        let attributes = OpPayloadAttributes {
            eip_1559_params: Some(B64::from_str("0x0000000800000008").unwrap()),
            min_base_fee: Some(100),
            ..Default::default()
        };

        // Use Holocene function for pre-Jovian decoding of extra data
        let result = attributes.get_holocene_extra_data(BaseFeeParams::new(80, 60));
        assert_eq!(result.unwrap_err(), EIP1559ParamError::MinBaseFeeMustBeNone);
    }
}
