use alloy::{
    consensus::{Signed, TxEip1559, TxEip2930, TxEip4844, TxEip4844Variant, TxEnvelope, TxLegacy},
    rpc::types::eth::{AccessList, ConversionError, Signature},
    serde as alloy_serde,
};
use alloy_primitives::{Address, Bytes, B256, U128, U256, U64};
use serde::{Deserialize, Serialize};

use self::{request::TransactionRequest, tx_type::TxType};

pub mod receipt;
pub mod request;
pub mod tx_type;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[serde(rename_all = "camelCase")]
pub struct Transaction {
    /// Hash
    pub hash: B256,
    /// Nonce
    #[serde(with = "alloy_serde::num::u64_hex")]
    pub nonce: u64,
    /// Block hash
    pub block_hash: Option<B256>,
    /// Block number
    #[serde(with = "alloy_serde::num::u64_hex_opt")]
    pub block_number: Option<u64>,
    /// Transaction Index
    #[serde(with = "alloy_serde::num::u64_hex_opt")]
    pub transaction_index: Option<u64>,
    /// Sender
    pub from: Address,
    /// Recipient
    pub to: Option<Address>,
    /// Transferred value
    pub value: U256,
    /// Gas Price
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "alloy_serde::num::u128_hex_or_decimal_opt"
    )]
    pub gas_price: Option<u128>,
    /// Gas amount
    #[serde(with = "alloy_serde::num::u128_hex_or_decimal")]
    pub gas: u128,
    /// Max BaseFeePerGas the user is willing to pay.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "alloy_serde::num::u128_hex_or_decimal_opt"
    )]
    pub max_fee_per_gas: Option<u128>,
    /// The miner's tip.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "alloy_serde::num::u128_hex_or_decimal_opt"
    )]
    pub max_priority_fee_per_gas: Option<u128>,
    /// Data
    pub input: Bytes,
    /// All _flattened_ fields of the transaction signature.
    ///
    /// Note: this is an option so special transaction types without a signature (e.g. <https://github.com/ethereum-optimism/optimism/blob/0bf643c4147b43cd6f25a759d331ef3a2a61a2a3/specs/deposits.md#the-deposited-transaction-type>) can be supported.
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub signature: Option<Signature>,
    /// The chain id of the transaction, if any.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::u64_hex_opt")]
    pub chain_id: Option<u64>,
    /// EIP2930
    ///
    /// Pre-pay to warm storage access.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_list: Option<AccessList>,
    /// EIP2718
    ///
    /// Transaction type, Some(2) for EIP-1559 transaction,
    /// Some(1) for AccessList transaction, None for Legacy
    #[serde(
        default,
        rename = "type",
        skip_serializing_if = "Option::is_none",
        with = "alloy_serde::num::u8_hex_opt"
    )]
    pub transaction_type: Option<TxType>,
    /// The ETH value to mint on L2
    #[serde(rename = "mint", skip_serializing_if = "Option::is_none")]
    pub mint: Option<U128>,
    /// Hash that uniquely identifies the source of the deposit.
    #[serde(rename = "sourceHash", skip_serializing_if = "Option::is_none")]
    pub source_hash: Option<B256>,
    /// Field indicating whether the transaction is a system transaction, and therefore
    /// exempt from the L2 gas limit.
    #[serde(rename = "isSystemTx", skip_serializing_if = "Option::is_none")]
    pub is_system_tx: Option<bool>,
    /// Deposit receipt version for deposit transactions post-canyon
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deposit_receipt_version: Option<U64>,
}

impl Transaction {
    /// Converts [Transaction] into [TransactionRequest].
    ///
    /// During this conversion data for [TransactionRequest::sidecar] is not populated as it is not
    /// part of [Transaction].
    pub fn into_request(self) -> TransactionRequest {
        let gas_price = match (self.gas_price, self.max_fee_per_gas) {
            (Some(gas_price), None) => Some(gas_price),
            // EIP-1559 transactions include deprecated `gasPrice` field displaying gas used by
            // transaction.
            // Setting this field for resulted tx request will result in it being invalid
            (_, Some(_)) => None,
            // unreachable
            (None, None) => None,
        };
        TransactionRequest {
            from: Some(self.from),
            to: self.to,
            gas: Some(self.gas),
            gas_price,
            value: Some(self.value),
            input: self.input.into(),
            nonce: Some(self.nonce),
            chain_id: self.chain_id,
            access_list: self.access_list,
            transaction_type: self.transaction_type,
            max_fee_per_gas: self.max_fee_per_gas,
            max_priority_fee_per_gas: self.max_priority_fee_per_gas,
            max_fee_per_blob_gas: self.max_fee_per_blob_gas,
            blob_versioned_hashes: self.blob_versioned_hashes,
            sidecar: self.sidecar,
        }
    }
}

impl TryFrom<Transaction> for Signed<TxLegacy> {
    type Error = ConversionError;

    fn try_from(tx: Transaction) -> Result<Self, Self::Error> {
        let signature = tx.signature.ok_or(ConversionError::MissingSignature)?.try_into()?;

        let tx = TxLegacy {
            chain_id: tx.chain_id,
            nonce: tx.nonce,
            gas_price: tx.gas_price.ok_or(ConversionError::MissingGasPrice)?,
            gas_limit: tx.gas,
            to: tx.to.into(),
            value: tx.value,
            input: tx.input,
        };
        Ok(tx.into_signed(signature))
    }
}

impl TryFrom<Transaction> for Signed<TxEip1559> {
    type Error = ConversionError;

    fn try_from(tx: Transaction) -> Result<Self, Self::Error> {
        let signature = tx.signature.ok_or(ConversionError::MissingSignature)?.try_into()?;

        let tx = TxEip1559 {
            chain_id: tx.chain_id.ok_or(ConversionError::MissingChainId)?,
            nonce: tx.nonce,
            max_fee_per_gas: tx.max_fee_per_gas.ok_or(ConversionError::MissingMaxFeePerGas)?,
            max_priority_fee_per_gas: tx
                .max_priority_fee_per_gas
                .ok_or(ConversionError::MissingMaxPriorityFeePerGas)?,
            gas_limit: tx.gas,
            to: tx.to.into(),
            value: tx.value,
            input: tx.input,
            access_list: tx.access_list.unwrap_or_default(),
        };
        Ok(tx.into_signed(signature))
    }
}

impl TryFrom<Transaction> for Signed<TxEip2930> {
    type Error = ConversionError;

    fn try_from(tx: Transaction) -> Result<Self, Self::Error> {
        let signature = tx.signature.ok_or(ConversionError::MissingSignature)?.try_into()?;

        let tx = TxEip2930 {
            chain_id: tx.chain_id.ok_or(ConversionError::MissingChainId)?,
            nonce: tx.nonce,
            gas_price: tx.gas_price.ok_or(ConversionError::MissingGasPrice)?,
            gas_limit: tx.gas,
            to: tx.to.into(),
            value: tx.value,
            input: tx.input,
            access_list: tx.access_list.ok_or(ConversionError::MissingAccessList)?,
        };
        Ok(tx.into_signed(signature))
    }
}

impl TryFrom<Transaction> for Signed<TxEip4844> {
    type Error = ConversionError;

    fn try_from(tx: Transaction) -> Result<Self, Self::Error> {
        let signature = tx.signature.ok_or(ConversionError::MissingSignature)?.try_into()?;
        let tx = TxEip4844 {
            chain_id: tx.chain_id.ok_or(ConversionError::MissingChainId)?,
            nonce: tx.nonce,
            max_fee_per_gas: tx.max_fee_per_gas.ok_or(ConversionError::MissingMaxFeePerGas)?,
            max_priority_fee_per_gas: tx
                .max_priority_fee_per_gas
                .ok_or(ConversionError::MissingMaxPriorityFeePerGas)?,
            gas_limit: tx.gas,
            to: tx.to.ok_or(ConversionError::MissingTo)?,
            value: tx.value,
            input: tx.input,
            access_list: tx.access_list.unwrap_or_default(),
            blob_versioned_hashes: tx.blob_versioned_hashes.unwrap_or_default(),
            max_fee_per_blob_gas: tx
                .max_fee_per_blob_gas
                .ok_or(ConversionError::MissingMaxFeePerBlobGas)?,
        };
        Ok(tx.into_signed(signature))
    }
}

impl TryFrom<Transaction> for Signed<TxEip4844Variant> {
    type Error = ConversionError;

    fn try_from(tx: Transaction) -> Result<Self, Self::Error> {
        let tx: Signed<TxEip4844> = tx.try_into()?;
        let (inner, signature, _) = tx.into_parts();
        let tx = TxEip4844Variant::TxEip4844(inner);

        Ok(tx.into_signed(signature))
    }
}

impl TryFrom<Transaction> for TxEnvelope {
    // TODO: When the TxEnvelope is implemented for op-consensus, import it from there. This
    // envelope doesn't handle DEPOSIT
    type Error = ConversionError;

    fn try_from(tx: Transaction) -> Result<Self, Self::Error> {
        match tx.transaction_type.unwrap_or_default().try_into()? {
            TxType::Legacy => Ok(Self::Legacy(tx.try_into()?)),
            TxType::Eip1559 => Ok(Self::Eip1559(tx.try_into()?)),
            TxType::Eip2930 => Ok(Self::Eip2930(tx.try_into()?)),
            TxType::Eip4844 => Ok(Self::Eip4844(tx.try_into()?)),
            TxType::Deposit => Ok(Self::Deposit(tx.try_into()?)),
        }
    }
}
