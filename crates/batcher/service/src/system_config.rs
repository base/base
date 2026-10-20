//! Startup read of the batcher address authorized by the L1 `SystemConfig` contract.

use alloy_primitives::{Address, B256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_sol_types::{SolCall, sol};

sol! {
    /// ABI of the L1 `SystemConfig` contract, reduced to what the batcher reads.
    interface ISystemConfig {
        /// The authorized batcher address, left-padded to 32 bytes.
        function batcherHash() external view returns (bytes32);
    }
}

/// Reads the batcher hash that derivation accepts batches from.
#[derive(Debug)]
pub struct SystemConfigBatcher;

impl SystemConfigBatcher {
    /// Returns the `batcherHash` stored in the L1 `SystemConfig` contract at `system_config`,
    /// as of the latest L1 block.
    pub async fn fetch(l1_provider: &RootProvider, system_config: Address) -> eyre::Result<B256> {
        let call = TransactionRequest::default()
            .to(system_config)
            .input(ISystemConfig::batcherHashCall {}.abi_encode().into());
        let output = l1_provider
            .call(call)
            .latest()
            .await
            .map_err(|e| eyre::eyre!("SystemConfig.batcherHash call failed: {e}"))?;
        Ok(ISystemConfig::batcherHashCall::abi_decode_returns(&output)?)
    }
}
