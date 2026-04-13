//! EVM compatibility implementations for base-alloy consensus types.
//!
//! Provides [`FromRecoveredTx`] and [`FromTxWithEncoded`] impls for
//! [`BaseTxEnvelope`] and [`TxDeposit`].

use alloy_consensus::Typed2718;
use alloy_evm::{FromRecoveredTx, FromTxWithEncoded};
use alloy_primitives::{Address, Bytes};
use revm::context::TxEnv;

use crate::{BaseTxEnvelope, TxDeposit};

impl FromRecoveredTx<BaseTxEnvelope> for TxEnv {
    fn from_recovered_tx(tx: &BaseTxEnvelope, caller: Address) -> Self {
        match tx {
            BaseTxEnvelope::Legacy(tx) => Self::from_recovered_tx(tx.tx(), caller),
            BaseTxEnvelope::Eip1559(tx) => Self::from_recovered_tx(tx.tx(), caller),
            BaseTxEnvelope::Eip2930(tx) => Self::from_recovered_tx(tx.tx(), caller),
            BaseTxEnvelope::Eip7702(tx) => Self::from_recovered_tx(tx.tx(), caller),
            BaseTxEnvelope::Deposit(tx) => Self::from_recovered_tx(tx.inner(), caller),
        }
    }
}

impl FromRecoveredTx<TxDeposit> for TxEnv {
    fn from_recovered_tx(tx: &TxDeposit, caller: Address) -> Self {
        let TxDeposit {
            to,
            value,
            gas_limit,
            input,
            source_hash: _,
            from: _,
            mint: _,
            is_system_transaction: _,
        } = tx;
        Self {
            tx_type: tx.ty(),
            caller,
            gas_limit: *gas_limit,
            kind: *to,
            value: *value,
            data: input.clone(),
            ..Default::default()
        }
    }
}

impl FromTxWithEncoded<BaseTxEnvelope> for TxEnv {
    fn from_encoded_tx(tx: &BaseTxEnvelope, caller: Address, _encoded: Bytes) -> Self {
        Self::from_recovered_tx(tx, caller)
    }
}
