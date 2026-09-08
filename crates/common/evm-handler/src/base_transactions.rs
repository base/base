use alloy_eips::Typed2718;
use base_common_consensus::{BaseTxEnvelope, TxDeposit};
use base_evm_context::TxEnv;

use crate::{FromRecoveredTx, FromTxWithEncoded};

impl FromRecoveredTx<BaseTxEnvelope> for TxEnv {
    fn from_recovered_tx(tx: &BaseTxEnvelope, caller: alloy_primitives::Address) -> Self {
        match tx {
            BaseTxEnvelope::Legacy(tx) => Self::from_recovered_tx(tx.tx(), caller),
            BaseTxEnvelope::Eip1559(tx) => Self::from_recovered_tx(tx.tx(), caller),
            BaseTxEnvelope::Eip2930(tx) => Self::from_recovered_tx(tx.tx(), caller),
            BaseTxEnvelope::Eip7702(tx) => Self::from_recovered_tx(tx.tx(), caller),
            BaseTxEnvelope::Eip8130(_) => {
                unimplemented!("EIP-8130 AA transactions cannot be converted to TxEnv yet")
            }
            BaseTxEnvelope::Deposit(tx) => Self::from_recovered_tx(tx.inner(), caller),
        }
    }
}

impl FromTxWithEncoded<BaseTxEnvelope> for TxEnv {
    fn from_encoded_tx(
        tx: &BaseTxEnvelope,
        caller: alloy_primitives::Address,
        _encoded: alloy_primitives::Bytes,
    ) -> Self {
        Self::from_recovered_tx(tx, caller)
    }
}

impl FromRecoveredTx<TxDeposit> for TxEnv {
    fn from_recovered_tx(tx: &TxDeposit, caller: alloy_primitives::Address) -> Self {
        Self {
            tx_type: tx.ty(),
            caller,
            gas_limit: tx.gas_limit,
            kind: tx.to,
            value: tx.value,
            data: tx.input.clone(),
            ..Default::default()
        }
    }
}
