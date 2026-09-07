use reth_ethereum::network::{Direction, api::PeerId};
use tokio::sync::mpsc;

use crate::subprotocol::connection::CustomCommand;

/// The events that can be emitted by our custom protocol.
#[derive(Debug)]
pub(crate) enum ProtocolEvent {
    Established {
        #[expect(dead_code)]
        direction: Direction,
        peer_id: PeerId,
        to_connection: mpsc::UnboundedSender<CustomCommand>,
    },
}
