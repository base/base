use core::{
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

use crate::BuilderConfig;

/// Configuration values specific to the flashblocks builder.
///
/// Controls flashblock timing and WebSocket publishing for progressive
/// block construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlashblocksConfig {
    /// The address of the websockets endpoint that listens for subscriptions to
    /// new flashblocks updates.
    pub ws_addr: SocketAddr,

    /// How often a flashblock is produced. This is independent of the block time of the chain.
    /// Each block will contain one or more flashblocks. On average, the number of flashblocks
    /// per block is equal to the block time divided by the flashblock interval.
    pub interval: Duration,

    /// How much time would be deducted from block build time to account for latencies in
    /// milliseconds. This value would be deducted from first flashblock and
    /// it shouldn't be more than interval.
    pub leeway_time: Duration,
}

impl Default for FlashblocksConfig {
    fn default() -> Self {
        Self {
            ws_addr: SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 1111),
            interval: Duration::from_millis(250),
            leeway_time: Duration::from_millis(50),
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl FlashblocksConfig {
    /// Creates a new [`FlashblocksConfig`] suitable for testing with a randomized port.
    pub fn for_tests() -> Self {
        Self {
            ws_addr: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0),
            interval: Duration::from_millis(200),
            leeway_time: Duration::from_millis(100),
        }
    }

    #[must_use]
    pub const fn with_interval_ms(mut self, ms: u64) -> Self {
        self.interval = Duration::from_millis(ms);
        self
    }

    #[must_use]
    pub const fn with_leeway_time_ms(mut self, ms: u64) -> Self {
        self.leeway_time = Duration::from_millis(ms);
        self
    }

    #[must_use]
    pub const fn with_port(mut self, port: u16) -> Self {
        self.ws_addr.set_port(port);
        self
    }
}

pub(super) trait FlashBlocksConfigExt {
    fn flashblocks_per_block(&self) -> u64;
}

impl FlashBlocksConfigExt for BuilderConfig {
    fn flashblocks_per_block(&self) -> u64 {
        if self.block_time.as_millis() == 0 {
            return 0;
        }
        (self.block_time.as_millis() / self.flashblocks.interval.as_millis()) as u64
    }
}
