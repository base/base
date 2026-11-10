/// How a block watcher is receiving new blocks from a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionMode {
    /// Connected via WebSocket subscription.
    WebSocket,
    /// Connected via HTTP polling.
    Polling,
}

impl ConnectionMode {
    /// Returns the label appended to panel titles, including its leading space.
    pub const fn title_suffix(self) -> &'static str {
        match self {
            Self::WebSocket => " WS",
            Self::Polling => " Poll",
        }
    }
}
