use reth_node_builder::Events;
use reth_optimism_node::OpEngineTypes;
use reth_optimism_payload_builder::OpBuiltPayload;
use tokio::sync::mpsc;
use tracing::warn;

/// Handles newly built flashblock payloads.
///
/// In the case of a payload built by this node, it is broadcast to peers and an event is sent to the payload builder.
/// In the case of a payload received from a peer, it is executed and if successful, an event is sent to the payload builder.
pub(super) struct PayloadHandler {
    // receives new payloads built by this builder.
    built_rx: mpsc::Receiver<OpBuiltPayload>,
    // sends a `Events::BuiltPayload` to the reth payload builder when a new payload is received.
    payload_events_handle: tokio::sync::broadcast::Sender<Events<OpEngineTypes>>,
}

impl PayloadHandler {
    pub(super) const fn new(
        built_rx: mpsc::Receiver<OpBuiltPayload>,
        payload_events_handle: tokio::sync::broadcast::Sender<Events<OpEngineTypes>>,
    ) -> Self {
        Self { built_rx, payload_events_handle }
    }

    pub(crate) async fn run(self) {
        let Self { mut built_rx, payload_events_handle } = self;

        tracing::debug!("flashblocks payload handler started");

        loop {
            tokio::select! {
                Some(payload) = built_rx.recv() => {
                    if let Err(e) = payload_events_handle.send(Events::BuiltPayload(payload)) {
                        warn!(e = ?e, "failed to send BuiltPayload event");
                    }
                }
                else => break,
            }
        }
    }
}
