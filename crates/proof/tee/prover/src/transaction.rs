use alloy_eips::{Typed2718, eip2718::Encodable2718};
use alloy_primitives::Bytes;
use base_alloy_rpc_types::Transaction as OpTransaction;
use base_proof_rpc::OpBlock;

use crate::TeeProverError;

/// Deposit transaction type identifier (EIP-2718 type byte for OP deposits).
pub const DEPOSIT_TX_TYPE: u8 = 0x7E;

/// Transaction serialization utilities for TEE proving.
#[derive(Debug)]
pub struct TransactionSerializer;

impl TransactionSerializer {
    /// Serializes an RPC transaction to EIP-2718 encoded bytes.
    ///
    /// Extracts the `OpTxEnvelope` from the RPC transaction and encodes it.
    /// This handles both standard Ethereum transactions and OP Stack deposit
    /// transactions (type `0x7E`).
    pub fn serialize_rpc_transaction(tx: &OpTransaction) -> Result<Bytes, TeeProverError> {
        let mut buf = Vec::new();
        tx.as_ref().encode_2718(&mut buf);
        Ok(Bytes::from(buf))
    }

    /// Serializes all transactions in a block to EIP-2718 encoded bytes.
    ///
    /// When `include_deposits` is `false`, deposit transactions (type `0x7E`)
    /// are filtered out.
    pub fn serialize_block_transactions(
        block: &OpBlock,
        include_deposits: bool,
    ) -> Result<Vec<Bytes>, TeeProverError> {
        block
            .transactions
            .txns()
            .filter(|tx| include_deposits || !Self::is_deposit_tx(tx))
            .map(Self::serialize_rpc_transaction)
            .collect()
    }

    /// Returns `true` if the transaction is a deposit (type `0x7E`).
    pub fn is_deposit_tx(tx: &OpTransaction) -> bool {
        tx.ty() == DEPOSIT_TX_TYPE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deposit_tx_type_constant() {
        assert_eq!(DEPOSIT_TX_TYPE, 0x7E);
        assert_eq!(DEPOSIT_TX_TYPE, 126);
    }
}
