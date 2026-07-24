//! WebSocket subscription handling for flashblocks.

use std::{sync::Arc, time::Duration};

use base_common_flashblocks::Flashblock;
use futures::{SinkExt as _, StreamExt};
use tokio::{
    sync::mpsc,
    time::{Instant, interval_at},
};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use url::Url;

#[cfg(feature = "edge-measurement")]
use crate::{EdgeMeasurementGlobal, SourceConnectionTransitionV1};
use crate::{FlashblocksReceiver, metrics::Metrics};

#[derive(Debug)]
enum ActorMessage {
    BestPayload { payload: Flashblock },
}

/// Subscribes to flashblocks via WebSocket and forwards them to the receiver.
#[derive(Debug)]
pub struct FlashblocksSubscriber<Receiver> {
    flashblocks_state: Arc<Receiver>,
    ws_url: Url,
    ping_interval: Duration,
}

impl<Receiver> FlashblocksSubscriber<Receiver>
where
    Receiver: FlashblocksReceiver + Send + Sync + 'static,
{
    /// Max duration of backoff before reconnecting to upstream.
    pub const MAX_BACKOFF: Duration = Duration::from_secs(10);

    /// Creates a new flashblocks subscriber.
    pub const fn new(
        flashblocks_state: Arc<Receiver>,
        ws_url: Url,
        ping_interval: Duration,
    ) -> Self {
        Self { ws_url, flashblocks_state, ping_interval }
    }

    /// Starts the WebSocket subscription to receive flashblocks.
    pub fn start(&mut self) {
        info!(
            message = "Starting Flashblocks subscription",
            url = %self.ws_url,
        );

        let ws_url = self.ws_url.clone();
        let ping_period = self.ping_interval;

        let (sender, mut mailbox) = mpsc::channel(100);

        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            #[cfg(feature = "edge-measurement")]
            let mut direct_reconnect = false;
            #[cfg(feature = "edge-measurement")]
            let mut backoff_reconnect = false;
            #[cfg(feature = "edge-measurement")]
            let recorder = EdgeMeasurementGlobal::recorder();
            #[cfg(feature = "edge-measurement")]
            recorder.connection_transition(SourceConnectionTransitionV1::OwnerStart);
            #[cfg(feature = "edge-measurement")]
            recorder
                .connection_transition(SourceConnectionTransitionV1::InitialConnectAttemptStarted);

            loop {
                #[cfg(feature = "edge-measurement")]
                if direct_reconnect {
                    recorder.connection_transition(
                        SourceConnectionTransitionV1::DirectReconnectAttemptStarted,
                    );
                } else if backoff_reconnect {
                    recorder.connection_transition(
                        SourceConnectionTransitionV1::BackoffReconnectAttemptStarted,
                    );
                }
                #[cfg(feature = "edge-measurement")]
                {
                    direct_reconnect = false;
                    backoff_reconnect = false;
                }

                match connect_async(ws_url.as_str()).await {
                    Ok((ws_stream, _)) => {
                        backoff = Duration::from_secs(1);
                        info!(message = "WebSocket connection established");
                        #[cfg(feature = "edge-measurement")]
                        recorder.connection_transition(SourceConnectionTransitionV1::Established);

                        let mut ping_interval =
                            interval_at(Instant::now() + ping_period, ping_period);
                        let mut awaiting_pong_resp = false;
                        let mut read_open = true;

                        let (mut write, mut read) = ws_stream.split();

                        'conn: loop {
                            tokio::select! {
                                msg = read.next(), if read_open => {
                                    let Some(msg) = msg else {
                                        read_open = false;
                                        #[cfg(feature = "edge-measurement")]
                                        recorder.connection_transition(
                                            SourceConnectionTransitionV1::ReadHalfClosedWaitingForControl,
                                        );
                                        continue;
                                    };
                                    Metrics::upstream_messages().increment(1);

                                    match msg {
                                        Ok(msg @ (Message::Binary(_) | Message::Text(_))) => {
                                            #[cfg(feature = "edge-measurement")]
                                            recorder.connection_transition(
                                                SourceConnectionTransitionV1::DataMessageYielded,
                                            );
                                            let bytes = msg.into_data();
                                            #[cfg(feature = "edge-measurement")]
                                            let observation = recorder.observe_wire(&bytes);
                                            match Flashblock::try_decode_message(bytes) {
                                                Ok(payload) => {
                                                    #[cfg(feature = "edge-measurement")]
                                                    if let Some(observation) = observation {
                                                        _ = recorder.decoded_flashblock(observation, &payload);
                                                    }
                                                    let _ = sender.send(ActorMessage::BestPayload { payload }).await.map_err(|e| {
                                                        error!(message = "Failed to publish message to channel", error = %e);
                                                    });
                                                }
                                                Err(e) => {
                                                    error!(
                                                        message = "error decoding flashblock message",
                                                        error = %e
                                                    );
                                                }
                                            }
                                        }
                                        Ok(Message::Close(_)) => {
                                            #[cfg(feature = "edge-measurement")]
                                            recorder.connection_transition(
                                                SourceConnectionTransitionV1::CloseFrameReceived,
                                            );
                                            info!(message = "WebSocket connection closed by upstream");
                                            #[cfg(feature = "edge-measurement")]
                                            recorder.connection_transition(
                                                SourceConnectionTransitionV1::EstablishedClosedByClose,
                                            );
                                            #[cfg(feature = "edge-measurement")]
                                            {
                                                direct_reconnect = true;
                                            }
                                            break;
                                        }
                                        Ok(Message::Ping(_)) => {
                                            #[cfg(feature = "edge-measurement")]
                                            recorder.connection_transition(
                                                SourceConnectionTransitionV1::ControlPingReceived,
                                            );
                                        }
                                        Ok(Message::Pong(data)) => {
                                            #[cfg(feature = "edge-measurement")]
                                            recorder.connection_transition(
                                                SourceConnectionTransitionV1::ControlPongReceived,
                                            );
                                            trace!(target: "flashblocks_rpc::subscription",
                                                ?data,
                                                "Received pong from upstream"
                                            );
                                            awaiting_pong_resp = false;
                                            #[cfg(feature = "edge-measurement")]
                                            recorder.connection_transition(
                                                SourceConnectionTransitionV1::PongObserved,
                                            );
                                        }
                                        Err(e) => {
                                            #[cfg(feature = "edge-measurement")]
                                            recorder.connection_transition(
                                                SourceConnectionTransitionV1::ReadError,
                                            );
                                            Metrics::upstream_errors().increment(1);
                                            error!(
                                                message = "error receiving message",
                                                error = %e
                                            );
                                            #[cfg(feature = "edge-measurement")]
                                            recorder.connection_transition(
                                                SourceConnectionTransitionV1::EstablishedClosedByReadError,
                                            );
                                            #[cfg(feature = "edge-measurement")]
                                            {
                                                direct_reconnect = true;
                                            }
                                            break;
                                        }
                                        _ => {}
                                    }
                                },
                                _ = ping_interval.tick() => {
                                    #[cfg(feature = "edge-measurement")]
                                    recorder.connection_transition(
                                        SourceConnectionTransitionV1::OutgoingPingDue,
                                    );
                                    if awaiting_pong_resp {
                                        #[cfg(feature = "edge-measurement")]
                                        recorder.connection_transition(
                                            SourceConnectionTransitionV1::NoPongTimeout,
                                        );
                                          warn!(
                                            target: "flashblocks_rpc::subscription",
                                            ?backoff,
                                            timeout = ?ping_period,
                                            "No pong response from upstream, reconnecting",
                                        );
                                        #[cfg(feature = "edge-measurement")]
                                        recorder.connection_transition(
                                            SourceConnectionTransitionV1::EstablishedClosedByNoPong,
                                        );
                                        #[cfg(feature = "edge-measurement")]
                                        recorder.connection_transition(
                                            SourceConnectionTransitionV1::BackoffStarted,
                                        );
                                        backoff = Self::sleep(backoff).await;
                                        #[cfg(feature = "edge-measurement")]
                                        recorder.connection_transition(
                                            SourceConnectionTransitionV1::BackoffCompleted,
                                        );
                                        #[cfg(feature = "edge-measurement")]
                                        {
                                            backoff_reconnect = true;
                                        }
                                        break 'conn;
                                    }

                                    trace!(target: "flashblocks_rpc::subscription",
                                        "Sending ping to upstream"
                                    );

                                    if let Err(error) = write.send(Message::Ping(Default::default())).await {
                                        #[cfg(feature = "edge-measurement")]
                                        recorder.connection_transition(
                                            SourceConnectionTransitionV1::PingWriteFailure,
                                        );
                                        warn!(
                                            target: "flashblocks_rpc::subscription",
                                            ?backoff,
                                            %error,
                                            "WebSocket connection lost, reconnecting",
                                        );
                                        #[cfg(feature = "edge-measurement")]
                                        recorder.connection_transition(
                                            SourceConnectionTransitionV1::EstablishedClosedByPingWriteFailure,
                                        );
                                        #[cfg(feature = "edge-measurement")]
                                        recorder.connection_transition(
                                            SourceConnectionTransitionV1::BackoffStarted,
                                        );
                                        backoff = Self::sleep(backoff).await;
                                        #[cfg(feature = "edge-measurement")]
                                        recorder.connection_transition(
                                            SourceConnectionTransitionV1::BackoffCompleted,
                                        );
                                        #[cfg(feature = "edge-measurement")]
                                        {
                                            backoff_reconnect = true;
                                        }
                                        break 'conn;
                                    }
                                    #[cfg(feature = "edge-measurement")]
                                    recorder.connection_transition(
                                        SourceConnectionTransitionV1::OutgoingPingWritten,
                                    );
                                    #[cfg(feature = "edge-measurement")]
                                    if !read_open {
                                        recorder.connection_transition(
                                            SourceConnectionTransitionV1::OutgoingPingWrittenWhileReadHalfClosed,
                                        );
                                    }
                                    awaiting_pong_resp = true
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!(
                            message = "WebSocket connection error, retrying",
                            backoff_duration = ?backoff,
                            error = %e
                        );
                        #[cfg(feature = "edge-measurement")]
                        recorder
                            .connection_transition(SourceConnectionTransitionV1::ConnectFailure);
                        #[cfg(feature = "edge-measurement")]
                        recorder
                            .connection_transition(SourceConnectionTransitionV1::BackoffStarted);
                        backoff = Self::sleep(backoff).await;
                        #[cfg(feature = "edge-measurement")]
                        recorder
                            .connection_transition(SourceConnectionTransitionV1::BackoffCompleted);
                        #[cfg(feature = "edge-measurement")]
                        {
                            backoff_reconnect = true;
                        }
                        continue;
                    }
                }
            }
        });

        let flashblocks_state = Arc::clone(&self.flashblocks_state);
        tokio::spawn(async move {
            while let Some(message) = mailbox.recv().await {
                match message {
                    ActorMessage::BestPayload { payload } => {
                        flashblocks_state.on_flashblock_received(payload);
                    }
                }
            }
        });
    }

    /// Sleeps for given backoff duration. Returns incremented backoff duration, capped at [`MAX_BACKOFF`].
    async fn sleep(backoff: Duration) -> Duration {
        Metrics::reconnect_attempts().increment(1);
        tokio::time::sleep(backoff).await;
        std::cmp::min(backoff * 2, Self::MAX_BACKOFF)
    }
}
