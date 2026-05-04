use alloy_primitives::{address, b256, Address, B256, U256};

pub const MAX_SEQUENCER_DRIFT: u64 = 20;
pub const SEQ_WINDOW_SIZE: u64 = 24;
pub const CHANNEL_TIMEOUT: u64 = 120;
pub const L1_CHAIN_ID: u64 = 1;
pub const BATCH_INBOX_ADDRESS: Address = address!("0000000000000000000000000000000000000001");
pub const EIP1559_ELASTICITY: u64 = 50;
pub const EIP1559_DENOMINATOR: u64 = 1;

pub const SUGGESTED_FEE_RECIPIENT: Address =
    address!("4200000000000000000000000000000000000011");

pub const DEFAULT_GAS_LIMIT: u64 = 30_000_000;
pub const SETUP_GAS_LIMIT: u64 = 1_000_000_000;

pub const BATCHER_KEY: B256 =
    b256!("d2ba8e70072983384203c438d4e94bf399cbd88bbcafb82b61cc96ed12541707");

pub const PREFUND_KEY: B256 =
    b256!("ad0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

pub fn prefund_amount() -> U256 {
    U256::from(1_000_000u64) * U256::from(10u64).pow(U256::from(18u64))
}
