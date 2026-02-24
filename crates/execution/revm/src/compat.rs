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
