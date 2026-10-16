//! Shared fixtures for unit tests across the crate.

use alloy_consensus::Header;
use alloy_genesis::ChainConfig;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, address, bytes, hex};
use base_common_genesis::{
    BaseUpgradeConfig, ChainGenesis, RollupConfig, SystemConfig, UpgradeConfig,
};

use crate::{
    AccountChange, Call, Delegation, Eip8130Signed, L1BlockInfoBedrock, L1BlockInfoEcotone,
    L1BlockInfoIsthmus, L1BlockInfoTx, TxDeposit, TxEip8130,
};

pub(crate) const RAW_BEDROCK_INFO_TX: [u8; L1BlockInfoBedrock::L1_INFO_TX_LEN] = hex!(
    "015d8eb9000000000000000000000000000000000000000000000000000000000117c4eb0000000000000000000000000000000000000000000000000000000065280377000000000000000000000000000000000000000000000000000000026d05d953392012032675be9f94aae5ab442de73c5f4fb1bf30fa7dd0d2442239899a40fc00000000000000000000000000000000000000000000000000000000000000040000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f3298500000000000000000000000000000000000000000000000000000000000000bc00000000000000000000000000000000000000000000000000000000000a6fe0"
);
pub(crate) const RAW_ECOTONE_INFO_TX: [u8; L1BlockInfoEcotone::L1_INFO_TX_LEN] = hex!(
    "440a5e2000000558000c5fc5000000000000000500000000661c277300000000012bec20000000000000000000000000000000000000000000000000000000026e9f109900000000000000000000000000000000000000000000000000000000000000011c4c84c50740386c7dc081efddd644405f04cde73e30a2e381737acce9f5add30000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f32985"
);
pub(crate) const RAW_ISTHMUS_INFO_TX: [u8; L1BlockInfoIsthmus::L1_INFO_TX_LEN] = hex!(
    "098999be00000558000c5fc5000000000000000500000000661c277300000000012bec20000000000000000000000000000000000000000000000000000000026e9f109900000000000000000000000000000000000000000000000000000000000000011c4c84c50740386c7dc081efddd644405f04cde73e30a2e381737acce9f5add30000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f329850000abcd000000000000dcba"
);

/// A deposit with every field set to a distinct, non-default value.
pub(crate) fn sample_deposit() -> TxDeposit {
    TxDeposit {
        source_hash: B256::with_last_byte(2),
        from: Address::repeat_byte(0x42),
        to: TxKind::Call(Address::repeat_byte(0x11)),
        mint: 1,
        value: U256::from(1u64),
        gas_limit: 50_000,
        ..Default::default()
    }
}

/// A signed EIP-8130 transaction, self-paying or sponsored by `0xcc`.
pub(crate) fn sample_signed_8130(payer_present: bool) -> Eip8130Signed {
    let tx = TxEip8130 {
        chain_id: 8453,
        sender: Some(address!("0x00000000000000000000000000000000000000aa")),
        nonce_key: U256::from(7u64),
        nonce_sequence: 3,
        max_priority_fee_per_gas: 1_000_000_000,
        max_fee_per_gas: 5_000_000_000,
        gas_limit: 250_000,
        account_changes: vec![AccountChange::Delegation(Delegation { target: Address::ZERO })],
        calls: vec![vec![Call {
            to: address!("0x00000000000000000000000000000000000000bb"),
            value: U256::ZERO,
            data: bytes!("01020304"),
        }]],
        metadata: bytes!("c0ffee"),
        payer: payer_present.then(|| address!("0x00000000000000000000000000000000000000cc")),
        ..Default::default()
    };
    Eip8130Signed::new(
        tx,
        bytes!("deadbeef"),
        if payer_present { bytes!("cafebabe") } else { Bytes::new() },
    )
}

/// Rollup config where Denim activates at `14`, with 2 second blocks from an `l2_time` of `10`.
pub(crate) fn denim_rollup_config() -> RollupConfig {
    RollupConfig {
        genesis: ChainGenesis { l2_time: 10, ..Default::default() },
        block_time: 2,
        upgrades: UpgradeConfig {
            base: BaseUpgradeConfig { denim: Some(14), ..Default::default() },
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Rollup config with Isthmus active from timestamp `1`.
pub(crate) fn isthmus_rollup_config() -> RollupConfig {
    RollupConfig {
        upgrades: UpgradeConfig { isthmus_time: Some(1), ..Default::default() },
        ..Default::default()
    }
}

/// System config with a batcher address and operator fee parameters set.
pub(crate) fn isthmus_system_config() -> SystemConfig {
    SystemConfig {
        batcher_address: address!("6887246668a3b87f54deb3b94ba47a6f63f32985"),
        operator_fee_scalar: Some(0xabcd),
        operator_fee_constant: Some(0xdcba),
        ..Default::default()
    }
}

/// L1 header matching the Isthmus block info fixtures.
pub(crate) fn isthmus_l1_header() -> Header {
    Header {
        number: 19655712,
        timestamp: 1713121139,
        base_fee_per_gas: Some(10445852825),
        ..Default::default()
    }
}

/// Builds an [`L1BlockInfoTx`] at sequence number `0`, with the parent block two seconds before
/// `l2_block_time`.
pub(crate) fn try_new_info(
    rollup_config: &RollupConfig,
    l1_config: &ChainConfig,
    system_config: &SystemConfig,
    l1_header: &Header,
    l2_block_time: u64,
) -> L1BlockInfoTx {
    L1BlockInfoTx::try_new(
        rollup_config,
        l1_config,
        system_config,
        0,
        l1_header,
        l2_block_time.saturating_sub(2),
        l2_block_time,
    )
    .unwrap()
}
