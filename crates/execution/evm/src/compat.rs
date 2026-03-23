//! Compatibility trait implementations for integration with alloy-evm and reth-evm.

#[cfg(feature = "alloy")]
mod alloy_compat {
    use alloy_evm::{InvalidTxError, tx::IntoTxEnv};
    use revm::context_interface::{result::InvalidTransaction, transaction::Transaction};

    use crate::{OpTransaction, OpTransactionError};

    impl<T> IntoTxEnv<Self> for OpTransaction<T>
    where
        T: Transaction,
    {
        fn into_tx_env(self) -> Self {
            self
        }
    }

    impl InvalidTxError for OpTransactionError {
        fn as_invalid_tx_err(&self) -> Option<&InvalidTransaction> {
            match self {
                Self::Base(tx) => Some(tx),
                _ => None,
            }
        }
    }
}

#[cfg(feature = "alloy")]
mod consensus_compat {
    use alloy_eips::Encodable2718;
    use alloy_evm::{FromRecoveredTx, FromTxWithEncoded};
    use alloy_primitives::{Address, Bytes};
    use base_alloy_consensus::{OpTxEnvelope, TxDeposit};
    use revm::context::TxEnv;

    use crate::{DepositTransactionParts, OpTransaction};

    impl FromRecoveredTx<OpTxEnvelope> for OpTransaction<TxEnv> {
        fn from_recovered_tx(tx: &OpTxEnvelope, sender: Address) -> Self {
            let encoded = tx.encoded_2718();
            Self::from_encoded_tx(tx, sender, encoded.into())
        }
    }

    impl FromTxWithEncoded<OpTxEnvelope> for OpTransaction<TxEnv> {
        fn from_encoded_tx(tx: &OpTxEnvelope, caller: Address, encoded: Bytes) -> Self {
            match tx {
                OpTxEnvelope::Legacy(tx) => Self {
                    base: TxEnv::from_recovered_tx(tx.tx(), caller),
                    enveloped_tx: Some(encoded),
                    deposit: Default::default(),
                },
                OpTxEnvelope::Eip1559(tx) => Self {
                    base: TxEnv::from_recovered_tx(tx.tx(), caller),
                    enveloped_tx: Some(encoded),
                    deposit: Default::default(),
                },
                OpTxEnvelope::Eip2930(tx) => Self {
                    base: TxEnv::from_recovered_tx(tx.tx(), caller),
                    enveloped_tx: Some(encoded),
                    deposit: Default::default(),
                },
                OpTxEnvelope::Eip7702(tx) => Self {
                    base: TxEnv::from_recovered_tx(tx.tx(), caller),
                    enveloped_tx: Some(encoded),
                    deposit: Default::default(),
                },
                OpTxEnvelope::Deposit(tx) => Self::from_encoded_tx(tx.inner(), caller, encoded),
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
}

#[cfg(feature = "reth")]
mod reth_compat {
    use revm::context_interface::transaction::AccessList;

    use crate::OpTransaction;

    impl<T: reth_evm::TransactionEnv> reth_evm::TransactionEnv for OpTransaction<T> {
        fn set_gas_limit(&mut self, gas_limit: u64) {
            self.base.set_gas_limit(gas_limit);
        }

        fn nonce(&self) -> u64 {
            reth_evm::TransactionEnv::nonce(&self.base)
        }

        fn set_nonce(&mut self, nonce: u64) {
            self.base.set_nonce(nonce);
        }

        fn set_access_list(&mut self, access_list: AccessList) {
            self.base.set_access_list(access_list);
        }
    }
}

#[cfg(feature = "rpc")]
mod rpc_compat {
    use alloy_evm::{
        EvmEnv,
        env::BlockEnvironment,
        rpc::{EthTxEnvError, TryIntoTxEnv},
    };
    use alloy_primitives::Bytes;
    use base_alloy_rpc_types::OpTransactionRequest;
    use revm::context::TxEnv;

    use crate::OpTransaction;

    impl<Block: BlockEnvironment> TryIntoTxEnv<OpTransaction<TxEnv>, Block> for OpTransactionRequest {
        type Err = EthTxEnvError;

        fn try_into_tx_env<Spec>(
            self,
            evm_env: &EvmEnv<Spec, Block>,
        ) -> Result<OpTransaction<TxEnv>, Self::Err> {
            Ok(OpTransaction {
                base: self.as_ref().clone().try_into_tx_env(evm_env)?,
                enveloped_tx: Some(Bytes::new()),
                deposit: Default::default(),
            })
        }
    }
}
