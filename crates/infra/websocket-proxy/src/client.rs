use std::net::IpAddr;

use axum::extract::ws::WebSocket;

use crate::{filter::FilterType, rate_limit::Ticket};

/// A connected WebSocket client with its associated metadata.
#[derive(derive_more::Debug)]
pub struct ClientConnection {
    client_addr: IpAddr,
    #[debug(skip)]
    _ticket: Ticket,
    /// The WebSocket connection for this client.
    #[debug(skip)]
    pub websocket: WebSocket,
    /// The event filter this client is subscribed to.
    pub filter: FilterType,
}

impl ClientConnection {
    /// Creates a new client connection from the given address, rate-limit
    /// ticket, WebSocket, and event filter.
    pub const fn new(
        client_addr: IpAddr,
        ticket: Ticket,
        websocket: WebSocket,
        filter: FilterType,
    ) -> Self {
        Self { client_addr, _ticket: ticket, websocket, filter }
    }

    /// Returns a string identifier for this client (its IP address).
    pub fn id(&self) -> String {
        self.client_addr.to_string()
    }
}
