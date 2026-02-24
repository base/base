//! EVM compatibility implementations for base-alloy consensus types.
//!
//! Provides [`FromRecoveredTx`] and [`FromTxWithEncoded`] impls for
//! [`OpTxEnvelope`] and [`TxDeposit`].

use alloy_eips::{Encodable2718, Typed2718};
use alloy_evm::{FromRecoveredTx, FromTxWithEncoded};
use alloy_primitives::{Address, Bytes};
use op_revm::{OpTransaction, transaction::deposit::DepositTransactionParts};
use revm::context::TxEnv;

use crate::{OpTxEnvelope, TxDeposit};

// ---------------------------------------------------------------------------
// FromRecoveredTx / FromTxWithEncoded – OpTxEnvelope -> TxEnv
// ---------------------------------------------------------------------------

impl FromRecoveredTx<OpTxEnvelope> for TxEnv {
    fn from_recovered_tx(tx: &OpTxEnvelope, caller: Address) -> Self {
        match tx {
            OpTxEnvelope::Legacy(tx) => Self::from_recovered_tx(tx.tx(), caller),
            OpTxEnvelope::Eip1559(tx) => Self::from_recovered_tx(tx.tx(), caller),
            OpTxEnvelope::Eip2930(tx) => Self::from_recovered_tx(tx.tx(), caller),
            OpTxEnvelope::Eip7702(tx) => Self::from_recovered_tx(tx.tx(), caller),
            OpTxEnvelope::Deposit(tx) => Self::from_recovered_tx(tx.inner(), caller),
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

impl FromTxWithEncoded<OpTxEnvelope> for TxEnv {
    fn from_encoded_tx(tx: &OpTxEnvelope, caller: Address, _encoded: Bytes) -> Self {
        Self::from_recovered_tx(tx, caller)
    }
}

// ---------------------------------------------------------------------------
// FromRecoveredTx / FromTxWithEncoded – OpTxEnvelope -> OpTransaction<TxEnv>
// ---------------------------------------------------------------------------

impl FromRecoveredTx<OpTxEnvelope> for OpTransaction<TxEnv> {
    fn from_recovered_tx(tx: &OpTxEnvelope, sender: Address) -> Self {
        let encoded = tx.encoded_2718();
        Self::from_encoded_tx(tx, sender, encoded.into())
    }
}

impl FromTxWithEncoded<OpTxEnvelope> for OpTransaction<TxEnv> {
    fn from_encoded_tx(tx: &OpTxEnvelope, caller: Address, encoded: Bytes) -> Self {
        match tx {
            OpTxEnvelope::Legacy(tx) => Self::from_encoded_tx(tx, caller, encoded),
            OpTxEnvelope::Eip1559(tx) => Self::from_encoded_tx(tx, caller, encoded),
            OpTxEnvelope::Eip2930(tx) => Self::from_encoded_tx(tx, caller, encoded),
            OpTxEnvelope::Eip7702(tx) => Self::from_encoded_tx(tx, caller, encoded),
            OpTxEnvelope::Deposit(tx) => Self::from_encoded_tx(tx.inner(), caller, encoded),
        }
    }
}

// ---------------------------------------------------------------------------
// TxDeposit -> OpTransaction<TxEnv>
// ---------------------------------------------------------------------------

impl FromRecoveredTx<TxDeposit> for OpTransaction<TxEnv> {
    fn from_recovered_tx(tx: &TxDeposit, sender: Address) -> Self {
        let encoded = tx.encoded_2718();
        Self::from_encoded_tx(tx, sender, encoded.into())
    }
}

impl FromTxWithEncoded<TxDeposit> for OpTransaction<TxEnv> {
    fn from_encoded_tx(tx: &TxDeposit, caller: Address, encoded: Bytes) -> Self {
        let base = TxEnv::from_recovered_tx(tx, caller);
        let deposit = DepositTransactionParts {
            source_hash: tx.source_hash,
            mint: Some(tx.mint),
            is_system_transaction: tx.is_system_transaction,
        };
        Self { base, enveloped_tx: Some(encoded), deposit }
    }
}
