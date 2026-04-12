//! EVM compatibility implementations for base-alloy consensus types.
//!
//! Provides [`FromRecoveredTx`] and [`FromTxWithEncoded`] impls for
//! [`BaseTxEnvelope`] and [`TxDeposit`].

use alloy_eips::{Encodable2718, Typed2718};
use alloy_evm::{FromRecoveredTx, FromTxWithEncoded};
use alloy_primitives::{Address, Bytes};
use base_revm::{DepositTransactionParts, OpTransaction};
use revm::context::TxEnv;

use crate::{BaseTxEnvelope, TxDeposit};

// ---------------------------------------------------------------------------
// FromRecoveredTx / FromTxWithEncoded – BaseTxEnvelope -> TxEnv
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// FromRecoveredTx / FromTxWithEncoded – BaseTxEnvelope -> OpTransaction<TxEnv>
// ---------------------------------------------------------------------------

impl FromRecoveredTx<BaseTxEnvelope> for OpTransaction<TxEnv> {
    fn from_recovered_tx(tx: &BaseTxEnvelope, sender: Address) -> Self {
        let encoded = tx.encoded_2718();
        Self::from_encoded_tx(tx, sender, encoded.into())
    }
}

impl FromTxWithEncoded<BaseTxEnvelope> for OpTransaction<TxEnv> {
    fn from_encoded_tx(tx: &BaseTxEnvelope, caller: Address, encoded: Bytes) -> Self {
        match tx {
            BaseTxEnvelope::Legacy(tx) => Self {
                base: TxEnv::from_recovered_tx(tx.tx(), caller),
                enveloped_tx: Some(encoded),
                deposit: Default::default(),
            },
            BaseTxEnvelope::Eip1559(tx) => Self {
                base: TxEnv::from_recovered_tx(tx.tx(), caller),
                enveloped_tx: Some(encoded),
                deposit: Default::default(),
            },
            BaseTxEnvelope::Eip2930(tx) => Self {
                base: TxEnv::from_recovered_tx(tx.tx(), caller),
                enveloped_tx: Some(encoded),
                deposit: Default::default(),
            },
            BaseTxEnvelope::Eip7702(tx) => Self {
                base: TxEnv::from_recovered_tx(tx.tx(), caller),
                enveloped_tx: Some(encoded),
                deposit: Default::default(),
            },
            BaseTxEnvelope::Deposit(tx) => Self::from_encoded_tx(tx.inner(), caller, encoded),
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
