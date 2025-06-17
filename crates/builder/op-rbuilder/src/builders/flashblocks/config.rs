use crate::{args::OpRbuilderArgs, builders::BuilderConfig};
use core::{
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

/// Configuration values that are specific to the flashblocks builder.
#[derive(Debug, Clone)]
pub struct FlashblocksConfig {
    /// The address of the websockets endpoint that listens for subscriptions to
    /// new flashblocks updates.
    pub ws_addr: SocketAddr,

    /// How often a flashblock is produced. This is independent of the block time of the chain.
    /// Each block will contain one or more flashblocks. On average, the number of flashblocks
    /// per block is equal to the block time divided by the flashblock interval.
    pub interval: Duration,

    /// How much time would be deducted from block build time to account for latencies in
    /// milliseconds
    pub leeway_time: Duration,

    /// Enables dynamic flashblocks number based on FCU arrival time
    pub dynamic_adjustment: bool,
}

impl Default for FlashblocksConfig {
    fn default() -> Self {
        Self {
            ws_addr: SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 1111),
            interval: Duration::from_millis(250),
            leeway_time: Duration::from_millis(50),
            dynamic_adjustment: false,
        }
    }
}

impl TryFrom<OpRbuilderArgs> for FlashblocksConfig {
    type Error = eyre::Report;

    fn try_from(args: OpRbuilderArgs) -> Result<Self, Self::Error> {
        let interval = Duration::from_millis(args.flashblocks.flashblocks_block_time);

        let ws_addr = SocketAddr::new(
            args.flashblocks.flashblocks_addr.parse()?,
            args.flashblocks.flashblocks_port,
        );

        let leeway_time = Duration::from_millis(args.flashblocks.flashblocks_leeway_time);

        let dynamic_adjustment = args.flashblocks.flashblocks_dynamic;

        Ok(Self {
            ws_addr,
            interval,
            leeway_time,
            dynamic_adjustment,
        })
    }
}

pub trait FlashBlocksConfigExt {
    fn flashblocks_per_block(&self) -> u64;
}

impl FlashBlocksConfigExt for BuilderConfig<FlashblocksConfig> {
    fn flashblocks_per_block(&self) -> u64 {
        if self.block_time.as_millis() == 0 {
            return 0;
        }
        (self.block_time.as_millis() / self.specific.interval.as_millis()) as u64
    }
}
