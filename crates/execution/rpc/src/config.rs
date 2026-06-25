//! Base-specific `eth_config` RPC support.

use std::sync::Arc;

use alloy_eips::{
    eip4844::BLOB_TX_MIN_BLOB_GASPRICE,
    eip7840::BlobParams,
    eip7910::{EthConfig, EthForkConfig, SystemContract},
};
use base_common_chains::Upgrades;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth_chainspec::{ChainSpecProvider, EthereumHardforks, Hardforks};
use reth_evm::ConfigureEvm;
use reth_node_api::NodePrimitives;
use reth_primitives_traits::header::HeaderMut;
use reth_rpc_eth_api::helpers::config::{EthConfigApiServer, EthConfigHandler};
use reth_storage_api::BlockReaderIdExt;

const fn zero_blob_params() -> BlobParams {
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

fn sanitize_system_contracts_for_fork(chain_spec: &impl Upgrades, fork_config: &mut EthForkConfig) {
    let activation_time = fork_config.activation_time;

    fork_config.system_contracts.retain(|contract, _| match contract {
        SystemContract::BeaconRoots => chain_spec.is_ecotone_active_at_timestamp(activation_time),
        SystemContract::HistoryStorage => {
            chain_spec.is_isthmus_active_at_timestamp(activation_time)
        }
        // Base does not support L1-style deposit, consolidation, or withdrawal request contracts.
        SystemContract::ConsolidationRequestPredeploy
        | SystemContract::DepositContract
        | SystemContract::WithdrawalRequestPredeploy => false,
        SystemContract::Other(_) => true,
    });
}

fn for_each_fork(config: &mut EthConfig, mut f: impl FnMut(&mut EthForkConfig)) {
    f(&mut config.current);

    if let Some(next) = config.next.as_mut() {
        f(next);
    }

    if let Some(last) = config.last.as_mut() {
        f(last);
    }
}

/// RPC endpoint support for Base's `eth_config` response.
///
/// Base does not currently support native blob transactions, so its `blobSchedule` should be
/// reported as zeroed rather than inheriting synthetic Cancun/Prague/Osaka defaults from the
/// upstream Ethereum handler.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "eth"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "eth"))]
pub trait BaseEthConfigApi {
    /// Returns an object with data about recent and upcoming fork configurations.
    #[method(name = "config")]
    fn config(&self) -> RpcResult<EthConfig>;
}

/// Base-specific handler for the `eth_config` RPC endpoint.
#[derive(Debug, Clone)]
pub struct BaseEthConfigHandler<Provider: ChainSpecProvider, Evm> {
    chain_spec: Arc<<Provider as ChainSpecProvider>::ChainSpec>,
    eth_config: EthConfigHandler<Provider, Evm>,
}

impl<Provider, Evm> BaseEthConfigHandler<Provider, Evm>
where
    Provider: ChainSpecProvider<ChainSpec: Hardforks + EthereumHardforks + Upgrades>
        + BlockReaderIdExt<Header: HeaderMut>
        + Clone
        + 'static,
    Evm: ConfigureEvm<Primitives: NodePrimitives<BlockHeader = Provider::Header>> + Clone + 'static,
{
    /// Creates a new [`BaseEthConfigHandler`].
    pub fn new(provider: Provider, evm_config: Evm) -> Self {
        let chain_spec = provider.chain_spec();
        let eth_config = EthConfigHandler::new(provider, evm_config);
        Self { chain_spec, eth_config }
    }

    fn sanitize_blob_schedules(&self, config: &mut EthConfig) {
        for_each_fork(config, |fork| {
            fork.blob_schedule = zero_blob_params();
        });
    }

    fn sanitize_system_contracts(&self, config: &mut EthConfig) {
        for_each_fork(config, |fork| {
            sanitize_system_contracts_for_fork(self.chain_spec.as_ref(), fork)
        });
    }
}

impl<Provider, Evm> BaseEthConfigApiServer for BaseEthConfigHandler<Provider, Evm>
where
    Provider: ChainSpecProvider<ChainSpec: Hardforks + EthereumHardforks + Upgrades>
        + BlockReaderIdExt<Header: HeaderMut>
        + Clone
        + 'static,
    Evm: ConfigureEvm<Primitives: NodePrimitives<BlockHeader = Provider::Header>> + Clone + 'static,
{
    fn config(&self) -> RpcResult<EthConfig> {
        let mut config = EthConfigApiServer::config(&self.eth_config)?;
        self.sanitize_blob_schedules(&mut config);
        self.sanitize_system_contracts(&mut config);
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use alloy_eips::{
        eip4844::BLOB_TX_MIN_BLOB_GASPRICE,
        eip7910::{EthForkConfig, SystemContract},
    };
    use base_execution_chainspec::BaseChainSpecBuilder;

    use super::{sanitize_system_contracts_for_fork, zero_blob_params};

    fn prague_system_contracts() -> BTreeMap<SystemContract, alloy_primitives::Address> {
        SystemContract::cancun().into_iter().chain(SystemContract::prague(None)).collect()
    }

    fn fork_config(activation_time: u64) -> EthForkConfig {
        EthForkConfig {
            activation_time,
            blob_schedule: zero_blob_params(),
            chain_id: 1,
            fork_id: Default::default(),
            precompiles: Default::default(),
            system_contracts: prague_system_contracts(),
        }
    }

    #[test]
    fn ecotone_only_keeps_beacon_roots() {
        let chain_spec = BaseChainSpecBuilder::base_mainnet().ecotone_activated().build();
        let mut fork_config = fork_config(0);

        sanitize_system_contracts_for_fork(&chain_spec, &mut fork_config);

        assert_eq!(
            fork_config.system_contracts.keys().cloned().collect::<Vec<_>>(),
            vec![SystemContract::BeaconRoots]
        );
    }

    #[test]
    fn isthmus_keeps_beacon_roots_and_history_storage() {
        let chain_spec = BaseChainSpecBuilder::base_mainnet().isthmus_activated().build();
        let mut fork_config = fork_config(0);

        sanitize_system_contracts_for_fork(&chain_spec, &mut fork_config);

        assert_eq!(
            fork_config.system_contracts.keys().cloned().collect::<Vec<_>>(),
            vec![SystemContract::BeaconRoots, SystemContract::HistoryStorage]
        );
    }

    #[test]
    fn zero_blob_params_zeroes_blob_capacity_fields() {
        let params = zero_blob_params();

        assert_eq!(params.target_blob_count, 0);
        assert_eq!(params.max_blob_count, 0);
        assert_eq!(params.update_fraction, 0);
        assert_eq!(params.min_blob_fee, BLOB_TX_MIN_BLOB_GASPRICE);
        assert_eq!(params.max_blobs_per_tx, 0);
        assert_eq!(params.blob_base_cost, 0);
    }
}
