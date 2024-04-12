use crate::op::transaction::tx_type::TxType;
use alloy::{rpc::types::eth::Log, serde as alloy_serde};
use alloy_primitives::{Address, B256};
use op_consensus::receipt::ReceiptEnvelope;
use serde::{Deserialize, Serialize};
/// Transaction receipt
///
/// This type is generic over an inner [`ReceiptEnvelope`] which contains
/// consensus data and metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(
    any(test, feature = "arbitrary"),
    derive(proptest_derive::Arbitrary, arbitrary::Arbitrary)
)]
#[serde(rename_all = "camelCase")]
pub struct TransactionReceipt<T = ReceiptEnvelope<Log>> {
    /// The receipt envelope, which contains the consensus receipt data..
    #[serde(flatten)]
    pub inner: T,
    /// Transaction Hash.
    pub transaction_hash: B256,
    /// Index within the block.
    #[serde(with = "alloy_serde::u64_hex")]
    pub transaction_index: u64,
    /// Hash of the block this transaction was included within.
    pub block_hash: Option<B256>,
    /// Number of the block this transaction was included within.
    #[serde(with = "alloy_serde::u64_hex_opt")]
    pub block_number: Option<u64>,
    /// Gas used by this transaction alone.
    #[serde(with = "alloy_serde::u128_hex_or_decimal")]
    pub gas_used: u128,
    /// The price paid post-execution by the transaction (i.e. base fee + priority fee). Both
    /// fields in 1559-style transactions are maximums (max fee + max priority fee), the amount
    /// that's actually paid by users can only be determined post-execution
    #[serde(with = "alloy_serde::u128_hex_or_decimal")]
    pub effective_gas_price: u128,
    /// Blob gas used by the eip-4844 transaction
    ///
    /// This is None for non eip-4844 transactions
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "alloy_serde::u128_hex_or_decimal_opt",
        default
    )]
    pub blob_gas_used: Option<u128>,
    /// The price paid by the eip-4844 transaction per blob gas.
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "alloy_serde::u128_hex_or_decimal_opt",
        default
    )]
    pub blob_gas_price: Option<u128>,
    /// Address of the sender
    pub from: Address,
    /// Address of the receiver. None when its a contract creation transaction.
    pub to: Option<Address>,
    /// Contract address created, or None if not a deployment.
    pub contract_address: Option<Address>,
    /// The post-transaction stateroot (pre Byzantium)
    ///
    /// EIP98 makes this optional field, if it's missing then skip serializing it
    #[serde(skip_serializing_if = "Option::is_none", rename = "root")]
    pub state_root: Option<B256>,
    /// The fee associated with a transaction on the Layer 1
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "alloy_serde::u128_hex_or_decimal_opt"
    )]
    pub l1_fee: Option<u128>,
    /// A multiplier applied to the actual gas usage on Layer 1 to calculate the dynamic costs.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "l1_fee_scalar_serde")]
    pub l1_fee_scalar: Option<f64>,
    /// The gas price for transactions on the Layer 1
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "alloy_serde::u128_hex_or_decimal_opt"
    )]
    pub l1_gas_price: Option<u128>,
    /// The amount of gas consumed by a transaction on the Layer 1
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "alloy_serde::u128_hex_or_decimal_opt"
    )]
    pub l1_gas_used: Option<u128>,
    /// Deposit nonce for Optimism deposit transactions
    #[serde(skip_serializing_if = "Option::is_none", with = "alloy_serde::u64_hex_opt")]
    pub deposit_nonce: Option<u64>,
    /// Deposit receipt version for Optimism deposit transactions
    ///
    ///
    /// The deposit receipt version was introduced in Canyon to indicate an update to how
    /// receipt hashes should be computed when set. The state transition process
    /// ensures this is only set for post-Canyon deposit transactions.
    /// The value is always equal to `1` when present.
    #[serde(skip_serializing_if = "Option::is_none", with = "alloy_serde::u64_hex_opt")]
    pub deposit_receipt_version: Option<u64>,
    pub tx_type: tx_type::TxType,
}

impl AsRef<ReceiptEnvelope<Log>> for TransactionReceipt {
    fn as_ref(&self) -> &ReceiptEnvelope<Log> {
        &self.inner
    }
}

impl TransactionReceipt {
    /// Returns the status of the transaction.
    pub const fn status(&self) -> bool {
        match &self.inner {
            ReceiptEnvelope::Eip1559(receipt)
            | ReceiptEnvelope::Eip2930(receipt)
            | ReceiptEnvelope::Eip4844(receipt)
            | ReceiptEnvelope::Deposit(receipt)
            | ReceiptEnvelope::Legacy(receipt) => receipt.receipt.status,
            _ => false,
        }
    }

    /// Returns the transaction type.
    pub const fn transaction_type(&self) -> TxType {
        self.inner.tx_type()
    }

    /// Calculates the address that will be created by the transaction, if any.
    ///
    /// Returns `None` if the transaction is not a contract creation (the `to` field is set), or if
    /// the `from` field is not set.
    pub fn calculate_create_address(&self, nonce: u64) -> Option<Address> {
        if self.to.is_some() {
            return None;
        }
        Some(self.from.create(nonce))
    }
}

impl<T> TransactionReceipt<T> {
    /// Maps the inner receipt value of this receipt.
    pub fn map_inner<U, F>(self, f: F) -> TransactionReceipt<U>
    where
        F: FnOnce(T) -> U,
    {
        TransactionReceipt {
            inner: f(self.inner),
            transaction_hash: self.transaction_hash,
            transaction_index: self.transaction_index,
            block_hash: self.block_hash,
            block_number: self.block_number,
            gas_used: self.gas_used,
            effective_gas_price: self.effective_gas_price,
            blob_gas_used: self.blob_gas_used,
            blob_gas_price: self.blob_gas_price,
            from: self.from,
            to: self.to,
            contract_address: self.contract_address,
            state_root: self.state_root,
            l1_fee: self.l1_fee,
            l1_fee_scalar: self.l1_fee_scalar,
            l1_gas_price: self.l1_gas_price,
            l1_gas_used: self.l1_gas_used,
            deposit_nonce: self.deposit_nonce,
            deposit_receipt_version: self.deposit_receipt_version,
            tx_type: self.tx_type,
        }
    }
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
