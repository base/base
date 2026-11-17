use std::{sync::Arc, time::Duration};

use alloy_provider::RootProvider;
use alloy_rpc_types_engine::JwtSecret;
use alloy_transport::{TransportError, TransportErrorKind, TransportResult};
use base_cli_utils::GenesisStartArgs;
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use base_consensus_engine::{BaseEngineClient, EngineClient, EngineClientBuilder, EngineEndpoint};
use backon::Retryable;
use base_retry::RetryConfig;
use tracing::warn;
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
    /// Bounds each height request so a stalled EL request can be retried.
    pub const STARTUP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

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
    ///
    /// Retry temporary failures until shutdown, rather than crash-looping during a slow EL cold
    /// start. Backoff starts at 1 to 2 seconds and caps at 30 seconds including jitter. Each request
    /// has a 30-second timeout. Callers must drop this future when shutdown is requested.
    /// Successful genesis responses and terminal protocol errors are never retried.
    pub async fn check_startup<E: EngineClient>(
        client: &E,
        genesis_start: GenesisStartArgs,
    ) -> TransportResult<()> {
        let backoff = RetryConfig::unbounded(Duration::from_secs(1), Duration::from_secs(15))
            .to_backoff_builder();
        let head = (|| async {
            tokio::time::timeout(Self::STARTUP_REQUEST_TIMEOUT, client.latest_block_number())
                .await.map_err(TransportErrorKind::custom)?
        })
        .retry(backoff)
        .when(|error: &TransportError| {
            if let Some(kind) = error.as_transport_err() {
                match kind {
                    TransportErrorKind::Custom(_) | TransportErrorKind::BackendGone
                    | TransportErrorKind::MissingBatchResponse(_) => true,
                    TransportErrorKind::HttpError(error)
                    | TransportErrorKind::HttpErrorWithRetryAfter { error, .. } => {
                        error.status == 429 || (500..600).contains(&error.status)
                    }
                    _ => false,
                }
            } else if let Some(response) = error.as_error_resp() {
                response.code == -32603 || response.is_retry_err()
            } else {
                matches!(error, TransportError::NullResp)
            }
        })
        .notify(|error, delay| {
            warn!(error = %error, delay = ?delay, "Waiting for execution head before startup admission");
        })
        .await?;
        genesis_start.check(head, client.cfg().genesis.l2.number).map_err(TransportErrorKind::non_retryable)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

    use alloy_eips::BlockNumberOrTag;
    use base_cli_utils::GenesisStartArgs;
    use base_common_genesis::RollupConfig;
    use base_common_rpc_types::BaseBlockResponse;
    use base_common_network::SharedEngineApi;
    use base_consensus_engine::{BaseEngineClient, EngineClientBuilder, EngineEndpoint, InMemoryTransport};
    use alloy_provider::RootProvider;
    use base_common_network::Base;
    use alloy_rpc_types_engine::JwtSecret;
    use jsonrpsee::{RpcModule, types::ErrorObjectOwned};
    use url::Url;

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
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let mut module = RpcModule::new(());
        module.register_method("eth_blockNumber", move |_, _, _| {
            counter.fetch_add(1, Ordering::Relaxed);
            Err::<String, _>(ErrorObjectOwned::owned(-32601, "method not found", None::<()>))
        }).expect("the terminal-error method must register");
        let client = startup_client(module).await;
        let error = EngineConfig::check_startup(&client, GenesisStartArgs { allow_genesis_start: true })
            .await.expect_err("the launch flag must not hide a terminal RPC error");
        assert_eq!(error.as_error_resp().expect("the JSON-RPC error must be preserved").code, -32601,
            "method-not-found must remain a terminal protocol error");
        assert_eq!(calls.load(Ordering::Relaxed), 1, "a terminal RPC error must not be retried");
    }

    /// Builds a real height-query client without starting a server or using a socket.
    async fn startup_client(module: RpcModule<()>) -> BaseEngineClient<RootProvider, RootProvider<Base>> {
        let cfg = Arc::new(RollupConfig::default());
        EngineClientBuilder {
            l2: EngineEndpoint::InMemory {
                transport: InMemoryTransport { module },
                engine_api: SharedEngineApi::new(FakeEngineClient::new(Arc::clone(&cfg))),
            },
            l2_jwt: JwtSecret::random(),
            l1_rpc: Url::parse("http://127.0.0.1:1").expect("valid unused L1 URL"),
            l1_rpc_timeout: std::time::Duration::from_secs(1),
            cfg,
        }.build().await.expect("the in-memory engine client must build")
    }

    /// A slow EL must become admissible after real JSON-RPC failures, without a startup deadline.
    #[tokio::test(start_paused = true)]
    async fn waits_for_non_genesis_after_rpc_errors() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let mut module = RpcModule::new(());
        module.register_method("eth_blockNumber", move |_, _, _| {
            if counter.fetch_add(1, Ordering::Relaxed) < 100 {
                Err(ErrorObjectOwned::owned(-32603, "execution database is opening", None::<()>))
            } else {
                Ok("0x1")
            }
        }).expect("the recovering head method must register");
        let client = startup_client(module).await;
        let result = EngineConfig::check_startup(&client, GenesisStartArgs::default()).await;
        assert_eq!(result.is_ok(), true, "a recovered non-genesis EL must pass startup admission");
        assert_eq!(calls.load(Ordering::Relaxed), 101, "admission must retry every failure and stop on the first valid height");
    }

    /// A stalled height request must time out and allow a later request to recover.
    #[tokio::test(start_paused = true)]
    async fn retries_a_stalled_height_request() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let mut module = RpcModule::new(());
        module.register_async_method("eth_blockNumber", move |_, _, _| {
            let attempt = counter.fetch_add(1, Ordering::Relaxed);
            async move {
                if attempt == 0 {
                    std::future::pending::<()>().await;
                }
                "0x1"
            }
        }).expect("the stalled head method must register");
        let client = startup_client(module).await;
        let result = EngineConfig::check_startup(&client, GenesisStartArgs::default()).await;
        assert_eq!(result.is_ok(), true, "a stalled request must not block later startup admission");
        assert_eq!(calls.load(Ordering::Relaxed), 2, "one timeout must cause exactly one new request");
    }

    /// A transport failure must not become a fatal startup error.
    #[tokio::test(start_paused = true)]
    async fn retries_a_transport_failure() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let mut module = RpcModule::new(());
        module.register_method("eth_blockNumber", move |_, _, _| {
            if counter.fetch_add(1, Ordering::Relaxed) == 0 {
                panic!("simulated broken request task");
            }
            "0x1"
        }).expect("the transport-failure method must register");
        let client = startup_client(module).await;
        let result = EngineConfig::check_startup(&client, GenesisStartArgs::default()).await;
        assert_eq!(result.is_ok(), true, "a real transport error must recover without exiting startup");
        assert_eq!(calls.load(Ordering::Relaxed), 2, "one failed request task must cause exactly one retry");
    }

    /// Recovery must never admit genesis or retry a successful genesis refusal.
    #[tokio::test(start_paused = true)]
    async fn refuses_genesis_after_rpc_errors() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let mut module = RpcModule::new(());
        module.register_method("eth_blockNumber", move |_, _, _| {
            if counter.fetch_add(1, Ordering::Relaxed) < 3 {
                Err(ErrorObjectOwned::owned(-32603, "execution database is opening", None::<()>))
            } else {
                Ok("0x0")
            }
        }).expect("the recovering genesis method must register");
        let client = startup_client(module).await;
        let error = EngineConfig::check_startup(&client, GenesisStartArgs::default()).await
            .expect_err("the recovered genesis head must still require explicit launch permission");
        assert_eq!(error.to_string(),
            "refusing to start from genesis; restore a snapshot for an existing chain; use --allow-genesis-start only for launching a new chain",
            "recovery must preserve the exact genesis refusal");
        assert_eq!(calls.load(Ordering::Relaxed), 4, "the successful genesis response must refuse immediately without another retry");
    }
}
