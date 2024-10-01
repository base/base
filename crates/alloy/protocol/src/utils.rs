//! Utility methods used by protocol types.

use crate::{block_info::DecodeError, L1BlockInfoBedrock, L1BlockInfoEcotone, L1BlockInfoTx};
use alloy_primitives::B256;
use op_alloy_consensus::{OpBlock, OpTxEnvelope};
use op_alloy_genesis::{RollupConfig, SystemConfig};

/// Returns if the given `value` is a deposit transaction.
pub fn starts_with_2718_deposit<B>(value: &B) -> bool
where
    B: AsRef<[u8]>,
{
    value.as_ref().first() == Some(&0x7E)
}

/// An error encountered during [OpBlock] conversion.
#[derive(Debug, derive_more::Display)]
pub enum OpBlockConversionError {
    /// Invalid genesis hash.
    #[display("Invalid genesis hash. Expected {_0}, got {_1}")]
    InvalidGenesisHash(B256, B256),
    /// Invalid transaction type.
    #[display("First payload transaction has unexpected type: {_0}")]
    InvalidTxType(u8),
    /// L1 Info error
    #[display("Failed to decode L1 info: {_0}")]
    L1InfoError(DecodeError),
    /// Missing system config in genesis block.
    #[display("Missing system config in genesis block")]
    MissingSystemConfigGenesis,
    /// Empty transactions.
    #[display("Empty transactions in payload. Block hash: {_0}")]
    EmptyTransactions(B256),
}

impl core::error::Error for OpBlockConversionError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::L1InfoError(err) => Some(err),
            _ => None,
        }
    }
}

impl From<DecodeError> for OpBlockConversionError {
    fn from(e: DecodeError) -> Self {
        Self::L1InfoError(e)
    }
}

/// Converts the [OpBlock] to a partial [SystemConfig].
pub fn to_system_config(
    block: &OpBlock,
    rollup_config: &RollupConfig,
) -> Result<SystemConfig, OpBlockConversionError> {
    if block.header.number == rollup_config.genesis.l2.number {
        if block.header.hash_slow() != rollup_config.genesis.l2.hash {
            return Err(OpBlockConversionError::InvalidGenesisHash(
                rollup_config.genesis.l2.hash,
                block.header.hash_slow(),
            ));
        }
        return rollup_config
            .genesis
            .system_config
            .ok_or(OpBlockConversionError::MissingSystemConfigGenesis);
    }

    if block.body.transactions.is_empty() {
        return Err(OpBlockConversionError::EmptyTransactions(block.header.hash_slow()));
    }
    let OpTxEnvelope::Deposit(ref tx) = block.body.transactions[0] else {
        return Err(OpBlockConversionError::InvalidTxType(
            block.body.transactions[0].tx_type() as u8
        ));
    };

    let l1_info = L1BlockInfoTx::decode_calldata(tx.input.as_ref())?;
    let l1_fee_scalar = match l1_info {
        L1BlockInfoTx::Bedrock(L1BlockInfoBedrock { l1_fee_scalar, .. }) => l1_fee_scalar,
        L1BlockInfoTx::Ecotone(L1BlockInfoEcotone {
            base_fee_scalar,
            blob_base_fee_scalar,
            ..
        }) => {
            // Translate Ecotone values back into encoded scalar if needed.
            // We do not know if it was derived from a v0 or v1 scalar,
            // but v1 is fine, a 0 blob base fee has the same effect.
            let mut buf = B256::ZERO;
            buf[0] = 0x01;
            buf[24..28].copy_from_slice(blob_base_fee_scalar.to_be_bytes().as_ref());
            buf[28..32].copy_from_slice(base_fee_scalar.to_be_bytes().as_ref());
            buf.into()
        }
    };

    Ok(SystemConfig {
        batcher_address: l1_info.batcher_address(),
        overhead: l1_info.l1_fee_overhead(),
        scalar: l1_fee_scalar,
        gas_limit: block.header.gas_limit,
        base_fee_scalar: None,
        blob_base_fee_scalar: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;

    #[test]
    fn test_is_deposit() {
        assert!(starts_with_2718_deposit(&[0x7E]));
        assert!(!starts_with_2718_deposit(&[]));
        assert!(!starts_with_2718_deposit(&[0x7F]));
    }

    #[test]
    fn test_bytes_deposit() {
        assert!(starts_with_2718_deposit(&Bytes::from_static(&[0x7E])));
        assert!(!starts_with_2718_deposit(&Bytes::from_static(&[])));
        assert!(!starts_with_2718_deposit(&Bytes::from_static(&[0x7F])));
    }
}
