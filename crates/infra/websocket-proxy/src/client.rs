use std::{fmt, net::IpAddr};

use axum::extract::ws::WebSocket;

use crate::{filter::FilterType, rate_limit::Ticket};

/// A connected WebSocket client with its associated metadata.
pub struct ClientConnection {
    client_addr: IpAddr,
    _ticket: Ticket,
    pub(crate) websocket: WebSocket,
    /// The event filter this client is subscribed to.
    pub filter: FilterType,
    /// Optional position from which to replay buffered flashblocks.
    ///
    /// When set, the registry sends all ring-buffered entries after
    /// `(block_number, flashblock_index)` before joining the live stream.
    pub resume_position: Option<(u64, u64)>,
}

impl fmt::Debug for ClientConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientConnection")
            .field("client_addr", &self.client_addr)
            .field("filter", &self.filter)
            .field("resume_position", &self.resume_position)
            .finish_non_exhaustive()
    }
}

impl ClientConnection {
    /// Creates a new client connection from the given address, rate-limit
    /// ticket, WebSocket, event filter, and optional resume position.
    pub const fn new(
        client_addr: IpAddr,
        ticket: Ticket,
        websocket: WebSocket,
        filter: FilterType,
        resume_position: Option<(u64, u64)>,
    ) -> Self {
        Self { client_addr, _ticket: ticket, websocket, filter, resume_position }
    }

    /// Returns a string identifier for this client (its IP address).
    pub fn id(&self) -> String {
        self.client_addr.to_string()
    }
}
