use std::{fmt, net::IpAddr};

use axum::extract::ws::WebSocket;

use crate::rate_limit::Ticket;

/// A connected WebSocket client with its associated metadata.
pub struct ClientConnection {
    client_addr: IpAddr,
    _ticket: Ticket,
    /// The WebSocket connection for this client.
    pub websocket: WebSocket,
}

impl fmt::Debug for ClientConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientConnection")
            .field("client_addr", &self.client_addr)
            .finish_non_exhaustive()
    }
}

impl ClientConnection {
    /// Creates a new client connection from the given address, rate-limit
    /// ticket, and WebSocket.
    pub const fn new(client_addr: IpAddr, ticket: Ticket, websocket: WebSocket) -> Self {
        Self { client_addr, _ticket: ticket, websocket }
    }

    /// Returns a string identifier for this client (its IP address).
    pub fn id(&self) -> String {
        self.client_addr.to_string()
    }
}
