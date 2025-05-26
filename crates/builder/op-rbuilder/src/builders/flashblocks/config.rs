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
}

impl Default for FlashblocksConfig {
    fn default() -> Self {
        Self {
            ws_addr: SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 1111),
            interval: Duration::from_millis(250),
        }
    }
}

impl TryFrom<OpRbuilderArgs> for FlashblocksConfig {
    type Error = eyre::Report;

    fn try_from(args: OpRbuilderArgs) -> Result<Self, Self::Error> {
        let ws_addr = args
            .flashblocks_ws_url
            .parse()
            .map_err(|_| eyre::eyre!("Invalid flashblocks websocket address"))?;

        let interval = Duration::from_millis(args.flashblock_block_time);

        Ok(Self { ws_addr, interval })
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
