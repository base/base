use std::time::Duration;

use alloy_primitives::{Address, U256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::{BlockId, BlockNumberOrTag, TransactionRequest};
use alloy_sol_types::SolCall;
use async_trait::async_trait;
use base_tx_manager::{SendResponse, TxCandidate, TxManager};
use eyre::{OptionExt, WrapErr};
use tokio::time::timeout;

use crate::{FeeDisburser, FeeReader, FeeSender};

/// Latest-state RPC reader with bounded requests.
#[derive(Debug)]
pub struct AlloyFeeReader<P> {
    /// Provider for the job's chain.
    pub provider: P,
    /// Timeout for each precondition RPC request.
    pub network_timeout: Duration,
}

impl<P: Provider> AlloyFeeReader<P> {
    /// Reads a scheduling getter at the latest block.
    pub async fn read_uint<C: SolCall<Return = U256>>(
        &self,
        contract: Address,
        call: C,
    ) -> eyre::Result<U256> {
        let request = TransactionRequest::default().to(contract).input(call.abi_encode().into());
        let bytes =
            timeout(self.network_timeout, self.provider.call(request).block(BlockId::latest()))
                .await
                .wrap_err("contract read timed out")??;
        C::abi_decode_returns(&bytes).wrap_err("invalid scheduling getter result")
    }
}

#[async_trait]
impl<P: Provider> FeeReader for AlloyFeeReader<P> {
    async fn disbursement_window(&self, contract: Address) -> eyre::Result<(U256, U256)> {
        let interval =
            self.read_uint(contract, FeeDisburser::FEE_DISBURSEMENT_INTERVALCall {}).await?;
        let last = self.read_uint(contract, FeeDisburser::lastDisbursementTimeCall {}).await?;
        Ok((last, interval))
    }

    async fn latest_timestamp(&self) -> eyre::Result<u64> {
        let block = timeout(
            self.network_timeout,
            self.provider.get_block_by_number(BlockNumberOrTag::Latest),
        )
        .await
        .wrap_err("latest block read timed out")??
        .ok_or_eyre("latest block is unavailable")?;
        Ok(block.header.timestamp)
    }

    async fn balance(&self, contract: Address) -> eyre::Result<U256> {
        timeout(
            self.network_timeout,
            self.provider.get_balance(contract).block_id(BlockId::latest()),
        )
        .await
        .wrap_err("balance read timed out")?
        .wrap_err("failed to read contract balance")
    }
}

#[async_trait]
impl<T: TxManager> FeeSender for T {
    async fn send(&self, candidate: TxCandidate) -> SendResponse {
        TxManager::send(self, candidate).await
    }
}
