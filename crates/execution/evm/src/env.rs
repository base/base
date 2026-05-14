use alloy_consensus::Header;
use alloy_evm::EvmEnv;
use alloy_primitives::U256;
use base_common_chains::Upgrades;
use base_common_evm::BaseSpecId;
#[cfg(feature = "std")]
use base_common_rpc_types_engine::ExecutionData;
use reth_chainspec::EthChainSpec;
use reth_primitives_traits::constants::MAX_TX_GAS_LIMIT_OSAKA;
use revm::{
    context::{BlockEnv, CfgEnv},
    context_interface::block::BlobExcessGasAndPrice,
    primitives::hardfork::SpecId,
};

use crate::BaseNextBlockEnvAttributes;

/// Builds Base EVM environments from chain and payload metadata.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct BaseEvmEnvBuilder;

impl BaseEvmEnvBuilder {
    /// Builds a [`CfgEnv`] for a Base chain spec at `timestamp`.
    pub fn cfg_env(
        spec: BaseSpecId,
        timestamp: u64,
        chain_spec: &(impl Upgrades + EthChainSpec),
    ) -> CfgEnv<BaseSpecId> {
        let mut cfg_env = CfgEnv::new()
            .with_chain_id(chain_spec.chain().id())
            .with_spec_and_mainnet_gas_params(spec);

        if chain_spec.is_azul_active_at_timestamp(timestamp) {
            cfg_env.tx_gas_limit_cap = Some(MAX_TX_GAS_LIMIT_OSAKA);
        }

        cfg_env
    }

    /// Builds an [`EvmEnv`] for a block header using Base spec resolution.
    pub fn evm_env(
        header: &Header,
        chain_spec: &(impl Upgrades + EthChainSpec),
    ) -> EvmEnv<BaseSpecId> {
        let spec = BaseSpecId::from_header(chain_spec, header);
        let cfg_env = Self::cfg_env(spec, header.timestamp, chain_spec);
        let blob_excess_gas_and_price = Self::blob_excess_gas_and_price(spec);
        let is_merge = spec.into_eth_spec() >= SpecId::MERGE;

        let block_env = BlockEnv {
            number: U256::from(header.number),
            beneficiary: header.beneficiary,
            timestamp: U256::from(header.timestamp),
            difficulty: if is_merge { U256::ZERO } else { header.difficulty },
            prevrandao: if is_merge { Some(header.mix_hash) } else { None },
            gas_limit: header.gas_limit,
            basefee: header.base_fee_per_gas.unwrap_or_default(),
            blob_excess_gas_and_price,
        };

        EvmEnv { cfg_env, block_env }
    }

    /// Builds an [`EvmEnv`] for the next block given a parent header.
    pub fn next_evm_env(
        parent: &Header,
        attributes: &BaseNextBlockEnvAttributes,
        base_fee_per_gas: u64,
        chain_spec: &(impl Upgrades + EthChainSpec),
    ) -> EvmEnv<BaseSpecId> {
        let spec = BaseSpecId::from_timestamp(chain_spec, attributes.timestamp);
        let cfg_env = Self::cfg_env(spec, attributes.timestamp, chain_spec);
        let blob_excess_gas_and_price = Self::blob_excess_gas_and_price(spec);
        let is_merge = spec.into_eth_spec() >= SpecId::MERGE;

        let block_env = BlockEnv {
            number: U256::from(parent.number.saturating_add(1)),
            beneficiary: attributes.suggested_fee_recipient,
            timestamp: U256::from(attributes.timestamp),
            difficulty: if is_merge { U256::ZERO } else { parent.difficulty },
            prevrandao: if is_merge { Some(attributes.prev_randao) } else { None },
            gas_limit: attributes.gas_limit,
            basefee: base_fee_per_gas,
            blob_excess_gas_and_price,
        };

        EvmEnv { cfg_env, block_env }
    }

    /// Builds an [`EvmEnv`] for engine payload execution.
    #[cfg(feature = "std")]
    pub fn payload_evm_env(
        payload: &ExecutionData,
        chain_spec: &(impl Upgrades + EthChainSpec),
    ) -> EvmEnv<BaseSpecId> {
        let timestamp = payload.payload.timestamp();
        let block_number = payload.payload.block_number();
        let spec = BaseSpecId::from_timestamp(chain_spec, timestamp);
        let cfg_env = Self::cfg_env(spec, timestamp, chain_spec);
        let blob_excess_gas_and_price = Self::blob_excess_gas_and_price(spec);
        let is_merge = spec.into_eth_spec() >= SpecId::MERGE;

        let block_env = BlockEnv {
            number: U256::from(block_number),
            beneficiary: payload.payload.as_v1().fee_recipient,
            timestamp: U256::from(timestamp),
            difficulty: if is_merge {
                U256::ZERO
            } else {
                payload.payload.as_v1().prev_randao.into()
            },
            prevrandao: is_merge.then(|| payload.payload.as_v1().prev_randao),
            gas_limit: payload.payload.as_v1().gas_limit,
            basefee: payload.payload.as_v1().base_fee_per_gas.to(),
            blob_excess_gas_and_price,
        };

        EvmEnv { cfg_env, block_env }
    }

    /// Returns the EIP-4844 blob gas values expected for a Base spec.
    pub fn blob_excess_gas_and_price(spec: BaseSpecId) -> Option<BlobExcessGasAndPrice> {
        spec.into_eth_spec()
            .is_enabled_in(SpecId::CANCUN)
            .then_some(BlobExcessGasAndPrice { excess_blob_gas: 0, blob_gasprice: 1 })
    }
}
