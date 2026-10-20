use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::TransactionRequest;
use alloy_transport::TransportError;
use base_tx_manager::{FeeCalculator, TxCandidate};
use thiserror::Error;

use crate::{WithdrawalRpc, WithdrawalRpcTransport, WithdrawerConfig};

/// An unsigned L1 candidate and its dry-run maximum gas cost.
#[derive(Debug, Clone)]
pub struct PreparedWithdrawalTransaction {
    /// Candidate built without accessing signing material.
    pub candidate: TxCandidate,
    /// Estimated maximum fee per gas in wei.
    pub max_fee_per_gas: u128,
}

/// L1 transaction preparation failed.
#[derive(Debug, Error)]
pub enum WithdrawalTransactionError {
    /// A provider request failed after credential-bearing details were removed.
    #[error("transaction preparation RPC request failed: {0}")]
    Request(#[source] TransportError),
    /// The preparation deadline elapsed.
    #[error("transaction preparation timed out")]
    Timeout(#[source] tokio::time::error::Elapsed),
    /// The latest L1 block is missing.
    #[error("latest L1 block is unavailable")]
    MissingLatestBlock,
    /// The L1 block cannot support an EIP-1559 fee estimate.
    #[error("latest L1 block has no base fee")]
    MissingBaseFee,
    /// The L1 RPC serves a different chain than the selected network.
    #[error("L1 RPC reports chain ID {actual}, but the network expects {expected}")]
    L1ChainMismatch {
        /// Expected chain ID.
        expected: u64,
        /// Reported chain ID.
        actual: u64,
    },
}

impl From<TransportError> for WithdrawalTransactionError {
    fn from(error: TransportError) -> Self {
        Self::Request(WithdrawalRpcTransport::redact_diagnostic(error))
    }
}

impl PreparedWithdrawalTransaction {
    /// Estimates a candidate without constructing a manager or accessing signing material.
    pub async fn prepare(
        config: &WithdrawerConfig,
        rpc: &WithdrawalRpc,
        sender: Address,
        target: Address,
        input: Bytes,
    ) -> Result<Self, WithdrawalTransactionError> {
        tokio::time::timeout(rpc.timeout, async {
            let chain_id = rpc.l1.get_chain_id().await?;
            if let Some(expected) = config.network.l1_chain_id
                && expected != chain_id
            {
                return Err(WithdrawalTransactionError::L1ChainMismatch {
                    expected,
                    actual: chain_id,
                });
            }
            let tip =
                rpc.l1.get_max_priority_fee_per_gas().await?.max(config.tx_manager.min_tip_cap);
            let block = rpc
                .l1
                .get_block_by_number(BlockNumberOrTag::Latest)
                .await?
                .ok_or(WithdrawalTransactionError::MissingLatestBlock)?;
            let base_fee = u128::from(
                block.header.base_fee_per_gas.ok_or(WithdrawalTransactionError::MissingBaseFee)?,
            )
            .max(config.tx_manager.min_basefee);
            let max_fee_per_gas = FeeCalculator::calc_gas_fee_cap(base_fee, tip);
            let request = TransactionRequest {
                from: Some(sender),
                to: Some(target.into()),
                input: input.clone().into(),
                value: Some(U256::ZERO),
                chain_id: Some(chain_id),
                max_fee_per_gas: Some(max_fee_per_gas),
                max_priority_fee_per_gas: Some(tip),
                ..Default::default()
            };
            let gas_limit =
                rpc.l1.estimate_gas(request).await?.max(config.gas_limit.unwrap_or_default());
            Ok(Self {
                candidate: TxCandidate {
                    to: Some(target),
                    tx_data: input,
                    gas_limit,
                    ..Default::default()
                },
                max_fee_per_gas,
            })
        })
        .await
        .map_err(WithdrawalTransactionError::Timeout)?
    }

    /// Maximum gas cost in wei at the estimated fee.
    pub fn maximum_gas_cost(&self) -> U256 {
        U256::from(self.candidate.gas_limit) * U256::from(self.max_fee_per_gas)
    }
}
