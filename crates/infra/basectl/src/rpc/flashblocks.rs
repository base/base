use std::time::Duration;

use base_common_flashblocks::Flashblock;
use futures::StreamExt;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::connect_async;
use tracing::warn;

use crate::tui::Toast;

const WS_RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(1);
const WS_RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);

/// Connects to the URL in `url_rx`, forwarding decoded flashblocks to `tx`.
///
/// Reconnects automatically on disconnection or error (exponential backoff).
/// If `url_rx` emits a new value while connected, the current connection is
/// dropped immediately and a fresh connection is opened to the new URL with
/// no backoff delay.  This is the mechanism used to follow conductor leader
/// changes: when a new Raft leader is elected, the caller pushes the new
/// leader's flashblocks endpoint into the watch channel and the loop here
/// switches over without waiting for the old socket to time out.
async fn run_flashblock_ws_inner<T: Send + 'static>(
    url_rx: &mut watch::Receiver<String>,
    tx: &mpsc::Sender<T>,
    toast_tx: &mpsc::Sender<Toast>,
    map_fb: impl Fn(Flashblock) -> T,
) {
    let mut delay = WS_RECONNECT_INITIAL_DELAY;

    loop {
        let url = url_rx.borrow_and_update().clone();

        // Wrap connect_async in a select so a second leader change that
        // arrives while a TCP handshake is already in progress (e.g. rapid
        // successive transfers, or non-localhost endpoints that stall rather
        // than immediately refuse) is acted on without waiting for the
        // handshake to resolve.
        tokio::select! {
            result = connect_async(url.as_str()) => {
                match result {
                    Ok((ws_stream, _)) => {
                        delay = WS_RECONNECT_INITIAL_DELAY;
                        let (_, mut read) = ws_stream.split();
                        let mut leader_changed = false;

                        loop {
                            tokio::select! {
                                msg_opt = read.next() => {
                                    let msg = match msg_opt {
                                        Some(Ok(m)) => m,
                                        Some(Err(e)) => {
                                            warn!(error = %e, "Flashblock WebSocket connection error");
                                            let _ = toast_tx.try_send(Toast::warning("WebSocket disconnected"));
                                            break;
                                        }
                                        None => break,
                                    };
                                    if !msg.is_binary() && !msg.is_text() {
                                        continue;
                                    }
                                    let fb = match Flashblock::try_decode_message(msg.into_data()) {
                                        Ok(fb) => fb,
                                        Err(_) => continue,
                                    };
                                    if tx.send(map_fb(fb)).await.is_err() {
                                        return;
                                    }
                                }
                                Ok(()) = url_rx.changed() => {
                                    leader_changed = true;
                                    break;
                                }
                            }
                        }

                        if leader_changed {
                            // Skip backoff: reconnect immediately to the new leader.
                            delay = WS_RECONNECT_INITIAL_DELAY;
                            continue;
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, url = %url, "Failed to connect to flashblock WebSocket");
                        let _ = toast_tx.try_send(Toast::warning(format!(
                            "WebSocket connection failed, retrying in {}s",
                            delay.as_secs()
                        )));
                    }
                }
            }
            Ok(()) = url_rx.changed() => {
                // URL changed while connecting; abandon this attempt and
                // reconnect to the new leader immediately, without backoff.
                delay = WS_RECONNECT_INITIAL_DELAY;
                continue;
            }
        }

        // Exponential backoff, but skip the remainder if the URL changes.
        tokio::select! {
            _ = tokio::time::sleep(delay) => {
                delay = (delay * 2).min(WS_RECONNECT_MAX_DELAY);
            }
            Ok(()) = url_rx.changed() => {
                delay = WS_RECONNECT_INITIAL_DELAY;
            }
        }
    }
}

/// Subscribes to flashblocks via WebSocket and forwards raw flashblocks.
pub async fn run_flashblock_ws(
    mut url_rx: watch::Receiver<String>,
    tx: mpsc::Sender<Flashblock>,
    toast_tx: mpsc::Sender<Toast>,
) {
    run_flashblock_ws_inner(&mut url_rx, &tx, &toast_tx, |fb| fb).await;
}

/// A flashblock paired with its local receive timestamp.
#[derive(Debug)]
pub struct TimestampedFlashblock {
    /// The decoded flashblock.
    pub flashblock: Flashblock,
    /// Local time when this flashblock was received.
    pub received_at: chrono::DateTime<chrono::Local>,
}

/// Subscribes to flashblocks via WebSocket and forwards timestamped flashblocks.
pub async fn run_flashblock_ws_timestamped(
    mut url_rx: watch::Receiver<String>,
    tx: mpsc::Sender<TimestampedFlashblock>,
    toast_tx: mpsc::Sender<Toast>,
) {
    run_flashblock_ws_inner(&mut url_rx, &tx, &toast_tx, |fb| TimestampedFlashblock {
        flashblock: fb,
        received_at: chrono::Local::now(),
    })
    .await;
}
