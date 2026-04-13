//! Compatibility implementations bridging base-common-consensus transaction types
//! and the revm transaction environment.

use alloy_eips::Encodable2718;
use alloy_evm::{FromRecoveredTx, FromTxWithEncoded};
use alloy_primitives::{Address, Bytes};
use base_common_consensus::{BaseTxEnvelope, TxDeposit};
use revm::context::TxEnv;

use crate::{DepositTransactionParts, OpTransaction};

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
