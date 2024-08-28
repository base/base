//! Receipt types for RPC

use alloy_network::ReceiptResponse;
use alloy_primitives::BlockHash;
use alloy_serde::OtherFields;
use op_alloy_consensus::OpReceiptEnvelope;
use serde::{Deserialize, Serialize};

/// OP Transaction Receipt type
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[doc(alias = "OpTxReceipt")]
pub struct OpTransactionReceipt {
    /// Regular eth transaction receipt including deposit receipts
    #[serde(flatten)]
    pub inner: alloy_rpc_types_eth::TransactionReceipt<OpReceiptEnvelope<alloy_rpc_types_eth::Log>>,
}

impl ReceiptResponse for OpTransactionReceipt {
    fn contract_address(&self) -> Option<alloy_primitives::Address> {
        self.inner.contract_address
    }

    fn status(&self) -> bool {
        self.inner.inner.status()
    }

    fn block_hash(&self) -> Option<BlockHash> {
        self.inner.block_hash
    }

    fn block_number(&self) -> Option<u64> {
        self.inner.block_number
    }
}

/// Additional fields for Optimism transaction receipts
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[doc(alias = "OptimismTxReceiptFields")]
pub struct OptimismTransactionReceiptFields {
    /// Deposit nonce for deposit transactions post-regolith
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub deposit_nonce: Option<u64>,
    /// Deposit receipt version for deposit transactions post-canyon
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub deposit_receipt_version: Option<u64>,
    /// L1 fee for the transaction
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub l1_fee: Option<u128>,
    /// L1 fee scalar for the transaction
    #[serde(default, skip_serializing_if = "Option::is_none", with = "l1_fee_scalar_serde")]
    pub l1_fee_scalar: Option<f64>,
    /// L1 gas price for the transaction
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub l1_gas_price: Option<u128>,
    /// L1 gas used for the transaction
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub l1_gas_used: Option<u128>,
}

/// Serialize/Deserialize l1FeeScalar to/from string
mod l1_fee_scalar_serde {
    use serde::{de, Deserialize};

    pub(super) fn serialize<S>(value: &Option<f64>, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if let Some(v) = value {
            return s.serialize_str(&v.to_string());
        }
        s.serialize_none()
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s: Option<String> = Option::deserialize(deserializer)?;
        if let Some(s) = s {
            return Ok(Some(s.parse::<f64>().map_err(de::Error::custom)?));
        }

        Ok(None)
    }
}

impl From<OptimismTransactionReceiptFields> for OtherFields {
    fn from(value: OptimismTransactionReceiptFields) -> Self {
        serde_json::to_value(value).unwrap().try_into().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    // <https://github.com/alloy-rs/op-alloy/issues/18>
    #[test]
    fn parse_rpc_receipt() {
        let s = r#"{
        "blockHash": "0x9e6a0fb7e22159d943d760608cc36a0fb596d1ab3c997146f5b7c55c8c718c67",
        "blockNumber": "0x6cfef89",
        "contractAddress": null,
        "cumulativeGasUsed": "0xfa0d",
        "depositNonce": "0x8a2d11",
        "effectiveGasPrice": "0x0",
        "from": "0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001",
        "gasUsed": "0xfa0d",
        "logs": [],
        "logsBloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        "status": "0x1",
        "to": "0x4200000000000000000000000000000000000015",
        "transactionHash": "0xb7c74afdeb7c89fb9de2c312f49b38cb7a850ba36e064734c5223a477e83fdc9",
        "transactionIndex": "0x0",
        "type": "0x7e"
    }"#;

        let receipt: OpTransactionReceipt = serde_json::from_str(s).unwrap();
        let value = serde_json::to_value(&receipt).unwrap();
        let expected_value = serde_json::from_str::<serde_json::Value>(s).unwrap();
        assert_eq!(value, expected_value);
    }

    #[test]
    fn serialize_empty_optimism_transaction_receipt_fields_struct() {
        let op_fields = OptimismTransactionReceiptFields::default();

        let json = serde_json::to_value(op_fields).unwrap();
        assert_eq!(json, json!({}));
    }

    #[test]
    fn serialize_l1_fee_scalar() {
        let op_fields = OptimismTransactionReceiptFields {
            l1_fee_scalar: Some(0.678),
            ..OptimismTransactionReceiptFields::default()
        };

        let json = serde_json::to_value(op_fields).unwrap();

        assert_eq!(json["l1FeeScalar"], serde_json::Value::String("0.678".to_string()));
    }

    #[test]
    fn deserialize_l1_fee_scalar() {
        let json = json!({
            "l1FeeScalar": "0.678"
        });

        let op_fields: OptimismTransactionReceiptFields = serde_json::from_value(json).unwrap();
        assert_eq!(op_fields.l1_fee_scalar, Some(0.678f64));

        let json = json!({
            "l1FeeScalar": Value::Null
        });

        let op_fields: OptimismTransactionReceiptFields = serde_json::from_value(json).unwrap();
        assert_eq!(op_fields.l1_fee_scalar, None);

        let json = json!({});

        let op_fields: OptimismTransactionReceiptFields = serde_json::from_value(json).unwrap();
        assert_eq!(op_fields.l1_fee_scalar, None);
    }
}
