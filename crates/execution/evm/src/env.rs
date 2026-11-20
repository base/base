use alloy_consensus::Header;
use alloy_primitives::U256;
use base_common_chains::{BaseUpgrade, BaseUpgradeExt, Upgrades};
use base_common_evm2::{BaseEvmTypes, BaseSpecId};
#[cfg(feature = "std")]
use base_common_rpc_types_engine::ExecutionData;
use evm2::env::BlockEnv;
use reth_chainspec::EthChainSpec;
use reth_evm_ethereum::EthEvmEnv;

use crate::BaseNextBlockEnvAttributes;

/// The resolved Base EVM environment: the active [`BaseSpecId`], the EVM version (chain ID, gas
/// schedule, feature toggles such as the nonce and balance checks, and the Azul EIP-7825
/// transaction gas cap that the Osaka version carries), and the block environment, whose extension
/// holds the block's L1 fee parameters.
pub type BaseEvmEnv = EthEvmEnv<BaseEvmTypes>;

/// Builds Base EVM environments from chain and payload metadata.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct BaseEvmEnvBuilder;

impl BaseEvmEnvBuilder {
    /// Builds a [`BaseEvmEnv`] for a block header using Base spec resolution.
    pub fn evm_env(header: &Header, chain_spec: &(impl Upgrades + EthChainSpec)) -> BaseEvmEnv {
        Self::from_block(
            chain_spec,
            BlockEnv::<BaseEvmTypes> {
                number: U256::from(header.number),
                beneficiary: header.beneficiary,
                timestamp: U256::from(header.timestamp),
                gas_limit: U256::from(header.gas_limit),
                basefee: U256::from(header.base_fee_per_gas.unwrap_or_default()),
                prevrandao: header.mix_hash.into(),
                ..Default::default()
            },
        )
    }

    /// Builds a [`BaseEvmEnv`] for the next block given a parent header.
    pub fn next_evm_env(
        parent: &Header,
        attributes: &BaseNextBlockEnvAttributes,
        base_fee_per_gas: u64,
        chain_spec: &(impl Upgrades + EthChainSpec),
    ) -> BaseEvmEnv {
        Self::from_block(
            chain_spec,
            BlockEnv::<BaseEvmTypes> {
                number: U256::from(parent.number.saturating_add(1)),
                beneficiary: attributes.suggested_fee_recipient,
                timestamp: U256::from(attributes.timestamp),
                gas_limit: U256::from(attributes.gas_limit),
                basefee: U256::from(base_fee_per_gas),
                prevrandao: attributes.prev_randao.into(),
                ..Default::default()
            },
        )
    }

    /// Builds a [`BaseEvmEnv`] for engine payload execution.
    #[cfg(feature = "std")]
    pub fn payload_evm_env(
        payload: &ExecutionData,
        chain_spec: &(impl Upgrades + EthChainSpec),
    ) -> BaseEvmEnv {
        let v1 = payload.payload.as_v1();
        Self::from_block(
            chain_spec,
            BlockEnv::<BaseEvmTypes> {
                number: U256::from(payload.payload.block_number()),
                beneficiary: v1.fee_recipient,
                timestamp: U256::from(payload.payload.timestamp()),
                gas_limit: U256::from(v1.gas_limit),
                basefee: v1.base_fee_per_gas,
                prevrandao: v1.prev_randao.into(),
                ..Default::default()
            },
        )
    }

    /// Resolves the Base spec active at `block`'s timestamp.
    ///
    /// Every Base upgrade is post-merge, so `DIFFICULTY` reads `prevrandao` and the difficulty
    /// field stays zero. Base has no blobs, so `BLOBBASEFEE` (Cancun onwards) reads the default
    /// minimum blob base fee of 1, matching the revm reference. The L1 fee parameters extension
    /// is loaded from the `L1Block` predeploy during execution.
    pub fn from_block(
        chain_spec: &(impl Upgrades + EthChainSpec),
        block: BlockEnv<BaseEvmTypes>,
    ) -> BaseEvmEnv {
        let timestamp = block.timestamp.saturating_to();
        let spec = BaseSpecId::new(BaseUpgrade::from_timestamp(chain_spec, timestamp));
        BaseEvmEnv::new(spec, block, chain_spec.chain().id())
    }
}
