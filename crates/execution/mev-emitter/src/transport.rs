//! C-4: outbound WebSocket transport for encoded [`crate::NodeEvent`]s.
//!
//! Streams each event — one [`crate::encode_event`] string per WebSocket TEXT
//! frame — to any number of connected clients (the TS `ProviderNodeStream`
//! consumer reads an `AsyncIterable<string>`, one event JSON per yielded
//! string). Framing is exactly one event per text frame: no JSON array, no
//! length prefix, no batching, no pretty-printing.
//!
//! Failure isolation is the hard requirement: this runs inside a LIVE node where
//! the ExEx is a critical tokio task and any propagated panic/error would crash
//! the whole node. So nothing here ever propagates an error to the ExEx — the
//! server failing to bind, a client handshake failing, a slow/lagging client, or
//! a send error is logged via `warn!`/`debug!` and the rest keeps running. The
//! producer side ([`EventSink::send_event`]) never blocks and never panics.

use std::net::SocketAddr;

use futures::SinkExt as _;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

/// Broadcast channel capacity (number of buffered events per receiver).
///
/// Bounded on purpose: a stalled client cannot grow node memory without bound.
/// A client that lags past this many events receives [`broadcast::error::RecvError::Lagged`]
/// and skips the missed events rather than stalling the producer — backpressure
/// must never reach the `ExEx`.
const CHANNEL_CAP: usize = 4096;

/// Default loopback bind address (§27 localhost-only). `8546` is the node's own
/// ws RPC, so the event transport uses a distinct port.
const DEFAULT_ADDR: &str = "127.0.0.1:8547";

/// Environment variable overriding the event-transport bind address.
const ADDR_ENV: &str = "MEV_EMITTER_WS_ADDR";

/// Producer handle for the outbound event transport. Cloneable and cheap; hand a
/// clone to any code that emits events. Dropping all sinks closes the channel
/// and ends connected clients' receive loops.
#[derive(Clone, Debug)]
pub struct EventSink {
    tx: broadcast::Sender<String>,
}

impl EventSink {
    /// Encode `event` and broadcast it to every connected client.
    ///
    /// Never blocks and never panics. The broadcast `send` only errors when there
    /// are zero receivers (no clients connected); that case is ignored. A client
    /// that lags past [`CHANNEL_CAP`] is dropped by its receiver (see the
    /// `Lagged` handling in [`start_event_server`]), by design — backpressure
    /// must never stall the `ExEx`.
    pub fn send_event(&self, event: &crate::NodeEvent) {
        let _ = self.tx.send(crate::encode_event(event));
    }
}

/// Start the outbound event WebSocket server and return its producer [`EventSink`].
///
/// Returns immediately: the bind + accept loop runs in a background task, so the
/// caller (the `ExEx`) is never blocked on bind. The returned sink is always valid
/// — if the server cannot start (bad address, bind failure) events simply go
/// nowhere and the `ExEx` is unaffected.
///
/// Address resolution: env [`ADDR_ENV`] (`MEV_EMITTER_WS_ADDR`), defaulting to
/// [`DEFAULT_ADDR`] (`127.0.0.1:8547`) when unset or empty. An unparseable
/// address is logged and the server is not started. A non-loopback address is
/// allowed but logged (§27 expects loopback).
pub fn start_event_server() -> EventSink {
    let (tx, _rx) = broadcast::channel::<String>(CHANNEL_CAP);
    let sink = EventSink { tx: tx.clone() };

    let raw = match std::env::var(ADDR_ENV) {
        Ok(s) if !s.trim().is_empty() => s,
        _ => DEFAULT_ADDR.to_string(),
    };
    let addr: SocketAddr = match raw.parse() {
        Ok(a) => a,
        Err(e) => {
            warn!(
                target: "base::mev_emitter",
                addr = %raw,
                error = %e,
                "invalid MEV_EMITTER_WS_ADDR; event transport disabled (events go nowhere)",
            );
            return sink;
        }
    };
    if !addr.ip().is_loopback() {
        warn!(
            target: "base::mev_emitter",
            addr = %addr,
            "MEV_EMITTER_WS_ADDR is not loopback (§27 expects localhost-only); proceeding",
        );
    }

    // Spawn the whole bind + accept loop so the function never awaits the bind:
    // the ExEx is never blocked waiting for the listener.
    tokio::spawn(async move {
        let listener = match TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                warn!(
                    target: "base::mev_emitter",
                    addr = %addr,
                    error = %e,
                    "event transport bind failed; transport disabled (events go nowhere)",
                );
                return;
            }
        };
        info!(target: "base::mev_emitter", addr = %addr, "event transport listening");
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    debug!(target: "base::mev_emitter", error = %e, "event transport accept failed");
                    continue;
                }
            };
            // Each connection gets a fresh receiver, captured before the spawn so
            // a client only sees events broadcast after it connected.
            let rx = tx.subscribe();
            tokio::spawn(handle_connection(stream, peer, rx));
        }
    });

    sink
}

/// Per-connection task: handshake, then forward each broadcast event as one
/// WebSocket TEXT frame until the client disconnects or the channel closes.
///
/// Failure-isolated: any error (handshake, send) only ends THIS connection.
async fn handle_connection(
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    mut rx: broadcast::Receiver<String>,
) {
    let mut ws = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            debug!(
                target: "base::mev_emitter",
                peer = %peer,
                error = %e,
                "event transport handshake failed; dropping connection",
            );
            return;
        }
    };
    debug!(target: "base::mev_emitter", peer = %peer, "event transport client connected");
    loop {
        match rx.recv().await {
            Ok(json) => {
                // One event = one TEXT frame: payload is the exact encode_event bytes.
                if ws.send(Message::Text(json.into())).await.is_err() {
                    // Client gone; end this connection only.
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                debug!(
                    target: "base::mev_emitter",
                    peer = %peer,
                    skipped = n,
                    "event transport client lagged; skipping dropped events",
                );
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    debug!(target: "base::mev_emitter", peer = %peer, "event transport client disconnected");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NodeEvent, PROTOCOL_VERSION, StateDiffEvent};
    use alloy_primitives::{Address, B256, I256};

    fn sample_event() -> NodeEvent {
        NodeEvent::StateDiff(StateDiffEvent {
            protocol_version: PROTOCOL_VERSION,
            tx_hash: B256::from([0x22; 32]),
            block_number: 47_517_747,
            flashblock_index: 3,
            payload_id: "0x04abc".to_string(),
            account: Address::from([0x11; 20]),
            token: Address::from([0x33; 20]),
            balance_delta_raw: I256::from_dec_str("-1000").unwrap(),
            internal_calls: None,
        })
    }

    #[test]
    fn subscriber_receives_encoded_event() {
        let (tx, mut rx) = broadcast::channel::<String>(8);
        let sink = EventSink { tx };
        let event = sample_event();
        sink.send_event(&event);
        let received = rx.try_recv().expect("event delivered to subscriber");
        assert_eq!(received, crate::encode_event(&event));
    }

    #[test]
    fn send_event_without_subscribers_does_not_panic() {
        let (tx, _rx) = broadcast::channel::<String>(8);
        // Drop the only receiver: send now has zero receivers.
        drop(_rx);
        let sink = EventSink { tx };
        // Must not panic even though there is no receiver.
        sink.send_event(&sample_event());
    }
}
