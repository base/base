//! Base Mainnet Rollup Config.

use alloy_eips::BlockNumHash;
use alloy_primitives::{address, b256, uint};
use op_alloy_genesis::{ChainGenesis, RollupConfig, SystemConfig, OP_MAINNET_BASE_FEE_PARAMS};

/// The [RollupConfig] for Base Mainnet.
pub const BASE_MAINNET_CONFIG: RollupConfig = RollupConfig {
    genesis: ChainGenesis {
        l1: BlockNumHash {
            hash: b256!("5c13d307623a926cd31415036c8b7fa14572f9dac64528e857a470511fc30771"),
            number: 17_481_768_u64,
        },
        l2: BlockNumHash {
            hash: b256!("f712aa9241cc24369b143cf6dce85f0902a9731e70d66818a3a5845b296c73dd"),
            number: 0_u64,
        },
        l2_time: 1686789347_u64,
        system_config: Some(SystemConfig {
            batcher_address: address!("5050f69a9786f081509234f1a7f4684b5e5b76c9"),
            overhead: uint!(0xbc_U256),
            scalar: uint!(0xa6fe0_U256),
            gas_limit: 30_000_000_u64,
            base_fee_scalar: None,
            blob_base_fee_scalar: None,
            eip1559_denominator: None,
            eip1559_elasticity: None,
        }),
    },
    block_time: 2,
    max_sequencer_drift: 600,
    seq_window_size: 3600,
    channel_timeout: 300,
    granite_channel_timeout: 50,
    l1_chain_id: 1,
    l2_chain_id: 8453,
    base_fee_params: OP_MAINNET_BASE_FEE_PARAMS.as_base_fee_params(),
    canyon_base_fee_params: OP_MAINNET_BASE_FEE_PARAMS.as_canyon_base_fee_params(),
    regolith_time: Some(0_u64),
    canyon_time: Some(1704992401),
    delta_time: Some(1708560000),
    ecotone_time: Some(1710374401),
    fjord_time: Some(1720627201),
    granite_time: Some(1_726_070_401_u64),
    holocene_time: None,
    isthmus_time: None,
    batch_inbox_address: address!("ff00000000000000000000000000000000008453"),
    deposit_contract_address: address!("49048044d57e1c92a77f79988d21fa8faf74e97e"),
    l1_system_config_address: address!("73a79fab69143498ed3712e519a88a918e1f4072"),
    protocol_versions_address: address!("8062abc286f5e7d9428a0ccb9abd71e50d93b935"),
    superchain_config_address: Some(address!("95703e0982140D16f8ebA6d158FccEde42f04a4C")),
    da_challenge_address: None,
    blobs_enabled_l1_timestamp: None,
};
