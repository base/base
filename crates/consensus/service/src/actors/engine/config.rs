use std::{sync::Arc, time::Duration};

use alloy_provider::RootProvider;
use alloy_rpc_types_engine::JwtSecret;
use alloy_transport::{TransportErrorKind, TransportResult};
use base_cli_utils::GenesisStartArgs;
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use base_consensus_engine::{BaseEngineClient, EngineClient, EngineClientBuilder, EngineEndpoint};
use url::Url;

use crate::NodeOperatingMode;

/// Configuration for the Engine Actor.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// The [`RollupConfig`].
    pub config: Arc<RollupConfig>,
    /// Explicit permission to launch a new chain from genesis.
    pub genesis_start: GenesisStartArgs,

    /// The engine endpoint.
    pub l2_endpoint: EngineEndpoint,
    /// The engine jwt secret.
    pub l2_jwt_secret: JwtSecret,

    /// The L1 rpc url.
    pub l1_url: Url,
    /// Request timeout for L1 execution JSON-RPC calls.
    pub l1_rpc_timeout: Duration,

    /// The mode of operation for the node.
    /// When the node is in sequencer mode, the engine actor will receive requests to build blocks
    /// from the sequencer actor.
    pub mode: NodeOperatingMode,
}

impl EngineConfig {
    /// Builds and returns the [`BaseEngineClient`].
    pub async fn build_engine_client(
        self,
    ) -> TransportResult<BaseEngineClient<RootProvider, RootProvider<Base>>> {
        EngineClientBuilder {
            l2: self.l2_endpoint,
            l2_jwt: self.l2_jwt_secret,
            l1_rpc: self.l1_url,
            l1_rpc_timeout: self.l1_rpc_timeout,
            cfg: self.config,
        }
        .build()
        .await
    }

    /// Checks the local EL head before any validator or sequencer bootstrap can run.
    pub async fn check_startup<E: EngineClient>(
        client: &E,
        genesis_start: GenesisStartArgs,
    ) -> TransportResult<()> {
        let head = client.latest_block_number().await?;
        genesis_start.check(head, client.cfg().genesis.l2.number).map_err(TransportErrorKind::non_retryable)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_eips::BlockNumberOrTag;
    use base_cli_utils::GenesisStartArgs;
    use base_common_genesis::RollupConfig;
    use base_common_rpc_types::BaseBlockResponse;

    use super::EngineConfig;
    use crate::test_utils::FakeEngineClient;

    /// Startup admission must use only the local height, regardless of node role or chain id.
    #[tokio::test]
    async fn checks_genesis_and_snapshot_heads() {
        for head in [0, 1, u64::MAX] {
            for allow_genesis_start in [false, true] {
                let client = FakeEngineClient::new(Arc::new(RollupConfig::default()));
                let mut block = BaseBlockResponse::default();
                block.header.number = head;
                client.handle().set_l2_block_by_label(BlockNumberOrTag::Latest, block);
                let result = EngineConfig::check_startup(&client, GenesisStartArgs { allow_genesis_start }).await;
                assert_eq!(result.is_ok(), head != 0 || allow_genesis_start,
                    "head {head} with launch flag {allow_genesis_start} must follow the startup policy");
                if head == 0 && !allow_genesis_start {
                    assert_eq!(result.expect_err("genesis must be refused").to_string(),
                        "refusing to start from genesis; restore a snapshot for an existing chain; use --allow-genesis-start only for launching a new chain",
                        "the startup error must preserve the exact recovery instructions");
                }
                assert_eq!(client.handle().call_count(), 1, "admission must issue exactly one local head query");
            }
        }
    }

    /// An unavailable local head is an error, never permission to start.
    #[tokio::test]
    async fn refuses_unavailable_execution_head() {
        let client = FakeEngineClient::new(Arc::new(RollupConfig::default()));
        let error = EngineConfig::check_startup(&client, GenesisStartArgs { allow_genesis_start: true })
            .await.expect_err("the launch flag must not hide an unavailable execution head");
        assert_eq!(error.to_string(), "local execution head is unavailable",
            "a missing head must retain its own error rather than be treated as genesis");
    }
}
