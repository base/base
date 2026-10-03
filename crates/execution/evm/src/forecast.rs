//! Pending block forecasts for the Denim timestamp schedule.

use alloy_consensus::BlockHeader;
use alloy_rpc_types_eth::BlockId;
use base_execution_chainspec::{BaseChainSpec, BaseChainSpecError};
use reth_chainspec::ChainSpecProvider;
use reth_errors::RethError;
use reth_evm::{ConfigureEvm, EvmEnvFor};
use reth_rpc_eth_api::{
    FromEthApiError,
    helpers::{LoadState, SpawnBlocking},
};
use reth_rpc_eth_types::{PendingBlockEnv, PendingBlockEnvOrigin};

use crate::BaseNextBlockEnvAttributes;

/// A pending successor block's scheduled timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BasePendingForecast {
    /// Successor block timestamp in seconds.
    pub timestamp: u64,
}

impl BasePendingForecast {
    /// Returns block `parent_number + 1` on the chain's timestamp schedule, or `None` when the
    /// chain has no Denim schedule.
    pub fn successor(
        chain_spec: &BaseChainSpec,
        parent_number: u64,
    ) -> Result<Option<Self>, BaseChainSpecError> {
        Ok(chain_spec.denim_timestamp_schedule()?.map(|schedule| Self {
            timestamp: schedule.block_timestamp_parts(parent_number.saturating_add(1)).0,
        }))
    }

    /// Resolves an EVM environment like [`LoadState::evm_env_at`], but builds pending state
    /// derived from latest at the scheduled successor timestamp.
    pub async fn evm_env_at<Eth>(
        eth: &Eth,
        at: BlockId,
    ) -> Result<(EvmEnvFor<Eth::Evm>, BlockId), Eth::Error>
    where
        Eth: LoadState + SpawnBlocking,
        Eth::Evm: ConfigureEvm<NextBlockEnvCtx = BaseNextBlockEnvAttributes>,
        Eth::Provider: ChainSpecProvider<ChainSpec = BaseChainSpec>,
    {
        if !at.is_pending() {
            return eth.evm_env_at(at).await;
        }

        let PendingBlockEnv { evm_env, origin } = eth.pending_block_env_and_cfg()?;
        let state_at = origin.state_block_id();
        let PendingBlockEnvOrigin::DerivedFromLatest(parent) = origin else {
            return Ok((evm_env, state_at));
        };
        let forecast = Self::successor(eth.provider().chain_spec().as_ref(), parent.number())
            .map_err(RethError::other)
            .map_err(Eth::Error::from_eth_err)?;
        let Some(forecast) = forecast else { return Ok((evm_env, state_at)) };
        let mut attributes = eth
            .pending_env_builder()
            .pending_env_attributes(&parent, None)
            .map_err(Eth::Error::from_eth_err)?;
        attributes.timestamp = forecast.timestamp;
        let evm_env = eth
            .evm_config()
            .next_evm_env(&parent, &attributes)
            .map_err(RethError::other)
            .map_err(Eth::Error::from_eth_err)?;
        Ok((evm_env, state_at))
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Header;
    use base_common_chains::BaseUpgrade;
    use reth_chainspec::{ForkCondition, Hardfork};
    use reth_primitives_traits::SealedHeader;

    use super::*;

    #[test]
    fn successor_follows_the_runtime_schedule() {
        let mut spec = BaseChainSpec::devnet();
        assert_eq!(BasePendingForecast::successor(&spec, 0).unwrap(), None);

        spec.inner.genesis_header =
            SealedHeader::seal_slow(Header { number: 100, timestamp: 11, ..Default::default() });
        spec.block_time = Some(3);
        // The first legacy slot at or after 15s is block 102 at 17s.
        spec.inner.hardforks.insert(BaseUpgrade::Denim.boxed(), ForkCondition::Timestamp(15));
        for (parent, timestamp) in [(100, 14), (101, 17), (105, 17), (106, 18)] {
            assert_eq!(
                BasePendingForecast::successor(&spec, parent).unwrap(),
                Some(BasePendingForecast { timestamp }),
                "successor of {parent}"
            );
        }

        spec.block_time = None;
        assert!(BasePendingForecast::successor(&spec, 0).is_err());
    }
}
