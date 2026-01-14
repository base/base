use core::{
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

use alloy_primitives::Address;

use crate::{args::OpRbuilderArgs, flashblocks::BuilderConfig};

/// Configuration values that are specific to the flashblocks builder.
#[allow(unnameable_types)]
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
    /// milliseconds.
    ///
    /// If dynamic_adjustment is false this value would be deducted from first flashblock and
    /// it shouldn't be more than interval
    ///
    /// If dynamic_adjustment is true this value would be deducted from first flashblock and
    /// it shouldn't be more than interval
    pub leeway_time: Duration,

    /// Disables dynamic flashblocks number adjustment based on FCU arrival time
    pub fixed: bool,

    /// Should we disable state root calculation for each flashblock
    pub disable_state_root: bool,

    /// The address of the flashblocks number contract.
    ///
    /// If set a builder tx will be added to the start of every flashblock instead of the regular builder tx.
    pub flashblocks_number_contract_address: Option<Address>,
}

impl Default for FlashblocksConfig {
    fn default() -> Self {
        Self {
            ws_addr: SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 1111),
            interval: Duration::from_millis(250),
            leeway_time: Duration::from_millis(50),
            fixed: false,
            disable_state_root: false,
            flashblocks_number_contract_address: None,
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

        let fixed = args.flashblocks.flashblocks_fixed;

        let disable_state_root = args.flashblocks.flashblocks_disable_state_root;

        let flashblocks_number_contract_address =
            args.flashblocks.flashblocks_number_contract_address;

        Ok(Self {
            ws_addr,
            interval,
            leeway_time,
            fixed,
            disable_state_root,
            flashblocks_number_contract_address,
        })
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
