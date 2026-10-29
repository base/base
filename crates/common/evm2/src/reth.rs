//! Reth execution-trait implementations for the Base EVM2 types.

use alloy_consensus::transaction::Recovered;
use alloy_eips::{Encodable2718, eip2718::WithEncoded, eip2930::AccessList};
use base_common_consensus::{BaseTransactionSigned, Eip8130Signed};
use evm2::TxResultWithState;
use reth_evm::{BlockTransactionResult, FromRecoveredTx, FromTxWithEncoded, TransactionEnvMut};

use crate::{BaseEvmTypes, BaseExecutedTx, BaseTxEnvelope};

impl FromRecoveredTx<BaseTransactionSigned> for BaseTxEnvelope {
    fn from_recovered_tx(tx: Recovered<BaseTransactionSigned>) -> Self {
        let encoded = tx.inner().encoded_2718().into();
        Self::from_signed(tx.into_inner(), encoded)
    }
}

impl FromTxWithEncoded<BaseTransactionSigned> for BaseTxEnvelope {
    fn from_tx_with_encoded(tx: WithEncoded<Recovered<BaseTransactionSigned>>) -> Self {
        let (encoded, tx) = tx.split();
        Self::from_signed(tx.into_inner(), encoded)
    }
}

/// Simulation overrides (for example `eth_estimateGas` and `eth_createAccessList`) keep the
/// transaction's original EIP-2718 bytes, so the L1 data fee is priced over the transaction as
/// signed, matching the revm reference.
impl TransactionEnvMut for BaseTxEnvelope {
    fn set_gas_limit(&mut self, gas_limit: u64) {
        match self {
            Self::Deposit(tx) => tx.gas_limit = gas_limit,
            Self::Standard { tx, .. } => tx.set_gas_limit(gas_limit),
            Self::Eip8130(signed) => {
                let mut tx = signed.tx().clone();
                tx.gas_limit = gas_limit;
                *signed = Eip8130Signed::new(
                    tx,
                    signed.sender_auth().clone(),
                    signed.payer_auth().clone(),
                );
            }
        }
    }

    /// Deposits and EIP-8130 transactions carry no access list, so only standard transactions
    /// take one.
    fn set_access_list(&mut self, access_list: AccessList) {
        if let Self::Standard { tx, .. } = self {
            tx.set_access_list(access_list);
        }
    }
}

impl BlockTransactionResult<BaseEvmTypes> for BaseExecutedTx {
    fn result(&self) -> &TxResultWithState<BaseEvmTypes> {
        &self.result
    }
}
