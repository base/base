//! Optimism specific types related to transactions.

use alloc::string::{String, ToString};
use alloy_consensus::{
    SignableTransaction, Transaction as ConsensusTransaction, TxEip1559, TxEip2930, TxEip7702,
    TxLegacy,
};
use alloy_eips::{eip2718::Eip2718Error, eip2930::AccessList, eip7702::SignedAuthorization};
use alloy_primitives::{Address, BlockHash, Bytes, ChainId, SignatureError, TxKind, B256, U256};
use alloy_serde::OtherFields;
use op_alloy_consensus::{OpTxEnvelope, OpTxType, TxDeposit};
use serde::{Deserialize, Serialize};

/// OP Transaction type
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[serde(rename_all = "camelCase")]
pub struct Transaction {
    /// Ethereum Transaction Types
    #[serde(flatten)]
    pub inner: alloy_rpc_types_eth::Transaction,
    /// The ETH value to mint on L2
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub mint: Option<u128>,
    /// Hash that uniquely identifies the source of the deposit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_hash: Option<B256>,
    /// Field indicating whether the transaction is a system transaction, and therefore
    /// exempt from the L2 gas limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_system_tx: Option<bool>,
    /// Deposit receipt version for deposit transactions post-canyon
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub deposit_receipt_version: Option<u64>,
}

impl ConsensusTransaction for Transaction {
    fn chain_id(&self) -> Option<ChainId> {
        self.inner.chain_id()
    }

    fn nonce(&self) -> u64 {
        self.inner.nonce()
    }

    fn gas_limit(&self) -> u64 {
        self.inner.gas_limit()
    }

    fn gas_price(&self) -> Option<u128> {
        self.inner.gas_price()
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.inner.max_fee_per_gas()
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.inner.max_priority_fee_per_gas()
    }

    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        self.inner.max_fee_per_blob_gas()
    }

    fn priority_fee_or_price(&self) -> u128 {
        self.inner.priority_fee_or_price()
    }

    fn to(&self) -> Option<Address> {
        self.inner.to()
    }

    fn kind(&self) -> TxKind {
        self.inner.kind()
    }

    fn value(&self) -> U256 {
        self.inner.value()
    }

    fn input(&self) -> &Bytes {
        self.inner.input()
    }

    fn ty(&self) -> u8 {
        self.inner.ty()
    }

    fn access_list(&self) -> Option<&AccessList> {
        self.inner.access_list()
    }

    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        self.inner.blob_versioned_hashes()
    }

    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        self.inner.authorization_list()
    }
}

impl alloy_network_primitives::TransactionResponse for Transaction {
    fn tx_hash(&self) -> alloy_primitives::TxHash {
        self.inner.tx_hash()
    }

    fn block_hash(&self) -> Option<BlockHash> {
        self.inner.block_hash()
    }

    fn block_number(&self) -> Option<u64> {
        self.inner.block_number()
    }

    fn transaction_index(&self) -> Option<u64> {
        self.inner.transaction_index()
    }

    fn from(&self) -> alloy_primitives::Address {
        self.inner.from()
    }

    fn to(&self) -> Option<alloy_primitives::Address> {
        ConsensusTransaction::to(&self.inner)
    }
}

/// Optimism specific transaction fields
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[doc(alias = "OptimismTxFields")]
#[serde(rename_all = "camelCase")]
pub struct OpTransactionFields {
    /// The ETH value to mint on L2
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub mint: Option<u128>,
    /// Hash that uniquely identifies the source of the deposit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_hash: Option<B256>,
    /// Field indicating whether the transaction is a system transaction, and therefore
    /// exempt from the L2 gas limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_system_tx: Option<bool>,
    /// Deposit receipt version for deposit transactions post-canyon
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub deposit_receipt_version: Option<u64>,
}

impl From<OpTransactionFields> for OtherFields {
    fn from(value: OpTransactionFields) -> Self {
        serde_json::to_value(value).unwrap().try_into().unwrap()
    }
}

/// Errors that can occur when converting a [Transaction] to an [OpTxEnvelope].
#[derive(Debug)]
pub enum TransactionConversionError {
    /// The transaction type is not supported.
    UnsupportedTransactionType(Eip2718Error),
    /// The transaction's signature could not be converted to the consensus type.
    SignatureConversionError(SignatureError),
    /// The transaction is missing a required field.
    MissingRequiredField(String),
    /// The transaction's signature is missing.
    MissingSignature,
}

impl core::fmt::Display for TransactionConversionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnsupportedTransactionType(e) => {
                write!(f, "Unsupported transaction type: {}", e)
            }
            Self::SignatureConversionError(e) => {
                write!(f, "Signature conversion error: {}", e)
            }
            Self::MissingRequiredField(field) => {
                write!(f, "Missing required field for conversion: {}", field)
            }
            Self::MissingSignature => {
                write!(f, "Missing signature")
            }
        }
    }
}

impl core::error::Error for TransactionConversionError {}

impl TryFrom<Transaction> for OpTxEnvelope {
    type Error = TransactionConversionError;

    fn try_from(value: Transaction) -> Result<Self, Self::Error> {
        /// Helper function to extract the signature from an RPC [Transaction].
        #[inline(always)]
        fn extract_signature(
            value: &Transaction,
        ) -> Result<alloy_primitives::Signature, TransactionConversionError> {
            value
                .inner
                .signature
                .ok_or(TransactionConversionError::MissingSignature)?
                .try_into()
                .map_err(TransactionConversionError::SignatureConversionError)
        }

        let ty = OpTxType::try_from(value.ty())
            .map_err(TransactionConversionError::UnsupportedTransactionType)?;
        match ty {
            OpTxType::Legacy => {
                let signature = extract_signature(&value)?;
                let legacy = TxLegacy {
                    chain_id: value.chain_id(),
                    nonce: value.nonce(),
                    gas_price: value.gas_price().unwrap_or_default(),
                    gas_limit: value.gas_limit(),
                    to: value.inner.to.map(TxKind::Call).unwrap_or(TxKind::Create),
                    value: value.value(),
                    input: value.inner.input,
                };
                Ok(Self::Legacy(legacy.into_signed(signature)))
            }
            OpTxType::Eip2930 => {
                let signature = extract_signature(&value)?;
                let access_list_tx = TxEip2930 {
                    chain_id: value.chain_id().ok_or_else(|| {
                        TransactionConversionError::MissingRequiredField("chain_id".to_string())
                    })?,
                    nonce: value.nonce(),
                    gas_price: value.gas_price().unwrap_or_default(),
                    gas_limit: value.gas_limit(),
                    to: value.inner.to.map(TxKind::Call).unwrap_or(TxKind::Create),
                    value: value.value(),
                    input: value.inner.input,
                    access_list: value.inner.access_list.unwrap_or_default(),
                };
                Ok(Self::Eip2930(access_list_tx.into_signed(signature)))
            }
            OpTxType::Eip1559 => {
                let signature = extract_signature(&value)?;
                let dynamic_fee_tx = TxEip1559 {
                    chain_id: value.chain_id().ok_or_else(|| {
                        TransactionConversionError::MissingRequiredField("chain_id".to_string())
                    })?,
                    nonce: value.nonce(),
                    gas_limit: value.gas_limit(),
                    to: value.inner.to.map(TxKind::Call).unwrap_or(TxKind::Create),
                    value: value.value(),
                    input: value.inner.input,
                    access_list: value.inner.access_list.unwrap_or_default(),
                    max_fee_per_gas: value.inner.max_fee_per_gas.unwrap_or_default(),
                    max_priority_fee_per_gas: value
                        .inner
                        .max_priority_fee_per_gas
                        .unwrap_or_default(),
                };
                Ok(Self::Eip1559(dynamic_fee_tx.into_signed(signature)))
            }
            OpTxType::Eip7702 => {
                let signature = extract_signature(&value)?;
                let set_code_tx = TxEip7702 {
                    chain_id: value.chain_id().ok_or_else(|| {
                        TransactionConversionError::MissingRequiredField("chain_id".to_string())
                    })?,
                    nonce: value.nonce(),
                    gas_limit: value.gas_limit(),
                    to: value.inner.to.ok_or_else(|| {
                        TransactionConversionError::MissingRequiredField("to".to_string())
                    })?,
                    value: value.value(),
                    input: value.inner.input,
                    access_list: value.inner.access_list.unwrap_or_default(),
                    max_fee_per_gas: value.inner.max_fee_per_gas.unwrap_or_default(),
                    max_priority_fee_per_gas: value
                        .inner
                        .max_priority_fee_per_gas
                        .unwrap_or_default(),
                    authorization_list: value.inner.authorization_list.unwrap_or_default(),
                };
                Ok(Self::Eip7702(set_code_tx.into_signed(signature)))
            }
            OpTxType::Deposit => {
                let deposit_tx = TxDeposit {
                    source_hash: value.source_hash.ok_or_else(|| {
                        TransactionConversionError::MissingRequiredField("source_hash".to_string())
                    })?,
                    from: value.inner.from,
                    to: value.inner.to.map(TxKind::Call).unwrap_or(TxKind::Create),
                    mint: value.mint,
                    value: value.inner.value,
                    gas_limit: value.gas_limit(),
                    is_system_transaction: value.is_system_tx.ok_or_else(|| {
                        TransactionConversionError::MissingRequiredField("is_system_tx".to_string())
                    })?,
                    input: value.inner.input,
                };
                Ok(Self::Deposit(deposit_tx))
            }
        }
    }
}
