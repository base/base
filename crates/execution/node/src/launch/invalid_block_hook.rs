//! Invalid block hook helpers for the node builder.

use base_execution_evm_blocks::BaseEvmConfig;
use base_execution_rpc_handlers::EthApiClient;
use eyre::OptionExt;
use reth_engine_primitives::{InvalidBlockHook, InvalidBlockHooks, NoopInvalidBlockHook};
use reth_invalid_block_hooks::InvalidBlockWitnessHook;
use reth_node_core::{
    args::InvalidBlockHookType,
    dirs::{ChainPath, DataDirPath},
    node_config::NodeConfig,
};

/// Constructs the configured invalid block diagnostics during node startup.
#[derive(Debug)]
pub struct InvalidBlockHookBuilder;

impl InvalidBlockHookBuilder {
    /// Creates an invalid block hook based on the node configuration.
    ///
    /// This function constructs the appropriate [`InvalidBlockHook`] based on the debug
    /// configuration in the node config. It supports:
    /// - Witness hooks for capturing block witness data
    /// - Healthy node verification via RPC
    ///
    /// # Arguments
    /// * `config` - The node configuration containing debug settings
    /// * `data_dir` - The data directory for storing hook outputs
    /// * `provider` - The blockchain database provider
    /// * `evm_config` - The EVM configuration
    /// * `chain_id` - The chain ID for verification
    pub async fn build<P>(
        config: &NodeConfig,
        data_dir: &ChainPath<DataDirPath>,
        provider: P,
        evm_config: BaseEvmConfig,
        chain_id: u64,
    ) -> eyre::Result<Box<dyn InvalidBlockHook>>
    where
        P: base_execution_state_provider::StateProviderFactory
            + base_execution_state_provider::ChainSpecProvider
            + Clone
            + Send
            + Sync
            + 'static,
    {
        let Some(ref hook) = config.debug.invalid_block_hook else {
            return Ok(Box::new(NoopInvalidBlockHook::default()));
        };

        let healthy_node_rpc_client = Self::healthy_node_client(config, chain_id).await?;

        let output_directory = data_dir.invalid_block_hooks();
        let hooks = hook
            .iter()
            .copied()
            .map(|hook| {
                let output_directory = output_directory.join(hook.to_string());
                std::fs::create_dir_all(&output_directory)?;

                Ok(match hook {
                    InvalidBlockHookType::Witness => Box::new(InvalidBlockWitnessHook::new(
                        provider.clone(),
                        evm_config.clone(),
                        output_directory,
                        healthy_node_rpc_client.clone(),
                    )),
                    InvalidBlockHookType::PreState | InvalidBlockHookType::Opcode => {
                        eyre::bail!("invalid block hook {hook:?} is not implemented yet");
                    }
                } as Box<dyn InvalidBlockHook>)
            })
            .collect::<Result<_, _>>()?;

        Ok(Box::new(InvalidBlockHooks(hooks)))
    }

    /// Returns an RPC client for the healthy node, if configured in the node config.
    pub async fn healthy_node_client(
        config: &NodeConfig,
        chain_id: u64,
    ) -> eyre::Result<Option<jsonrpsee::http_client::HttpClient>> {
        let Some(url) = config.debug.healthy_node_rpc_url.as_ref() else {
            return Ok(None);
        };

        let client = jsonrpsee::http_client::HttpClientBuilder::default().build(url)?;

        // Verify that the healthy node is running the same chain as the current node.
        let healthy_chain_id = EthApiClient::chain_id(&client)
            .await?
            .ok_or_eyre("healthy node rpc client didn't return a chain id")?;

        if healthy_chain_id.to::<u64>() != chain_id {
            eyre::bail!("Invalid chain ID. Expected {}, got {}", chain_id, healthy_chain_id);
        }

        Ok(Some(client))
    }
}
