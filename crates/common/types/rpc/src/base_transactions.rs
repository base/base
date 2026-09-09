use base_common_types_chain::{BaseTxEnvelope, BaseTypedTransaction, TxDeposit};

impl From<BaseTxEnvelope> for crate::TransactionRequest {
    fn from(value: BaseTxEnvelope) -> Self {
        match value {
            BaseTxEnvelope::Eip2930(tx) => tx.into_parts().0.into(),
            BaseTxEnvelope::Eip1559(tx) => tx.into_parts().0.into(),
            BaseTxEnvelope::Eip7702(tx) => tx.into_parts().0.into(),
            BaseTxEnvelope::Legacy(tx) => tx.into_parts().0.into(),
            BaseTxEnvelope::Eip8130(_) => unimplemented!(
                "BaseTxEnvelope::Eip8130 cannot be converted to an alloy TransactionRequest; AA transactions have no single sender/recipient/value to project into the legacy request shape"
            ),
            BaseTxEnvelope::Deposit(tx) => tx.into_inner().into(),
        }
    }
}

impl From<TxDeposit> for crate::TransactionRequest {
    fn from(tx: TxDeposit) -> Self {
        Self {
            from: Some(tx.from),
            to: Some(tx.to),
            value: Some(tx.value),
            gas: Some(tx.gas_limit),
            input: tx.input.into(),
            ..Default::default()
        }
    }
}

impl From<BaseTypedTransaction> for crate::TransactionRequest {
    fn from(tx: BaseTypedTransaction) -> Self {
        match tx {
            BaseTypedTransaction::Legacy(tx) => tx.into(),
            BaseTypedTransaction::Eip2930(tx) => tx.into(),
            BaseTypedTransaction::Eip1559(tx) => tx.into(),
            BaseTypedTransaction::Eip7702(tx) => tx.into(),
            BaseTypedTransaction::Eip8130(_) => unimplemented!(
                "BaseTypedTransaction::Eip8130 cannot be converted to an alloy TransactionRequest; AA transactions have no single sender/recipient/value to project into the legacy request shape"
            ),
            BaseTypedTransaction::Deposit(tx) => tx.into(),
        }
    }
}
