//! OP Mainnet Rollup Config.

use alloy_eips::BlockNumHash;
use alloy_primitives::{address, b256, uint};
use op_alloy_genesis::{ChainGenesis, RollupConfig, SystemConfig, OP_MAINNET_BASE_FEE_PARAMS};

/// The [RollupConfig] for OP Mainnet.
pub const OP_MAINNET_CONFIG: RollupConfig = RollupConfig {
    genesis: ChainGenesis {
        l1: BlockNumHash {
            hash: b256!("438335a20d98863a4c0c97999eb2481921ccd28553eac6f913af7c12aec04108"),
            number: 17_422_590_u64,
        },
        l2: BlockNumHash {
            hash: b256!("dbf6a80fef073de06add9b0d14026d6e5a86c85f6d102c36d3d8e9cf89c2afd3"),
            number: 105_235_063_u64,
        },
        l2_time: 1_686_068_903_u64,
        system_config: Some(SystemConfig {
            batcher_address: address!("6887246668a3b87f54deb3b94ba47a6f63f32985"),
            overhead: uint!(0xbc_U256),
            scalar: uint!(0xa6fe0_U256),
            gas_limit: 30_000_000_u64,
            base_fee_scalar: None,
            blob_base_fee_scalar: None,
            eip1559_denominator: None,
            eip1559_elasticity: None,
        }),
    },
    block_time: 2_u64,
    max_sequencer_drift: 600_u64,
    seq_window_size: 3600_u64,
    channel_timeout: 300_u64,
    granite_channel_timeout: 50,
    l1_chain_id: 1_u64,
    l2_chain_id: 10_u64,
    base_fee_params: OP_MAINNET_BASE_FEE_PARAMS.as_base_fee_params(),
    canyon_base_fee_params: OP_MAINNET_BASE_FEE_PARAMS.as_canyon_base_fee_params(),
    regolith_time: Some(0_u64),
    canyon_time: Some(1_704_992_401_u64),
    delta_time: Some(1_708_560_000_u64),
    ecotone_time: Some(1_710_374_401_u64),
    fjord_time: Some(1_720_627_201_u64),
    granite_time: Some(1_726_070_401_u64),
    holocene_time: None,
    isthmus_time: None,
    batch_inbox_address: address!("ff00000000000000000000000000000000000010"),
    deposit_contract_address: address!("beb5fc579115071764c7423a4f12edde41f106ed"),
    l1_system_config_address: address!("229047fed2591dbec1ef1118d64f7af3db9eb290"),
    protocol_versions_address: address!("8062abc286f5e7d9428a0ccb9abd71e50d93b935"),
    superchain_config_address: Some(address!("95703e0982140D16f8ebA6d158FccEde42f04a4C")),
    da_challenge_address: None,
    blobs_enabled_l1_timestamp: None,
};
