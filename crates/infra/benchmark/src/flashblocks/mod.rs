//! Flashblocks WebSocket consumer and replay server.

/// WebSocket consumer for flashblocks produced by the Builder client.
#[derive(Debug)]
pub struct FlashblocksClient {
    pub(crate) port: u16,
}

impl FlashblocksClient {
    /// Create a client that will connect to the builder's flashblocks WS port.
    pub fn new(port: u16) -> Self {
        Self { port }
    }

    /// WebSocket URL for this client.
    pub fn url(&self) -> String {
        format!("ws://127.0.0.1:{}", self.port)
    }
}
