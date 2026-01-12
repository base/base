use crate::{builders::flashblocks::ctx::OpPayloadSyncerCtx, traits::ClientBounds};
use reth_node_builder::Events;
use reth_optimism_node::OpEngineTypes;
use reth_optimism_payload_builder::OpBuiltPayload;
use tokio::sync::mpsc;
use tracing::warn;

/// Handles newly built flashblock payloads.
///
/// When a payload is built by this node, an event is sent to the payload builder.
pub(crate) struct PayloadHandler<Client> {
    // receives new payloads built by this builder.
    built_rx: mpsc::Receiver<OpBuiltPayload>,
    // sends a `Events::BuiltPayload` to the reth payload builder when a new payload is received.
    payload_events_handle: tokio::sync::broadcast::Sender<Events<OpEngineTypes>>,
    // context required for execution of blocks during syncing
    ctx: OpPayloadSyncerCtx,
    // chain client
    client: Client,
    cancel: tokio_util::sync::CancellationToken,
}

impl<Client> PayloadHandler<Client>
where
    Client: ClientBounds + 'static,
{
    pub(crate) fn new(
        built_rx: mpsc::Receiver<OpBuiltPayload>,
        payload_events_handle: tokio::sync::broadcast::Sender<Events<OpEngineTypes>>,
        ctx: OpPayloadSyncerCtx,
        client: Client,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            built_rx,
            payload_events_handle,
            ctx,
            client,
            cancel,
        }
    }

    pub(crate) async fn run(self) {
        let Self {
            mut built_rx,
            payload_events_handle,
            ctx: _ctx,
            client: _client,
            cancel: _cancel,
        } = self;

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
