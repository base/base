//! Loads chain configuration.

use std::collections::BTreeMap;

use alloy_eip2124::Head;
use alloy_eips::{
    eip4844::BLOB_TX_MIN_BLOB_GASPRICE,
    eip7840::BlobParams,
    eip7910::{EthConfig, EthForkConfig, SystemContract},
};
use alloy_primitives::Address;
use base_common_chain_config::Upgrades;
use base_common_chain_config::{BaseChainSpec, ChainSpecProvider};
use base_common_types_chain::BlockHeader;
use base_execution_evm_blocks::{BaseEvmConfig, Evm, PrecompilesMap};
use base_execution_evm_runtime::Precompile;
use base_execution_evm_runtime::database::EmptyDB;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth_primitives_traits::header::HeaderMut;
use reth_rpc_eth_types::EthApiError;
use reth_storage_api::BlockReaderIdExt;
use reth_storage_errors::provider::ProviderError;

/// RPC endpoint support for [EIP-7910](https://eips.ethereum.org/EIPS/eip-7910)
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "eth"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "eth"))]
pub trait BaseEthConfigApi {
    /// Returns an object with data about recent and upcoming fork configurations.
    #[method(name = "config")]
    fn config(&self) -> RpcResult<EthConfig>;
}

/// Handler for the `eth_config` RPC endpoint.
///
/// Ref: <https://eips.ethereum.org/EIPS/eip-7910>
#[derive(Debug, Clone)]
pub struct BaseEthConfigHandler<Provider> {
    provider: Provider,
    evm_config: BaseEvmConfig,
}

impl<Provider> BaseEthConfigHandler<Provider>
where
    Provider: ChainSpecProvider + BlockReaderIdExt + 'static,
{
    /// Creates a new [`BaseEthConfigHandler`].
    pub const fn new(provider: Provider, evm_config: BaseEvmConfig) -> Self {
        Self { provider, evm_config }
    }

    /// Returns the current and scheduled Base fork configurations.
    pub fn config(&self) -> Result<EthConfig, EthApiError> {
        let chain_spec = self.provider.chain_spec();
        let latest = self
            .provider
            .latest_header()?
            .ok_or_else(|| ProviderError::BestBlockNotFound)?
            .into_header();

        let current_precompiles = Self::evm_to_precompiles_map(
            self.evm_config
                .evm_for_block(EmptyDB::default(), &latest)
                .map_err(|error| reth_rpc_eth_types::EthApiError::Internal(error.into()))?,
        );

        let mut fork_timestamps =
            chain_spec.forks_iter().filter_map(|(_, cond)| cond.as_timestamp()).collect::<Vec<_>>();
        fork_timestamps.sort_unstable();
        fork_timestamps.dedup();

        let current_fork_idx = match fork_timestamps.iter().position(|ts| &latest.timestamp() < ts)
        {
            // All forks are in the past, use the last one.
            None => fork_timestamps.len().checked_sub(1),
            // First fork hasn't activated yet — no active timestamp fork.
            Some(0) => None,
            // Found a future fork; current is the one right before it.
            Some(idx) => Some(idx - 1),
        };
        let (current_fork_idx, current_fork_timestamp) = current_fork_idx
            .and_then(|idx| fork_timestamps.get(idx).map(|ts| (idx, *ts)))
            .ok_or_else(|| EthApiError::Internal("no active timestamp fork found".into()))?;

        let current =
            BaseForkConfig::build(&chain_spec, current_fork_timestamp, current_precompiles);

        let mut config = EthConfig { current, next: None, last: None };

        if let Some(next_fork_timestamp) = fork_timestamps.get(current_fork_idx + 1).copied() {
            let fake_header = {
                let mut header = latest.clone();
                header.set_timestamp(next_fork_timestamp);
                header
            };
            let next_precompiles = Self::evm_to_precompiles_map(
                self.evm_config
                    .evm_for_block(EmptyDB::default(), &fake_header)
                    .map_err(|error| reth_rpc_eth_types::EthApiError::Internal(error.into()))?,
            );

            config.next =
                Some(BaseForkConfig::build(&chain_spec, next_fork_timestamp, next_precompiles));
        } else {
            // If there is no fork scheduled, there is no "last" or "final" fork scheduled.
            return Ok(config);
        }

        let last_fork_timestamp = fork_timestamps.last().copied().unwrap();
        let fake_header = {
            let mut header = latest;
            header.set_timestamp(last_fork_timestamp);
            header
        };
        let last_precompiles = Self::evm_to_precompiles_map(
            self.evm_config
                .evm_for_block(EmptyDB::default(), &fake_header)
                .map_err(|error| reth_rpc_eth_types::EthApiError::Internal(error.into()))?,
        );

        config.last =
            Some(BaseForkConfig::build(&chain_spec, last_fork_timestamp, last_precompiles));

        Ok(config)
    }
}

impl<Provider> BaseEthConfigApiServer for BaseEthConfigHandler<Provider>
where
    Provider: ChainSpecProvider + BlockReaderIdExt + 'static,
{
    fn config(&self) -> RpcResult<EthConfig> {
        Ok(self.config().map_err(EthApiError::from)?)
    }
}

impl<Provider> BaseEthConfigHandler<Provider> {
    /// Lists the precompiles available in an execution environment.
    pub fn evm_to_precompiles_map(
        evm: impl Evm<Precompiles = PrecompilesMap>,
    ) -> BTreeMap<String, Address> {
        let precompiles = evm.precompiles();
        precompiles
            .addresses()
            .filter_map(|address| {
                Some((precompiles.get(address)?.precompile_id().name().to_string(), *address))
            })
            .collect()
    }
}

/// Constructs the Base rules advertised for an individual fork.
#[derive(Debug)]
pub struct BaseForkConfig;

impl BaseForkConfig {
    /// Returns the wire-compatible blob schedule with no native blob capacity.
    pub const fn blob_params() -> BlobParams {
        BlobParams {
            target_blob_count: 0,
            max_blob_count: 0,
            update_fraction: 0,
            // EIP-7840's serde shape omits this field, so clients round-trip a missing value back to
            // the protocol default of `1`. Keep the wire-observable default aligned while zeroing the
            // blob capacity fields that Base must not advertise.
            min_blob_fee: BLOB_TX_MIN_BLOB_GASPRICE,
            max_blobs_per_tx: 0,
            blob_base_cost: 0,
        }
    }

    /// Builds a fork response directly from the Base upgrade schedule.
    pub fn build(
        chain_spec: &BaseChainSpec,
        timestamp: u64,
        precompiles: BTreeMap<String, Address>,
    ) -> EthForkConfig {
        let mut system_contracts = BTreeMap::new();
        if chain_spec.is_ecotone_active_at_timestamp(timestamp) {
            system_contracts.extend(SystemContract::cancun());
        }
        if chain_spec.is_isthmus_active_at_timestamp(timestamp) {
            system_contracts.extend(
                SystemContract::prague(None)
                    .into_iter()
                    .filter(|(contract, _)| *contract == SystemContract::HistoryStorage),
            );
        }
        EthForkConfig {
            activation_time: timestamp,
            blob_schedule: Self::blob_params(),
            chain_id: chain_spec.chain().id(),
            fork_id: chain_spec
                .fork_id(&Head { timestamp, number: u64::MAX, ..Default::default() })
                .hash
                .0
                .into(),
            precompiles,
            system_contracts,
        }
    }
}
#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use alloy_eips::{eip4844::BLOB_TX_MIN_BLOB_GASPRICE, eip7910::SystemContract};
    use base_common_chain_config::BaseChainSpecBuilder;

    use super::BaseForkConfig;

    #[test]
    fn ecotone_only_keeps_beacon_roots() {
        let chain_spec = BaseChainSpecBuilder::base_mainnet().ecotone_activated().build();
        let fork_config = BaseForkConfig::build(&chain_spec, 0, BTreeMap::new());

        assert_eq!(
            fork_config.system_contracts.keys().cloned().collect::<Vec<_>>(),
            vec![SystemContract::BeaconRoots]
        );
    }

    #[test]
    fn isthmus_keeps_beacon_roots_and_history_storage() {
        let chain_spec = BaseChainSpecBuilder::base_mainnet().isthmus_activated().build();
        let fork_config = BaseForkConfig::build(&chain_spec, 0, BTreeMap::new());

        assert_eq!(
            fork_config.system_contracts.keys().cloned().collect::<Vec<_>>(),
            vec![SystemContract::BeaconRoots, SystemContract::HistoryStorage]
        );
    }

    #[test]
    fn zero_blob_params_zeroes_blob_capacity_fields() {
        let params = BaseForkConfig::blob_params();

        assert_eq!(params.target_blob_count, 0);
        assert_eq!(params.max_blob_count, 0);
        assert_eq!(params.update_fraction, 0);
        assert_eq!(params.min_blob_fee, BLOB_TX_MIN_BLOB_GASPRICE);
        assert_eq!(params.max_blobs_per_tx, 0);
        assert_eq!(params.blob_base_cost, 0);
    }
}
