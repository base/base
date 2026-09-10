//! Concrete Base transaction, receipt, log, and header conversion.

use alloy_primitives::{Signature, U256};
use base_common_types_chain::{
    BaseTxEnvelope, SignableTransaction, error::ValueError, transaction::Recovered,
};
use base_common_types_rpc::{BaseTransactionRequest, Header, TransactionInfo};

use crate::{
    TransactionConversionError, TryIntoTxEnv,
    eth_services::{BaseEthApiError, BaseTimeCache},
};

/// Converts Base RPC data using the provider and shared BaseTime cache.
#[derive(Clone)]
pub struct BaseRpcConverter {
    /// Provider used to load receipts and block metadata.
    pub provider: base_execution_state_provider::BlockchainProvider,
    /// Shared block timestamp cache.
    pub base_time: BaseTimeCache,
}

impl std::fmt::Debug for BaseRpcConverter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BaseRpcConverter").finish_non_exhaustive()
    }
}

impl BaseRpcConverter {
    /// Creates conversion state shared by all Base RPC endpoints.
    pub fn new(
        provider: base_execution_state_provider::BlockchainProvider,
        base_time: BaseTimeCache,
    ) -> Self {
        Self { provider, base_time }
    }
}

impl BaseRpcConverter {
    /// Converts a pending transaction without mined block metadata.
    pub fn fill_pending(
        &self,
        tx: Recovered<BaseTxEnvelope>,
    ) -> Result<base_common_types_rpc::BaseTransaction, BaseEthApiError> {
        self.fill(tx, TransactionInfo::default())
    }

    /// Converts a mined transaction with its deposit and block metadata.
    pub fn fill(
        &self,
        tx: Recovered<BaseTxEnvelope>,
        tx_info: TransactionInfo,
    ) -> Result<base_common_types_rpc::BaseTransaction, BaseEthApiError> {
        let (tx, signer) = tx.into_parts();
        let tx_info = self.try_map(&tx, tx_info)?;

        Ok(base_common_types_rpc::BaseTransaction::from_transaction(
            Recovered::new_unchecked(tx, signer),
            tx_info,
        ))
    }

    /// Builds the Base transaction used by eth_simulateV1.
    pub fn build_simulate_v1_transaction(
        &self,
        request: BaseTransactionRequest,
    ) -> Result<BaseTxEnvelope, BaseEthApiError> {
        let tx = request.build_typed_tx().map_err(|request| {
            TransactionConversionError::FromTxReq(
                ValueError::new(request, "Required fields missing").to_string(),
            )
        })?;
        Ok(tx.into_signed(Signature::new(U256::ZERO, U256::ZERO, false)).into())
    }

    /// Builds the Base execution environment for an RPC transaction request.
    pub fn tx_env(
        &self,
        request: BaseTransactionRequest,
        evm_env: &base_execution_evm_runtime::EvmEnv,
    ) -> Result<base_execution_evm_runtime::BaseTransaction, BaseEthApiError> {
        request.try_into_tx_env(evm_env).map_err(Into::into)
    }

    /// Converts a Base consensus header to its RPC representation.
    pub fn convert_header(
        &self,
        header: base_common_types_chain::SealedHeader,
        block_size: usize,
    ) -> Result<Header, BaseEthApiError> {
        Ok(base_common_types_rpc::Header::from_consensus(
            header.into(),
            None,
            Some(U256::from(block_size)),
        ))
    }
}
