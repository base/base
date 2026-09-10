use base_common_runtime::Runtime;

use crate::{BaseEthApi, EthConfig, EthFilter, EthPubSub};

/// Handlers for core, filter and pubsub `eth` namespace APIs.
#[derive(Debug, Clone)]
pub struct EthHandlers {
    /// Main `eth_` request handler
    pub api: BaseEthApi,
    /// Polling based filter handler available on all transports
    pub filter: EthFilter,
    /// Handler for subscriptions only available for transports that support it (ws, ipc)
    pub pubsub: EthPubSub,
}

impl EthHandlers {
    /// Returns a new instance with the additional handlers for the `eth` namespace.
    ///
    /// This will spawn all necessary tasks for the additional handlers.
    pub fn bootstrap(config: EthConfig, executor: Runtime, eth_api: BaseEthApi) -> Self {
        let filter = EthFilter::new(eth_api.clone(), config.filter_config(), executor.clone());

        let pubsub = EthPubSub::new(eth_api.clone(), executor);

        Self { api: eth_api, filter, pubsub }
    }
}
