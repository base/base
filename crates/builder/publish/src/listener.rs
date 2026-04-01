use core::fmt::{Debug, Formatter};

use tokio::{net::TcpListener, sync::broadcast::Receiver};
use tokio_tungstenite::{accept_async, tungstenite::Utf8Bytes};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{BroadcastLoop, PublishingMetrics};

/// WebSocket connection listener.
///
/// Accepts incoming TCP connections, upgrades them to WebSocket, and spawns
/// a [`BroadcastLoop`] for each connected client.
pub struct Listener {
    listener: TcpListener,
    receiver: Receiver<Utf8Bytes>,
    cancel: CancellationToken,
}

impl Debug for Listener {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Listener")
            .field("addr", &self.listener.local_addr())
            .finish_non_exhaustive()
    }
}

impl Listener {
    /// Creates a new [`Listener`] from an already-bound [`TcpListener`].
    pub const fn new(
        listener: TcpListener,
        receiver: Receiver<Utf8Bytes>,
        cancel: CancellationToken,
    ) -> Self {
        Self { listener, receiver, cancel }
    }

    /// Runs the listener loop, accepting connections until cancelled.
    pub async fn run(self) {
        let Self { listener, receiver, cancel } = self;

        let listen_addr =
            listener.local_addr().map(|a| a.to_string()).unwrap_or_else(|_| "unknown".into());
        info!(addr = %listen_addr, "WebSocketPublisher listening");

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    return;
                }

                result = listener.accept() => {
                    let Ok((connection, peer_addr)) = result else {
                        continue;
                    };

                    let cancel = cancel.clone();
                    let receiver_clone = receiver.resubscribe();

                    match accept_async(connection).await {
                        Ok(stream) => {
                            tokio::spawn(async move {
                                PublishingMetrics::ws_connections_active().increment(1.0);
                                let connected_at = std::time::Instant::now();
                                debug!(peer_addr = %peer_addr, "WebSocket connection established");

                                BroadcastLoop::new(stream, cancel, receiver_clone).run().await;

                                PublishingMetrics::ws_connections_active().decrement(1.0);
                                PublishingMetrics::ws_connection_duration()
                                    .record(connected_at.elapsed().as_secs_f64());
                                debug!(peer_addr = %peer_addr, "WebSocket connection closed");
                            });
                        }
                        Err(e) => {
                            PublishingMetrics::ws_handshake_error_count().increment(1);
                            warn!(peer_addr = %peer_addr, error = %e, "Failed to accept WebSocket connection");
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, time::Duration};

    use futures::StreamExt;
    use tokio::sync::broadcast;
    use tokio_tungstenite::{connect_async, tungstenite::Message};

    use super::*;

    async fn bind_listener() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, addr)
    }

    #[tokio::test]
    async fn listener_accepts_and_receives_message() {
        let (listener, addr) = bind_listener().await;
        let (tx, rx) = broadcast::channel::<Utf8Bytes>(16);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                Listener::new(listener, rx, cancel).run().await;
            }
        });

        let (mut client, _) = connect_async(format!("ws://{addr}")).await.unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        tx.send(Utf8Bytes::from("test-payload")).unwrap();

        let msg = client.next().await.unwrap().unwrap();
        assert_eq!(msg, Message::Text(Utf8Bytes::from("test-payload")));

        cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn listener_graceful_shutdown() {
        let (listener, _addr) = bind_listener().await;
        let (_, rx) = broadcast::channel::<Utf8Bytes>(16);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                Listener::new(listener, rx, cancel).run().await;
            }
        });

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn listener_multiple_clients_receive_same_message() {
        let (listener, addr) = bind_listener().await;
        let (tx, rx) = broadcast::channel::<Utf8Bytes>(16);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                Listener::new(listener, rx, cancel).run().await;
            }
        });

        let (mut client1, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        let (mut client2, _) = connect_async(format!("ws://{addr}")).await.unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        tx.send(Utf8Bytes::from("broadcast-msg")).unwrap();

        let msg1 = client1.next().await.unwrap().unwrap();
        let msg2 = client2.next().await.unwrap().unwrap();
        assert_eq!(msg1, Message::Text(Utf8Bytes::from("broadcast-msg")));
        assert_eq!(msg2, Message::Text(Utf8Bytes::from("broadcast-msg")));

        cancel.cancel();
        let _ = handle.await;
    }
}
