use core::fmt::{Debug, Formatter};

use futures::{SinkExt, StreamExt};
use tokio::{
    net::TcpStream,
    sync::broadcast::{self, error::RecvError},
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, Utf8Bytes},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::PublishingMetrics;

/// Per-client broadcast sender.
///
/// Created for each connected WebSocket client. Forwards messages from the
/// broadcast channel to the client's WebSocket stream. Exits on cancellation,
/// channel close, or WebSocket error.
pub struct BroadcastLoop {
    stream: WebSocketStream<TcpStream>,
    cancel: CancellationToken,
    blocks: broadcast::Receiver<Utf8Bytes>,
}

impl Debug for BroadcastLoop {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BroadcastLoop").finish_non_exhaustive()
    }
}

impl BroadcastLoop {
    /// Creates a new [`BroadcastLoop`].
    pub const fn new(
        stream: WebSocketStream<TcpStream>,
        cancel: CancellationToken,
        blocks: broadcast::Receiver<Utf8Bytes>,
    ) -> Self {
        Self { stream, cancel, blocks }
    }

    /// Runs the broadcast loop until cancellation or error.
    pub async fn run(mut self) {
        let Ok(peer_addr) = self.stream.get_ref().peer_addr() else {
            return;
        };

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    info!("WebSocketPublisher is terminating, closing broadcast loop");
                    return;
                }

                payload = self.blocks.recv() => match payload {
                    Ok(payload) => {
                        PublishingMetrics::messages_sent_count().increment(1);

                        debug!(payload = ?payload, "Broadcasted payload");
                        if let Err(e) = self.stream.send(Message::Text(payload)).await {
                            PublishingMetrics::ws_send_error_count().increment(1);
                            debug!(peer_addr = %peer_addr, error = %e, "Closing subscription");
                            break;
                        }
                    }
                    Err(RecvError::Closed) => {
                        debug!("Broadcast channel closed, exiting broadcast loop");
                        return;
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        PublishingMetrics::ws_lagged_count().increment(skipped);
                        warn!(
                            skipped = skipped,
                            "Broadcast channel lagged, some messages were dropped"
                        );
                    }
                },

                message = self.stream.next() => if let Some(message) = message { match message {
                    Ok(Message::Close(_)) => {
                        info!(peer_addr = %peer_addr, "Closing frame received, stopping connection");
                        break;
                    }
                    Err(e) => {
                        warn!(peer_addr = %peer_addr, error = %e, "Received error. Closing subscription");
                        break;
                    }
                    _ => (),
                } }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use metrics_exporter_prometheus::PrometheusBuilder;
    use rstest::rstest;
    use tokio::net::TcpListener;
    use tokio_tungstenite::{accept_async, connect_async};

    use super::*;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    #[test]
    fn broadcast_loop_forwards_messages() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let rt = runtime();

        metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let (tx, rx) = broadcast::channel::<Utf8Bytes>(16);
                let cancel = CancellationToken::new();

                let server_handle = tokio::spawn({
                    let cancel = cancel.clone();
                    async move {
                        let (stream, _) = listener.accept().await.unwrap();
                        let ws = accept_async(stream).await.unwrap();
                        BroadcastLoop::new(ws, cancel, rx).run().await;
                    }
                });

                let (mut client, _) = connect_async(format!("ws://{addr}")).await.unwrap();

                tx.send(Utf8Bytes::from("hello")).unwrap();

                let msg = client.next().await.unwrap().unwrap();
                assert_eq!(msg, Message::Text(Utf8Bytes::from("hello")));

                cancel.cancel();
                let _ = server_handle.await;
            });
        });

        let rendered = handle.render();
        assert!(rendered.contains("base_builder_messages_sent_count 1"));
    }

    #[tokio::test]
    async fn broadcast_loop_exits_on_cancellation() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_, rx) = broadcast::channel::<Utf8Bytes>(16);
        let cancel = CancellationToken::new();

        let server_handle = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let ws = accept_async(stream).await.unwrap();
                BroadcastLoop::new(ws, cancel, rx).run().await;
            }
        });

        let (_client, _) = connect_async(format!("ws://{addr}")).await.unwrap();

        cancel.cancel();
        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn broadcast_loop_exits_on_close_frame() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_tx, rx) = broadcast::channel::<Utf8Bytes>(16);
        let cancel = CancellationToken::new();

        let server_handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = accept_async(stream).await.unwrap();
            BroadcastLoop::new(ws, cancel, rx).run().await;
        });

        let (mut client, _) = connect_async(format!("ws://{addr}")).await.unwrap();

        client.close(None).await.unwrap();

        server_handle.await.unwrap();
    }

    #[rstest]
    #[case::lagged(true)]
    #[case::closed(false)]
    fn broadcast_loop_handles_recv_errors(#[case] test_lagged: bool) {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let rt = runtime();

        metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();

                let (tx, rx) = broadcast::channel::<Utf8Bytes>(1);
                let cancel = CancellationToken::new();

                let server_handle = tokio::spawn({
                    let cancel = cancel.clone();
                    async move {
                        let (stream, _) = listener.accept().await.unwrap();
                        let ws = accept_async(stream).await.unwrap();
                        BroadcastLoop::new(ws, cancel, rx).run().await;
                    }
                });

                let (_client, _) = connect_async(format!("ws://{addr}")).await.unwrap();

                if test_lagged {
                    for i in 0..5 {
                        let _ = tx.send(Utf8Bytes::from(format!("msg{i}")));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    cancel.cancel();
                } else {
                    drop(tx);
                }

                server_handle.await.unwrap();
            });
        });

        if test_lagged {
            let rendered = handle.render();
            assert!(rendered.contains("base_builder_ws_lagged_count"));
        }
    }
}
