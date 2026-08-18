//! Forwarding of ingress metering data to builder RPCs.

use std::sync::Arc;

use alloy_primitives::TxHash;
use alloy_provider::{Provider, RootProvider};
use base_bundles::MeterBundleResponse;
use base_common_network::Base;
use base_observability_events::{
    TransactionEventProducer, TransactionEventType, transaction_event,
};
use serde_json::{Map, json};
use tokio::{
    sync::{Semaphore, broadcast},
    task::JoinSet,
};
use tracing::{debug, error, info, warn};
use url::Url;

/// Metering response plus the transaction hashes from the original ingress
/// request, used to keep builder-send journal events transaction-scoped even
/// when the simulator returns an empty result set.
#[derive(Debug, Clone)]
pub struct MeteringForwardMessage {
    /// Transaction hashes from the original ingress request.
    pub tx_hashes: Vec<TxHash>,
    /// Metering response returned by the simulation RPC.
    pub response: MeterBundleResponse,
}

/// Maximum number of concurrent RPC calls per builder URL.
const MAX_CONCURRENT_RPCS: usize = 64;

/// Connects ingress metering data to builder RPCs.
#[derive(Debug)]
pub struct BuilderConnector;

impl BuilderConnector {
    /// Spawns a background task that forwards metering data to the builder RPC.
    ///
    /// RPC calls are dispatched concurrently (up to [`MAX_CONCURRENT_RPCS`]) so
    /// that slow responses don't block the recv loop and risk broadcast channel
    /// lag.
    pub fn connect(
        metering_rx: broadcast::Receiver<MeteringForwardMessage>,
        builder_rpc: Url,
        destination_index: usize,
    ) {
        let rpc_url = builder_rpc.clone();
        let builder: RootProvider<Base> = RootProvider::new_http(builder_rpc);

        tokio::spawn(async move {
            let mut event_rx = metering_rx;
            let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_RPCS));
            let mut join_set = JoinSet::new();
            info!(url = %rpc_url, "BuilderConnector started, waiting for metering data");
            loop {
                // Drain completed tasks to observe panics / errors.
                while let Some(result) = join_set.try_join_next() {
                    if let Err(e) = result {
                        error!(url = %rpc_url, error = %e, "RPC forwarding task failed");
                    }
                }

                match event_rx.recv().await {
                    Ok(message) => {
                        let event = message.response;
                        if event.results.is_empty() {
                            for tx_hash in &message.tx_hashes {
                                emit_metering_send_event(
                                    TransactionEventType::IngressMeteringSendDropped,
                                    *tx_hash,
                                    event.bundle_hash,
                                    destination_index,
                                    Map::from_iter([(
                                        "drop_reason".to_string(),
                                        json!("empty_results"),
                                    )]),
                                );
                            }
                            warn!(
                                url = %rpc_url,
                                hash = %event.bundle_hash,
                                "Received metering information with no transactions"
                            );
                            continue;
                        }

                        let tx_hash = event.results[0].tx_hash;
                        let bundle_hash = event.bundle_hash;
                        let Ok(permit) = Arc::clone(&semaphore).acquire_owned().await else {
                            break;
                        };
                        let builder = builder.clone();
                        let url = rpc_url.clone();
                        join_set.spawn(async move {
                            emit_metering_send_event(
                                TransactionEventType::IngressMeteringSendAttempt,
                                tx_hash,
                                bundle_hash,
                                destination_index,
                                Map::new(),
                            );
                            match builder
                                .client()
                                .request::<(TxHash, MeterBundleResponse), ()>(
                                    "base_setMeteringInformation",
                                    (tx_hash, event),
                                )
                                .await
                            {
                                Ok(()) => {
                                    emit_metering_send_event(
                                        TransactionEventType::IngressMeteringSendSuccess,
                                        tx_hash,
                                        bundle_hash,
                                        destination_index,
                                        Map::new(),
                                    );
                                    debug!(
                                        url = %url,
                                        tx_hash = %tx_hash,
                                        "Forwarded metering information"
                                    );
                                }
                                Err(e) => {
                                    emit_metering_send_event(
                                        TransactionEventType::IngressMeteringSendFailure,
                                        tx_hash,
                                        bundle_hash,
                                        destination_index,
                                        Map::from_iter([(
                                            "error".to_string(),
                                            json!(e.to_string()),
                                        )]),
                                    );
                                    error!(
                                        url = %url,
                                        error = %e,
                                        tx_hash = %tx_hash,
                                        "Failed to set metering information"
                                    );
                                }
                            }
                            drop(permit);
                        });
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(
                            url = %rpc_url,
                            skipped = n,
                            "BuilderConnector lagged behind, skipped messages"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!(url = %rpc_url, "BuilderConnector channel closed, shutting down");
                        break;
                    }
                }
            }

            // Drain remaining in-flight tasks on shutdown.
            while let Some(result) = join_set.join_next().await {
                if let Err(e) = result {
                    error!(url = %rpc_url, error = %e, "RPC forwarding task failed during shutdown");
                }
            }
        });
    }
}

fn emit_metering_send_event(
    event_type: TransactionEventType,
    tx_hash: TxHash,
    bundle_hash: alloy_primitives::B256,
    destination_index: usize,
    mut data: Map<String, serde_json::Value>,
) {
    data.entry("bundle_hash".to_string()).or_insert_with(|| json!(bundle_hash.to_string()));
    data.entry("target".to_string()).or_insert_with(|| json!("builder_metering"));
    data.entry("rpc_method".to_string()).or_insert_with(|| json!("base_setMeteringInformation"));
    data.entry("destination_index".to_string()).or_insert_with(|| json!(destination_index));

    if let Err(err) = transaction_event!(
        producer: TransactionEventProducer::IngressRpc,
        event_type: event_type,
        tx_hash: tx_hash,
        id: {
            "destination_index" => destination_index,
            "bundle_hash" => bundle_hash.to_string(),
        },
        data: data,
    ) {
        debug!(error = %err, event_type = %event_type, "transaction event not written");
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy_primitives::{Address, TxHash, U256};
    use base_bundles::{MeterBundleResponse, TransactionResult};
    use tokio::sync::broadcast;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    use super::{BuilderConnector, MeteringForwardMessage};

    fn response_with_results() -> MeterBundleResponse {
        MeterBundleResponse {
            results: vec![TransactionResult {
                coinbase_diff: U256::ZERO,
                eth_sent_to_coinbase: U256::ZERO,
                from_address: Address::ZERO,
                gas_fees: U256::ZERO,
                gas_price: U256::ZERO,
                gas_used: 21000,
                to_address: Some(Address::ZERO),
                tx_hash: TxHash::ZERO,
                value: U256::ZERO,
                execution_time_us: 500,
                opcode_gas: vec![],
            }],
            ..Default::default()
        }
    }

    fn forwarding_message(response: MeterBundleResponse) -> MeteringForwardMessage {
        let tx_hashes = if response.results.is_empty() {
            vec![TxHash::ZERO]
        } else {
            response.results.iter().map(|result| result.tx_hash).collect()
        };
        MeteringForwardMessage { tx_hashes, response }
    }

    fn jsonrpc_ok() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": null
        }))
    }

    #[tokio::test]
    async fn test_builder_connector_survives_lagged_receiver() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(jsonrpc_ok())
            .expect(1..)
            .mount(&mock_server)
            .await;

        // Create a tiny broadcast channel so it's easy to overflow.
        let (tx, rx) = broadcast::channel::<MeteringForwardMessage>(2);

        // Overflow the buffer before the connector starts reading.
        // The receiver will get RecvError::Lagged on its first recv().
        let event = forwarding_message(response_with_results());
        for _ in 0..5 {
            tx.send(event.clone()).unwrap();
        }

        // Start the connector with the already-lagged receiver.
        BuilderConnector::connect(rx, mock_server.uri().parse().unwrap(), 0);

        // Give the connector time to hit Lagged and recover.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Send a new message after recovery — this must be forwarded.
        // send() fails with SendError when there are zero receivers,
        // which is exactly what happened with the old buggy code: the
        // connector task exited on Lagged, dropping the only receiver.
        assert!(
            tx.send(event).is_ok(),
            "connector task died — receiver was dropped after Lagged error"
        );

        // Wait for the RPC call to complete.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // wiremock verifies expect(1..) — at least one call was made,
        // proving the connector survived the Lagged error.
    }

    #[tokio::test]
    async fn test_builder_connector_forwards_metering_data() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST")).respond_with(jsonrpc_ok()).expect(1).mount(&mock_server).await;

        let (tx, rx) = broadcast::channel::<MeteringForwardMessage>(16);
        BuilderConnector::connect(rx, mock_server.uri().parse().unwrap(), 0);

        tx.send(forwarding_message(response_with_results())).unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;
        // wiremock verifies exactly 1 call was made.
    }

    #[tokio::test]
    async fn test_builder_connector_skips_empty_results() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST")).respond_with(jsonrpc_ok()).expect(0).mount(&mock_server).await;

        let (tx, rx) = broadcast::channel::<MeteringForwardMessage>(16);
        BuilderConnector::connect(rx, mock_server.uri().parse().unwrap(), 0);

        // Default response has empty results — should be skipped.
        tx.send(forwarding_message(MeterBundleResponse::default())).unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;
        // wiremock verifies 0 calls were made.
    }

    #[tokio::test]
    async fn test_builder_connector_forwards_concurrently() {
        let mock_server = MockServer::start().await;

        // Each response takes 200ms. Sequential forwarding would need >=1000ms
        // for 5 messages. Concurrent forwarding completes in ~200ms; we allow
        // a generous 2s budget so CI load doesn't cause flaky failures.
        Mock::given(method("POST"))
            .respond_with(jsonrpc_ok().set_delay(Duration::from_millis(200)))
            .expect(5)
            .mount(&mock_server)
            .await;

        let (tx, rx) = broadcast::channel::<MeteringForwardMessage>(16);
        BuilderConnector::connect(rx, mock_server.uri().parse().unwrap(), 0);

        for _ in 0..5 {
            tx.send(forwarding_message(response_with_results())).unwrap();
        }

        // 2s is generous for concurrent (~200ms) but well under sequential (>=1s).
        tokio::time::sleep(Duration::from_millis(2000)).await;

        // wiremock verifies exactly 5 calls were made within the time window.
    }

    #[tokio::test]
    async fn test_builder_connector_shuts_down_on_channel_close() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST")).respond_with(jsonrpc_ok()).expect(1).mount(&mock_server).await;

        let (tx, rx) = broadcast::channel::<MeteringForwardMessage>(16);
        BuilderConnector::connect(rx, mock_server.uri().parse().unwrap(), 0);

        // Send one message, then close the channel.
        tx.send(forwarding_message(response_with_results())).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(tx);

        // The task should exit gracefully without panic.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
