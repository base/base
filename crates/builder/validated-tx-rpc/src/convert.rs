//! Conversion from [`ValidatedTransaction`] to pool-compatible transaction types.

use alloy_consensus::{SignableTransaction, Signed, TxEip1559, TxEip2930, TxLegacy};
use alloy_eips::eip2930::AccessList;
use alloy_primitives::{Signature, TxHash, TxKind};
use base_alloy_consensus::OpPooledTransaction;
use reth_primitives_traits::Recovered;
use thiserror::Error;

use crate::types::{TransactionSignature, ValidatedTransaction};

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

impl ValidatedTransaction {
    /// Converts this validated transaction into a recovered pooled transaction.
    ///
    /// The `from` field is trusted as the recovered sender address.
    pub fn into_recovered(self) -> Result<Recovered<OpPooledTransaction>, ConversionError> {
        let from = self.from;
        let signature = convert_signature(&self.signature);
        let tx_kind = self.to.map_or(TxKind::Create, TxKind::Call);
        let tx_type = self.tx_type;
        let access_list = self.access_list.clone().unwrap_or_default();

        let pooled = match tx_type {
            0 => self.into_legacy(signature, tx_kind)?,
            1 => self.into_eip2930(signature, tx_kind, access_list)?,
            2 => self.into_eip1559(signature, tx_kind, access_list)?,
            4 => {
                return Err(ConversionError::UnsupportedTxType(4));
            }
            other => return Err(ConversionError::UnsupportedTxType(other)),
        };

        Ok(Recovered::new_unchecked(pooled, from))
    }

    fn into_legacy(
        self,
        signature: Signature,
        tx_kind: TxKind,
    ) -> Result<OpPooledTransaction, ConversionError> {
        let gas_price = self.gas_price.ok_or(ConversionError::MissingGasPrice)?;

        let tx = TxLegacy {
            chain_id: self.chain_id,
            nonce: self.nonce,
            gas_price,
            gas_limit: self.gas_limit,
            to: tx_kind,
            value: self.value,
            input: self.input,
        };

        let signature_hash = tx.signature_hash();
        let signed = Signed::new_unchecked(tx, signature, signature_hash);
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

        let tx = TxEip2930 {
            chain_id,
            nonce: self.nonce,
            gas_price,
            gas_limit: self.gas_limit,
            to: tx_kind,
            value: self.value,
            input: self.input,
            access_list,
        };

        let signature_hash = tx.signature_hash();
        let signed = Signed::new_unchecked(tx, signature, signature_hash);
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

        let tx = TxEip1559 {
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

        let signature_hash = tx.signature_hash();
        let signed = Signed::new_unchecked(tx, signature, signature_hash);
        Ok(OpPooledTransaction::Eip1559(signed))
    }
}

const fn convert_signature(sig: &TransactionSignature) -> Signature {
    let y_parity = match sig.v {
        0 | 27 => false,
        1 | 28 => true,
        v => (v % 2) == 0,
    };

    Signature::new(sig.r, sig.s, y_parity)
}

/// Computes the transaction hash for a validated transaction.
///
/// This reconstructs the transaction just enough to compute the hash.
pub fn compute_tx_hash(tx: &ValidatedTransaction) -> Result<TxHash, ConversionError> {
    let tx_kind = tx.to.map_or(TxKind::Create, TxKind::Call);
    let access_list = tx.access_list.clone().unwrap_or_default();

    match tx.tx_type {
        0 => {
            let gas_price = tx.gas_price.ok_or(ConversionError::MissingGasPrice)?;
            let inner = TxLegacy {
                chain_id: tx.chain_id,
                nonce: tx.nonce,
                gas_price,
                gas_limit: tx.gas_limit,
                to: tx_kind,
                value: tx.value,
                input: tx.input.clone(),
            };
            let signature_hash = inner.signature_hash();
            Ok(signature_hash)
        }
        1 => {
            let chain_id = tx.chain_id.ok_or(ConversionError::MissingChainId)?;
            let gas_price = tx.gas_price.ok_or(ConversionError::MissingGasPrice)?;
            let inner = TxEip2930 {
                chain_id,
                nonce: tx.nonce,
                gas_price,
                gas_limit: tx.gas_limit,
                to: tx_kind,
                value: tx.value,
                input: tx.input.clone(),
                access_list,
            };
            let signature_hash = inner.signature_hash();
            Ok(signature_hash)
        }
        2 => {
            let chain_id = tx.chain_id.ok_or(ConversionError::MissingChainId)?;
            let max_fee_per_gas = tx.max_fee_per_gas.ok_or(ConversionError::MissingMaxFeePerGas)?;
            let max_priority_fee_per_gas =
                tx.max_priority_fee_per_gas.ok_or(ConversionError::MissingMaxPriorityFeePerGas)?;
            let inner = TxEip1559 {
                chain_id,
                nonce: tx.nonce,
                max_fee_per_gas,
                max_priority_fee_per_gas,
                gas_limit: tx.gas_limit,
                to: tx_kind,
                value: tx.value,
                input: tx.input.clone(),
                access_list,
            };
            let signature_hash = inner.signature_hash();
            Ok(signature_hash)
        }
        other => Err(ConversionError::UnsupportedTxType(other)),
    }
}
