//! L1 provider interfaces and implementations for origin selection.

use std::{fmt::Debug, sync::Arc, time::Instant};

use alloy_consensus::{Header, Receipt};
use alloy_primitives::B256;
use alloy_provider::{Provider, RootProvider};
use alloy_transport::TransportErrorKind;
use async_trait::async_trait;
use base_protocol::BlockInfo;
use tokio::sync::watch;

use super::{L1OriginSelectorError, PreparedL1Origin};
use crate::Metrics;

macro_rules! rpc_outcome {
    ($result:expr) => {
        match $result {
            Ok(Some(_)) => "success",
            Ok(None) => "not_found",
            Err(error)
                if error
                    .as_transport_err()
                    .and_then(|error| error.as_custom())
                    .and_then(|error| error.downcast_ref::<reqwest::Error>())
                    .is_some_and(reqwest::Error::is_timeout) =>
            {
                "timeout"
            }
            Err(_) => "error",
        }
    };
}

/// Prepared L1 origin provider interface for the [`super::L1OriginSelector`].
#[async_trait]
pub trait L1OriginSelectorProvider: Debug + Send + Sync + 'static {
    /// Returns the latest observed L1 head hash, used to identify the canonical chain view.
    fn chain_view(&self) -> Option<B256>;

    /// Returns the origin addressed by the exact `hash`, if available.
    ///
    /// This lookup verifies that the returned header hashes to `hash`, but does not establish that
    /// the block is canonical. Successor canonicality is established separately by a by-number
    /// lookup tied to [`Self::chain_view`]. Missing receipts do not make an exact-hash current
    /// origin unavailable; the returned origin may omit them so the attributes provider can use
    /// its RPC fallback.
    async fn prepared_by_hash(
        &self,
        hash: B256,
    ) -> Result<Option<PreparedL1Origin>, L1OriginSelectorError>;

    /// Returns a canonical successor candidate prepared by its number.
    ///
    /// The candidate may omit receipts when their RPC is unavailable. This lets the selector
    /// validate successor ancestry immediately, but it must not adopt the successor until receipts
    /// are present.
    async fn prepared_by_number(
        &self,
        number: u64,
    ) -> Result<Option<PreparedL1Origin>, L1OriginSelectorError>;
}

/// A wrapper around the [`RootProvider`] that delays the view of the L1 chain by a configurable
/// amount of blocks.
#[derive(Debug)]
pub struct DelayedL1OriginSelectorProvider {
    /// The inner [`RootProvider`].
    inner: RootProvider,
    /// The L1 head watch channel.
    l1_head: watch::Receiver<Option<BlockInfo>>,
    /// The confirmation depth to delay the view of the L1 chain.
    confirmation_depth: u64,
}

impl DelayedL1OriginSelectorProvider {
    /// Creates a new [`DelayedL1OriginSelectorProvider`].
    pub const fn new(
        inner: RootProvider,
        l1_head: watch::Receiver<Option<BlockInfo>>,
        confirmation_depth: u64,
    ) -> Self {
        Self { inner, l1_head, confirmation_depth }
    }

    async fn header_by_hash(&self, hash: B256) -> Result<Option<Header>, L1OriginSelectorError> {
        let start = Instant::now();
        let result = Provider::get_block_by_hash(&self.inner, hash).await;
        let outcome = rpc_outcome!(&result);
        Metrics::sequencer_l1_origin_rpc_duration_seconds("block_by_hash", outcome)
            .record(start.elapsed());
        Metrics::sequencer_l1_origin_rpc_calls_total("block_by_hash", outcome).increment(1);

        Ok(result?.map(|block| block.header.into_consensus()))
    }

    async fn header_by_number(&self, number: u64) -> Result<Option<Header>, L1OriginSelectorError> {
        let start = Instant::now();
        let result = Provider::get_block_by_number(&self.inner, number.into()).await;
        let outcome = rpc_outcome!(&result);
        Metrics::sequencer_l1_origin_rpc_duration_seconds("block_by_number", outcome)
            .record(start.elapsed());
        Metrics::sequencer_l1_origin_rpc_calls_total("block_by_number", outcome).increment(1);

        Ok(result?.map(|block| block.header.into_consensus()))
    }

    async fn receipts_by_hash(
        &self,
        hash: B256,
    ) -> Result<Option<Vec<Receipt>>, L1OriginSelectorError> {
        let start = Instant::now();
        let result = Provider::get_block_receipts(&self.inner, hash.into()).await;
        let outcome = rpc_outcome!(&result);
        Metrics::sequencer_l1_origin_rpc_duration_seconds("block_receipts", outcome)
            .record(start.elapsed());
        Metrics::sequencer_l1_origin_rpc_calls_total("block_receipts", outcome).increment(1);

        let Some(receipts) = result? else {
            return Ok(None);
        };
        receipts
            .into_iter()
            .map(|receipt| receipt.inner.into_primitives_receipt().as_receipt().cloned())
            .collect::<Option<Vec<_>>>()
            .map(Some)
            .ok_or_else(|| {
                L1OriginSelectorError::Provider(TransportErrorKind::custom_str(
                    "failed to convert RPC receipts",
                ))
            })
    }
}

#[async_trait]
impl L1OriginSelectorProvider for DelayedL1OriginSelectorProvider {
    fn chain_view(&self) -> Option<B256> {
        self.l1_head.borrow().as_ref().map(|head| head.hash)
    }

    async fn prepared_by_hash(
        &self,
        hash: B256,
    ) -> Result<Option<PreparedL1Origin>, L1OriginSelectorError> {
        // By-hash lookups are not delayed, as they're direct indexes.
        let Some(header) = self.header_by_hash(hash).await? else {
            return Ok(None);
        };
        let returned_hash = header.hash_slow();
        if returned_hash != hash {
            warn!(target: "l1_origin_selector", requested = %hash, returned = %returned_hash, "L1 RPC returned a mismatched header hash");
            return Ok(None);
        }
        let receipts = self.receipts_by_hash(hash).await?.map(Arc::new);
        Ok(Some(PreparedL1Origin { hash, header, receipts }))
    }

    async fn prepared_by_number(
        &self,
        number: u64,
    ) -> Result<Option<PreparedL1Origin>, L1OriginSelectorError> {
        let Some(l1_head) = *self.l1_head.borrow() else {
            // Without an observed head, a by-number result cannot be tied to a canonical chain
            // view or checked against the confirmation delay.
            return Ok(None);
        };

        if number == 0
            || self.confirmation_depth == 0
            || number.saturating_add(self.confirmation_depth) <= l1_head.number
        {
            let Some(header) = self.header_by_number(number).await? else {
                return Ok(None);
            };
            if header.number != number {
                warn!(
                    target: "l1_origin_selector",
                    requested = number,
                    returned = header.number,
                    "L1 RPC returned a header at the wrong block number"
                );
                return Err(L1OriginSelectorError::Provider(TransportErrorKind::custom_str(
                    "L1 RPC returned a header at the wrong block number",
                )));
            }
            let hash = header.hash_slow();
            let receipts = match self.receipts_by_hash(hash).await {
                Ok(receipts) => receipts.map(Arc::new),
                Err(error) => {
                    warn!(
                        target: "l1_origin_selector",
                        error = %error,
                        l1_origin = %hash,
                        "L1 receipts unavailable while preparing successor origin"
                    );
                    None
                }
            };
            Ok(Some(PreparedL1Origin { hash, header, receipts }))
        } else {
            Ok(None)
        }
    }
}

#[cfg(all(test, feature = "metrics"))]
mod tests {
    use std::time::Duration;

    use alloy_eips::NumHash;
    use alloy_rpc_client::RpcClient;
    use alloy_rpc_types_eth::{Block as RpcBlock, Header as RpcHeader};
    use base_common_genesis::RollupConfig;
    use base_protocol::L2BlockInfo;
    use httpmock::prelude::*;
    use metrics_util::{
        CompositeKey, MetricKind,
        debugging::{DebugValue, DebuggingRecorder},
    };
    use reqwest::Client;

    use super::*;
    use crate::{L1OriginSelector, OriginSelector};

    type SnapshotEntry =
        (CompositeKey, Option<metrics::Unit>, Option<metrics::SharedString>, DebugValue);

    #[derive(serde::Serialize)]
    struct JsonRpcResponse<T> {
        jsonrpc: &'static str,
        id: u64,
        result: T,
    }

    const REQUEST_TIMEOUT: Duration = Duration::from_millis(25);
    const RESPONSE_DELAY: Duration = Duration::from_millis(250);

    fn test_provider(
        server: &MockServer,
        l1_head: Option<BlockInfo>,
    ) -> DelayedL1OriginSelectorProvider {
        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("test HTTP client must build");
        let rpc_url = server.url("/").parse().expect("mock server URL must parse");
        let provider = RootProvider::new(RpcClient::new_http_with_client(client, rpc_url));
        let (_, l1_head_rx) = watch::channel(l1_head);
        DelayedL1OriginSelectorProvider::new(provider, l1_head_rx, 0)
    }

    fn metric<'a>(
        snapshot: &'a [SnapshotEntry],
        kind: MetricKind,
        name: &str,
        method: &str,
        outcome: &str,
    ) -> Option<&'a DebugValue> {
        snapshot
            .iter()
            .find(|(key, _, _, _)| {
                key.kind() == kind
                    && key.key().name() == name
                    && key
                        .key()
                        .labels()
                        .any(|label| label.key() == "method" && label.value() == method)
                    && key
                        .key()
                        .labels()
                        .any(|label| label.key() == "outcome" && label.value() == outcome)
            })
            .map(|(_, _, _, value)| value)
    }

    fn assert_observation(snapshot: &[SnapshotEntry], method: &str, outcome: &str) {
        assert_eq!(
            metric(
                snapshot,
                MetricKind::Counter,
                "base_node.sequencer_l1_origin_rpc_calls_total",
                method,
                outcome,
            ),
            Some(&DebugValue::Counter(1)),
        );
        match metric(
            snapshot,
            MetricKind::Histogram,
            "base_node.sequencer_l1_origin_rpc_duration_seconds",
            method,
            outcome,
        ) {
            Some(DebugValue::Histogram(values)) => {
                assert_eq!(values.len(), 1);
                assert!(values[0].into_inner() > 0.0);
            }
            value => panic!("expected one duration observation, got {value:?}"),
        }
    }

    #[tokio::test]
    async fn current_origin_can_be_prepared_without_receipts() {
        let server = MockServer::start_async().await;
        let header = Header::default();
        let hash = header.hash_slow();
        let block: RpcBlock = RpcBlock::empty(RpcHeader::new(header.clone()));
        let block_mock = server
            .mock_async(move |when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockByHash"}"#);
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body_obj(&JsonRpcResponse { jsonrpc: "2.0", id: 0, result: block });
            })
            .await;
        let receipts_mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockReceipts"}"#);
                then.status(200)
                    .header("content-type", "application/json")
                    .body(r#"{"jsonrpc":"2.0","id":1,"result":null}"#);
            })
            .await;
        let provider = test_provider(&server, None);

        let prepared = provider
            .prepared_by_hash(hash)
            .await
            .unwrap()
            .expect("header should prepare the current origin");

        assert_eq!(prepared.header, header);
        assert!(prepared.receipts.is_none());
        block_mock.assert_calls_async(1).await;
        receipts_mock.assert_calls_async(1).await;
    }

    #[tokio::test]
    async fn successor_header_is_prepared_without_receipts() {
        let server = MockServer::start_async().await;
        let header = Header { number: 7, ..Default::default() };
        let hash = header.hash_slow();
        let block: RpcBlock = RpcBlock::empty(RpcHeader::new(header));
        let block_mock = server
            .mock_async(move |when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockByNumber"}"#);
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body_obj(&JsonRpcResponse { jsonrpc: "2.0", id: 0, result: block });
            })
            .await;
        let receipts_mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockReceipts"}"#);
                then.status(200)
                    .header("content-type", "application/json")
                    .body(r#"{"jsonrpc":"2.0","id":1,"result":null}"#);
            })
            .await;
        let provider = test_provider(&server, Some(BlockInfo { number: 7, ..Default::default() }));

        let prepared = provider
            .prepared_by_number(7)
            .await
            .unwrap()
            .expect("successor header should remain available for ancestry validation");

        assert_eq!(prepared.hash, hash);
        assert!(prepared.receipts.is_none());
        block_mock.assert_calls_async(1).await;
        receipts_mock.assert_calls_async(1).await;
    }

    #[tokio::test]
    async fn current_origin_receipt_timeout_is_propagated() {
        let server = MockServer::start_async().await;
        let header = Header::default();
        let hash = header.hash_slow();
        let block: RpcBlock = RpcBlock::empty(RpcHeader::new(header));
        let block_mock = server
            .mock_async(move |when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockByHash"}"#);
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body_obj(&JsonRpcResponse { jsonrpc: "2.0", id: 0, result: block });
            })
            .await;
        let receipts_mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockReceipts"}"#);
                then.status(200).delay(RESPONSE_DELAY);
            })
            .await;
        let provider = test_provider(&server, None);

        let result = provider.prepared_by_hash(hash).await;

        assert!(result.is_err());
        block_mock.assert_calls_async(1).await;
        receipts_mock.assert_calls_async(1).await;
    }

    #[tokio::test]
    async fn successor_header_is_prepared_when_receipts_time_out() {
        let server = MockServer::start_async().await;
        let header = Header { number: 7, ..Default::default() };
        let hash = header.hash_slow();
        let block: RpcBlock = RpcBlock::empty(RpcHeader::new(header));
        let block_mock = server
            .mock_async(move |when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockByNumber"}"#);
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body_obj(&JsonRpcResponse { jsonrpc: "2.0", id: 0, result: block });
            })
            .await;
        let receipts_mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockReceipts"}"#);
                then.status(200).delay(RESPONSE_DELAY);
            })
            .await;
        let provider = test_provider(&server, Some(BlockInfo { number: 7, ..Default::default() }));

        let prepared =
            provider.prepared_by_number(7).await.unwrap().expect(
                "receipt timeout must preserve the successor header for ancestry validation",
            );

        assert_eq!(prepared.hash, hash);
        assert!(prepared.receipts.is_none());
        block_mock.assert_calls_async(1).await;
        receipts_mock.assert_calls_async(1).await;
    }

    #[tokio::test]
    async fn selector_detects_orphan_when_successor_receipts_time_out() {
        let server = MockServer::start_async().await;
        let current_header = Header { number: 4, ..Default::default() };
        let current_hash = current_header.hash_slow();
        let successor_header = Header {
            parent_hash: B256::with_last_byte(99),
            number: 5,
            timestamp: 2,
            ..Default::default()
        };
        let successor_hash = successor_header.hash_slow();
        let current_block: RpcBlock = RpcBlock::empty(RpcHeader::new(current_header.clone()));
        let successor_block: RpcBlock = RpcBlock::empty(RpcHeader::new(successor_header));
        server
            .mock_async(move |when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockByHash"}"#);
                then.status(200).header("content-type", "application/json").json_body_obj(
                    &JsonRpcResponse { jsonrpc: "2.0", id: 0, result: current_block },
                );
            })
            .await;
        server
            .mock_async(move |when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockByNumber"}"#);
                then.status(200).header("content-type", "application/json").json_body_obj(
                    &JsonRpcResponse { jsonrpc: "2.0", id: 0, result: successor_block },
                );
            })
            .await;
        let current_receipts_mock = server
            .mock_async(move |when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockReceipts"}"#)
                    .body_includes(current_hash.to_string());
                then.status(200)
                    .header("content-type", "application/json")
                    .body(r#"{"jsonrpc":"2.0","id":1,"result":[]}"#);
            })
            .await;
        let successor_receipts_mock = server
            .mock_async(move |when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockReceipts"}"#)
                    .body_includes(successor_hash.to_string());
                then.status(200).delay(RESPONSE_DELAY);
            })
            .await;
        let provider = test_provider(
            &server,
            Some(BlockInfo { hash: B256::with_last_byte(10), number: 10, ..Default::default() }),
        );
        let mut selector = L1OriginSelector::new(
            Arc::new(RollupConfig {
                block_time: 2,
                max_sequencer_drift: 600,
                ..Default::default()
            }),
            provider,
        );
        let unsafe_head = L2BlockInfo {
            l1_origin: NumHash { number: current_header.number, hash: current_hash },
            ..Default::default()
        };

        assert_eq!(selector.next_l1_origin(unsafe_head).await.unwrap().hash, current_hash);
        let error = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match selector.next_l1_origin(unsafe_head).await {
                    Ok(_) => tokio::task::yield_now().await,
                    Err(error) => break error,
                }
            }
        })
        .await
        .expect("selector should detect the orphan after the receipt request times out");

        assert!(matches!(
            error,
            L1OriginSelectorError::NextL1OriginOrphaned { current, next }
                if current == current_hash && next == successor_hash
        ));
        current_receipts_mock.assert_calls_async(1).await;
        successor_receipts_mock.assert_calls_async(1).await;
    }

    #[test]
    fn records_block_by_hash_timeouts() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime must build")
                .block_on(async {
                    let server = MockServer::start_async().await;
                    let mock = server
                        .mock_async(|when, then| {
                            when.method(POST).path("/");
                            then.status(200).delay(RESPONSE_DELAY);
                        })
                        .await;
                    let provider = test_provider(&server, None);

                    assert!(provider.prepared_by_hash(B256::ZERO).await.is_err());
                    mock.assert_calls_async(1).await;
                });
        });

        assert_observation(&snapshotter.snapshot().into_vec(), "block_by_hash", "timeout");
    }

    #[test]
    fn records_block_by_number_timeouts() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime must build")
                .block_on(async {
                    let server = MockServer::start_async().await;
                    let mock = server
                        .mock_async(|when, then| {
                            when.method(POST).path("/");
                            then.status(200).delay(RESPONSE_DELAY);
                        })
                        .await;
                    let provider = test_provider(
                        &server,
                        Some(BlockInfo { number: 10, ..Default::default() }),
                    );

                    assert!(provider.prepared_by_number(1).await.is_err());
                    mock.assert_calls_async(1).await;
                });
        });

        assert_observation(&snapshotter.snapshot().into_vec(), "block_by_number", "timeout");
    }

    #[test]
    fn records_not_found_responses() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime must build")
                .block_on(async {
                    let server = MockServer::start_async().await;
                    server
                        .mock_async(|when, then| {
                            when.method(POST).path("/");
                            then.status(200)
                                .header("content-type", "application/json")
                                .body(r#"{"jsonrpc":"2.0","id":0,"result":null}"#);
                        })
                        .await;
                    let provider = test_provider(&server, None);

                    assert!(provider.prepared_by_hash(B256::ZERO).await.unwrap().is_none());
                });
        });

        assert_observation(&snapshotter.snapshot().into_vec(), "block_by_hash", "not_found");
    }

    #[test]
    fn skips_metrics_when_no_number_request_is_made() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime must build")
                .block_on(async {
                    let server = MockServer::start_async().await;
                    let provider = test_provider(&server, None);

                    assert!(provider.prepared_by_number(1).await.unwrap().is_none());
                });
        });

        let snapshot = snapshotter.snapshot().into_vec();
        assert!(snapshot.iter().all(|(key, _, _, _)| {
            key.key().name() != "base_node.sequencer_l1_origin_rpc_calls_total"
                && key.key().name() != "base_node.sequencer_l1_origin_rpc_duration_seconds"
        }));
    }
}
