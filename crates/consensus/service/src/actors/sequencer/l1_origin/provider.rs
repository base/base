//! L1 provider interfaces and implementations for origin selection.

use std::{fmt::Debug, time::Instant};

use alloy_primitives::B256;
use alloy_provider::{Provider, RootProvider};
use async_trait::async_trait;
use base_protocol::BlockInfo;
use tokio::sync::watch;

use super::L1OriginSelectorError;
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

/// L1 [`BlockInfo`] provider interface for the [`super::L1OriginSelector`].
#[async_trait]
pub trait L1OriginSelectorProvider: Debug + Send + Sync + 'static {
    /// Returns the latest observed L1 head hash, used to identify the canonical chain view.
    fn chain_view(&self) -> Option<B256>;

    /// Returns a [`BlockInfo`] by its hash.
    async fn get_block_by_hash(
        &self,
        hash: B256,
    ) -> Result<Option<BlockInfo>, L1OriginSelectorError>;

    /// Returns a [`BlockInfo`] by its number.
    async fn get_block_by_number(
        &self,
        number: u64,
    ) -> Result<Option<BlockInfo>, L1OriginSelectorError>;
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
}

#[async_trait]
impl L1OriginSelectorProvider for DelayedL1OriginSelectorProvider {
    fn chain_view(&self) -> Option<B256> {
        self.l1_head.borrow().as_ref().map(|head| head.hash)
    }

    async fn get_block_by_hash(
        &self,
        hash: B256,
    ) -> Result<Option<BlockInfo>, L1OriginSelectorError> {
        // By-hash lookups are not delayed, as they're direct indexes.
        let start = Instant::now();
        let result = Provider::get_block_by_hash(&self.inner, hash).await;
        let outcome = rpc_outcome!(&result);
        Metrics::sequencer_l1_origin_rpc_duration_seconds("block_by_hash", outcome)
            .record(start.elapsed());
        Metrics::sequencer_l1_origin_rpc_calls_total("block_by_hash", outcome).increment(1);

        Ok(result?.map(Into::into))
    }

    async fn get_block_by_number(
        &self,
        number: u64,
    ) -> Result<Option<BlockInfo>, L1OriginSelectorError> {
        let Some(l1_head) = *self.l1_head.borrow() else {
            // Without an observed head, a by-number result cannot be tied to a canonical chain
            // view or checked against the confirmation delay.
            return Ok(None);
        };

        if number == 0
            || self.confirmation_depth == 0
            || number + self.confirmation_depth <= l1_head.number
        {
            let start = Instant::now();
            let result = Provider::get_block_by_number(&self.inner, number.into()).await;
            let outcome = rpc_outcome!(&result);
            Metrics::sequencer_l1_origin_rpc_duration_seconds("block_by_number", outcome)
                .record(start.elapsed());
            Metrics::sequencer_l1_origin_rpc_calls_total("block_by_number", outcome).increment(1);

            Ok(result?.map(Into::into))
        } else {
            Ok(None)
        }
    }
}

#[cfg(all(test, feature = "metrics"))]
mod tests {
    use std::time::Duration;

    use alloy_rpc_client::RpcClient;
    use httpmock::prelude::*;
    use metrics_util::{
        CompositeKey, MetricKind,
        debugging::{DebugValue, DebuggingRecorder},
    };
    use reqwest::Client;

    use super::*;

    type SnapshotEntry =
        (CompositeKey, Option<metrics::Unit>, Option<metrics::SharedString>, DebugValue);

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

                    assert!(provider.get_block_by_hash(B256::ZERO).await.is_err());
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

                    assert!(provider.get_block_by_number(1).await.is_err());
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

                    assert_eq!(provider.get_block_by_hash(B256::ZERO).await.unwrap(), None);
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

                    assert_eq!(provider.get_block_by_number(1).await.unwrap(), None);
                });
        });

        let snapshot = snapshotter.snapshot().into_vec();
        assert!(snapshot.iter().all(|(key, _, _, _)| {
            key.key().name() != "base_node.sequencer_l1_origin_rpc_calls_total"
                && key.key().name() != "base_node.sequencer_l1_origin_rpc_duration_seconds"
        }));
    }
}
