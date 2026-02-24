//! Types for pre-validated transactions.
//!
//! These types are used by both the mempool node (to construct validated transactions)
//! and the builder (to receive them via RPC).

use alloy_consensus::{SignableTransaction, Signed, TxEip1559, TxEip2930, TxLegacy};
use alloy_eips::eip2930::AccessList;
use alloy_primitives::{Address, Bytes, Signature, TxHash, TxKind, U256};
use base_alloy_consensus::OpPooledTransaction;
use reth_primitives_traits::Recovered;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Serde module for base64 encoding/decoding of `Bytes`.
///
/// Use with `#[serde(with = "base64_bytes")]` on `Bytes` fields.
pub mod base64_bytes {
    use alloy_primitives::Bytes;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    /// Serializes `Bytes` as a base64-encoded string.
    pub fn serialize<S>(bytes: &Bytes, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let encoded = STANDARD.encode(bytes.as_ref());
        serializer.serialize_str(&encoded)
    }

    /// Deserializes a base64-encoded string into `Bytes`.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Bytes, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let decoded = STANDARD.decode(&s).map_err(D::Error::custom)?;
        Ok(Bytes::from(decoded))
    }
}

/// Errors that can occur during transaction conversion.
#[derive(Debug, Error)]
pub enum ConversionError {
    /// The transaction type is not supported.
    #[error("unsupported transaction type: {0}")]
    UnsupportedTxType(u8),

    /// Missing `gas_price` for legacy or EIP-2930 transaction.
    #[error("missing gas_price for legacy/EIP-2930 transaction")]
    MissingGasPrice,

    /// Missing `max_fee_per_gas` for EIP-1559+ transaction.
    #[error("missing max_fee_per_gas for EIP-1559+ transaction")]
    MissingMaxFeePerGas,

    /// Missing `max_priority_fee_per_gas` for EIP-1559+ transaction.
    #[error("missing max_priority_fee_per_gas for EIP-1559+ transaction")]
    MissingMaxPriorityFeePerGas,

    /// Missing `chain_id` for typed transaction.
    #[error("missing chain_id for typed transaction")]
    MissingChainId,
}

/// Signature components for a transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionSignature {
    /// Recovery ID.
    ///
    /// For EIP-155+ transactions, this is 0 or 1.
    /// For legacy transactions, this may be 27 or 28 (or 27 + `chain_id` * 2 + 35).
    pub v: u64,
    /// R component of the signature.
    pub r: U256,
    /// S component of the signature.
    pub s: U256,
}

impl TransactionSignature {
    const fn to_signature(&self) -> Signature {
        let y_parity = match self.v {
            0 | 27 => false,
            1 | 28 => true,
            v => (v % 2) == 0,
        };
        Signature::new(self.r, self.s, y_parity)
    }
}

/// A pre-validated transaction with base64-encoded byte fields.
///
/// This represents a transaction that has already been validated by the sender
/// (e.g., the mempool node) and includes the recovered sender address.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatedTransaction {
    /// Recovered sender address.
    ///
    /// This is the address that was recovered from the signature.
    /// The RPC trusts that this has been verified by the caller.
    pub from: Address,

    /// Transaction type.
    ///
    /// - `0` = Legacy
    /// - `1` = EIP-2930 (access list)
    /// - `2` = EIP-1559 (dynamic fee)
    /// - `4` = EIP-7702 (set code)
    pub tx_type: u8,

    /// Chain ID (required for EIP-155+ transactions).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain_id: Option<u64>,

    /// Transaction nonce.
    pub nonce: u64,

    /// Recipient address.
    ///
    /// `None` for contract creation transactions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Address>,

    /// Value in wei.
    pub value: U256,

    /// Gas limit.
    pub gas_limit: u64,

    /// Gas price (legacy and EIP-2930 transactions).
    ///
    /// Mutually exclusive with `max_fee_per_gas`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gas_price: Option<u128>,

    /// Maximum fee per gas (EIP-1559+ transactions).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_fee_per_gas: Option<u128>,

    /// Maximum priority fee per gas (EIP-1559+ transactions).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_priority_fee_per_gas: Option<u128>,

    /// Input data (calldata) - BASE64 ENCODED.
    ///
    /// This field uses base64 encoding instead of hex for efficiency.
    #[serde(with = "base64_bytes")]
    pub input: Bytes,

    /// Access list (EIP-2930+ transactions).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_list: Option<AccessList>,

    /// Transaction signature.
    pub signature: TransactionSignature,
}

impl ValidatedTransaction {
    /// Converts this validated transaction into a recovered pooled transaction.
    ///
    /// The `from` field is trusted as the recovered sender address.
    pub fn into_recovered(self) -> Result<Recovered<OpPooledTransaction>, ConversionError> {
        let from = self.from;
        let signature = self.signature.to_signature();
        let tx_kind = self.to.map_or(TxKind::Create, TxKind::Call);
        let tx_type = self.tx_type;
        let access_list = self.access_list.clone().unwrap_or_default();

        let pooled = match tx_type {
            0 => self.into_legacy(signature, tx_kind)?,
            1 => self.into_eip2930(signature, tx_kind, access_list)?,
            2 => self.into_eip1559(signature, tx_kind, access_list)?,
            4 => return Err(ConversionError::UnsupportedTxType(4)),
            other => return Err(ConversionError::UnsupportedTxType(other)),
        };

        Ok(Recovered::new_unchecked(pooled, from))
    }

    /// Computes the transaction hash.
    ///
    /// This reconstructs the transaction just enough to compute the hash.
    pub fn compute_tx_hash(&self) -> Result<TxHash, ConversionError> {
        let tx_kind = self.to.map_or(TxKind::Create, TxKind::Call);
        let access_list = self.access_list.clone().unwrap_or_default();

        match self.tx_type {
            0 => {
                let gas_price = self.gas_price.ok_or(ConversionError::MissingGasPrice)?;
                let inner = TxLegacy {
                    chain_id: self.chain_id,
                    nonce: self.nonce,
                    gas_price,
                    gas_limit: self.gas_limit,
                    to: tx_kind,
                    value: self.value,
                    input: self.input.clone(),
                };
                Ok(inner.signature_hash())
            }
            1 => {
                let chain_id = self.chain_id.ok_or(ConversionError::MissingChainId)?;
                let gas_price = self.gas_price.ok_or(ConversionError::MissingGasPrice)?;
                let inner = TxEip2930 {
                    chain_id,
                    nonce: self.nonce,
                    gas_price,
                    gas_limit: self.gas_limit,
                    to: tx_kind,
                    value: self.value,
                    input: self.input.clone(),
                    access_list,
                };
                Ok(inner.signature_hash())
            }
            2 => {
                let chain_id = self.chain_id.ok_or(ConversionError::MissingChainId)?;
                let max_fee_per_gas =
                    self.max_fee_per_gas.ok_or(ConversionError::MissingMaxFeePerGas)?;
                let max_priority_fee_per_gas = self
                    .max_priority_fee_per_gas
                    .ok_or(ConversionError::MissingMaxPriorityFeePerGas)?;
                let inner = TxEip1559 {
                    chain_id,
                    nonce: self.nonce,
                    max_fee_per_gas,
                    max_priority_fee_per_gas,
                    gas_limit: self.gas_limit,
                    to: tx_kind,
                    value: self.value,
                    input: self.input.clone(),
                    access_list,
                };
                Ok(inner.signature_hash())
            }
            other => Err(ConversionError::UnsupportedTxType(other)),
        }
    }

    fn into_legacy(
        self,
        signature: Signature,
        tx_kind: TxKind,
    ) -> Result<OpPooledTransaction, ConversionError> {
        let gas_price = self.gas_price.ok_or(ConversionError::MissingGasPrice)?;

        let inner = TxLegacy {
            chain_id: self.chain_id,
            nonce: self.nonce,
            gas_price,
            gas_limit: self.gas_limit,
            to: tx_kind,
            value: self.value,
            input: self.input,
        };

        let signature_hash = inner.signature_hash();
        let signed = Signed::new_unchecked(inner, signature, signature_hash);
        Ok(OpPooledTransaction::Legacy(signed))
    }

    fn into_eip2930(
        self,
        signature: Signature,
        tx_kind: TxKind,
        access_list: AccessList,
    ) -> Result<OpPooledTransaction, ConversionError> {
        let chain_id = self.chain_id.ok_or(ConversionError::MissingChainId)?;
        let gas_price = self.gas_price.ok_or(ConversionError::MissingGasPrice)?;

        let inner = TxEip2930 {
            chain_id,
            nonce: self.nonce,
            gas_price,
            gas_limit: self.gas_limit,
            to: tx_kind,
            value: self.value,
            input: self.input,
            access_list,
        };

        let signature_hash = inner.signature_hash();
        let signed = Signed::new_unchecked(inner, signature, signature_hash);
        Ok(OpPooledTransaction::Eip2930(signed))
    }

    fn into_eip1559(
        self,
        signature: Signature,
        tx_kind: TxKind,
        access_list: AccessList,
    ) -> Result<OpPooledTransaction, ConversionError> {
        let chain_id = self.chain_id.ok_or(ConversionError::MissingChainId)?;
        let max_fee_per_gas = self.max_fee_per_gas.ok_or(ConversionError::MissingMaxFeePerGas)?;
        let max_priority_fee_per_gas =
            self.max_priority_fee_per_gas.ok_or(ConversionError::MissingMaxPriorityFeePerGas)?;

        let inner = TxEip1559 {
            chain_id,
            nonce: self.nonce,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            gas_limit: self.gas_limit,
            to: tx_kind,
            value: self.value,
            input: self.input,
            access_list,
        };

        let signature_hash = inner.signature_hash();
        let signed = Signed::new_unchecked(inner, signature, signature_hash);
        Ok(OpPooledTransaction::Eip1559(signed))
    }
}

/// Result of inserting a single transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsertResult {
    /// Transaction hash.
    pub tx_hash: TxHash,
    /// Whether the insertion succeeded.
    pub success: bool,
    /// Error message if the insertion failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl InsertResult {
    /// Creates a successful result.
    pub const fn success(tx_hash: TxHash) -> Self {
        Self { tx_hash, success: true, error: None }
    }

    /// Creates a failed result.
    pub fn failure(tx_hash: TxHash, error: impl Into<String>) -> Self {
        Self { tx_hash, success: false, error: Some(error.into()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base64_roundtrip() {
        let original = Bytes::from(vec![0x01, 0x02, 0x03, 0xde, 0xad, 0xbe, 0xef]);

        #[derive(Serialize, Deserialize)]
        struct Wrapper {
            #[serde(with = "base64_bytes")]
            data: Bytes,
        }

        let wrapper = Wrapper { data: original.clone() };
        let json = serde_json::to_string(&wrapper).unwrap();

        // base64 of [1,2,3,0xde,0xad,0xbe,0xef]
        assert!(json.contains("AQIDrevv"));
        assert!(!json.contains("0x"));

        let decoded: Wrapper = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.data, original);
    }
}
