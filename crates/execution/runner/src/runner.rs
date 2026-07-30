//! Contains the [`BaseNodeRunner`], which is responsible for configuring and launching a Base node.

use std::fmt;

use base_execution_payload_builder::config::{BaseDAConfig, GasLimitConfig};
use base_node_core::args::RollupArgs;
use eyre::Result;
use reth_node_builder::{Node, NodeHandle, NodeHandleFor};
use reth_provider::providers::BlockchainProvider;
use tracing::info;

use crate::{
    BaseNodeBuilder, BaseNodeExtension, FromExtensionConfig, NodeHooks,
    node::BaseNode,
    service::{DefaultPayloadServiceBuilder, PayloadServiceBuilder},
};

type StartedCallback = Box<dyn FnOnce() -> Result<()> + Send + 'static>;

/// Handle to a launched Base execution node.
#[derive(Debug)]
pub struct LaunchedBaseNode {
    /// The underlying reth node handle.
    pub handle: NodeHandleFor<BaseNode>,
}

/// Wraps the Base node configuration and orchestrates builder wiring.
pub struct BaseNodeRunner<SB: PayloadServiceBuilder = DefaultPayloadServiceBuilder> {
    /// Rollup-specific arguments forwarded to the Base node implementation.
    rollup_args: RollupArgs,
    /// Registered builder extensions.
    extensions: Vec<Box<dyn BaseNodeExtension>>,
    /// Payload service builder.
    service_builder: SB,
    /// Shared DA configuration for the node and payload builder.
    da_config: Option<BaseDAConfig>,
    /// Shared gas-limit configuration for the node and payload builder.
    gas_limit_config: Option<GasLimitConfig>,
    /// Whether to drop positively stale EIP-8130 transactions using their
    /// captured authorization manifest before execution.
    manifest_precheck_enabled: bool,
    /// Binary-owned callbacks to run after the node has started.
    started_callbacks: Vec<StartedCallback>,
}

impl BaseNodeRunner<DefaultPayloadServiceBuilder> {
    /// Creates a new launcher using the provided rollup arguments.
    pub fn new(rollup_args: RollupArgs) -> Self {
        Self {
            rollup_args,
            extensions: Vec::new(),
            service_builder: DefaultPayloadServiceBuilder,
            da_config: None,
            gas_limit_config: None,
            manifest_precheck_enabled: true,
            started_callbacks: Vec::new(),
        }
    }
}

impl<SB: PayloadServiceBuilder> fmt::Debug for BaseNodeRunner<SB> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BaseNodeRunner")
            .field("rollup_args", &self.rollup_args)
            .field("extensions", &self.extensions.len())
            .field("da_config", &self.da_config)
            .field("gas_limit_config", &self.gas_limit_config)
            .field("manifest_precheck_enabled", &self.manifest_precheck_enabled)
            .field("started_callbacks", &self.started_callbacks.len())
            .finish()
    }
}

impl<SB: PayloadServiceBuilder> BaseNodeRunner<SB> {
    /// Sets the shared DA configuration.
    pub fn with_da_config(mut self, da_config: BaseDAConfig) -> Self {
        self.da_config = Some(da_config);
        self
    }

    /// Sets the shared gas-limit configuration.
    pub fn with_gas_limit_config(mut self, gas_limit_config: GasLimitConfig) -> Self {
        self.gas_limit_config = Some(gas_limit_config);
        self
    }

    /// Configures whether EIP-8130 authorization manifests are checked before execution.
    pub const fn with_manifest_precheck_enabled(mut self, enabled: bool) -> Self {
        self.manifest_precheck_enabled = enabled;
        self
    }

    /// Swap the payload service builder.
    pub fn with_service_builder<SB2: PayloadServiceBuilder>(self, sb: SB2) -> BaseNodeRunner<SB2> {
        BaseNodeRunner {
            rollup_args: self.rollup_args,
            extensions: self.extensions,
            service_builder: sb,
            da_config: self.da_config,
            gas_limit_config: self.gas_limit_config,
            manifest_precheck_enabled: self.manifest_precheck_enabled,
            started_callbacks: self.started_callbacks,
        }
    }

    /// Registers a new builder extension.
    pub fn install_ext<T: FromExtensionConfig + 'static>(&mut self, config: T::Config) {
        self.extensions.push(Box::new(T::from_config(config)));
    }

    /// Registers a callback to run after the node has started.
    pub fn add_started_callback<F>(&mut self, callback: F)
    where
        F: FnOnce() -> Result<()> + Send + 'static,
    {
        self.started_callbacks.push(Box::new(callback));
    }

    /// Applies all Base-specific wiring to the supplied builder, launches the node, and waits for
    /// shutdown.
    pub async fn run(self, builder: BaseNodeBuilder) -> Result<()> {
        let LaunchedBaseNode { handle: NodeHandle { node: _node, node_exit_future } } =
            self.launch(builder).await?;
        node_exit_future.await?;
        Ok(())
    }

    /// Applies all Base-specific wiring to the supplied builder and returns a launched node
    /// handle without waiting for shutdown.
    pub async fn launch(self, builder: BaseNodeBuilder) -> Result<LaunchedBaseNode> {
        let handle = self.launch_node(builder).await?;
        Ok(LaunchedBaseNode { handle })
    }

    async fn launch_node(self, builder: BaseNodeBuilder) -> Result<NodeHandleFor<BaseNode>> {
        info!(target: "base-runner", "starting custom Base node");

        let Self {
            rollup_args,
            extensions,
            service_builder,
            da_config,
            gas_limit_config,
            manifest_precheck_enabled,
            started_callbacks,
        } = self;
        let mut base_node = BaseNode::new(rollup_args);
        if let Some(da_config) = da_config {
            base_node = base_node.with_da_config(da_config);
        }
        if let Some(gas_limit_config) = gas_limit_config {
            base_node = base_node.with_gas_limit_config(gas_limit_config);
        }
        base_node = base_node.with_manifest_precheck_enabled(manifest_precheck_enabled);
        // Collect sidecar sub-pools by reference before the fold below consumes the boxed
        // extensions, and before `build_components` constructs the pool builder.
        let sidecar_pools =
            extensions.iter().flat_map(|ext| ext.sidecar_pools()).collect::<Vec<_>>();
        base_node = base_node.with_sidecar_pools(sidecar_pools);
        let components = service_builder.build_components(&base_node);

        let builder = builder
            .with_types_and_provider::<BaseNode, BlockchainProvider<_>>()
            .with_components(components)
            .with_add_ons(base_node.add_ons())
            .on_component_initialized(move |_ctx| Ok(()));

        let hooks = extensions.into_iter().fold(NodeHooks::new(), |hooks, ext| ext.apply(hooks));
        let hooks = started_callbacks
            .into_iter()
            .fold(hooks, |hooks, callback| hooks.add_node_started_hook(move |_| callback()));

        hooks.apply_to(builder).launch().await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use base_execution_txpool::{BasePooledTransaction, RemovalReason, SidecarInsert, SidecarPool};

    use super::*;

    /// Stands in for an out-of-tree sidecar pool defined in another crate.
    #[derive(Debug, Default)]
    struct ProbePool;

    impl SidecarPool<BasePooledTransaction> for ProbePool {
        fn name(&self) -> &'static str {
            "probe"
        }
        fn claims(&self, _transaction: &BasePooledTransaction) -> bool {
            false
        }
        fn insert_validated(
            &self,
            _origin: reth_transaction_pool::TransactionOrigin,
            _transaction: reth_transaction_pool::ValidPoolTransaction<BasePooledTransaction>,
            _state_nonce: u64,
        ) -> reth_transaction_pool::PoolResult<SidecarInsert<BasePooledTransaction>> {
            unimplemented!()
        }
        fn best_transactions(
            &self,
            _base_fee: u64,
        ) -> Box<
            dyn reth_transaction_pool::BestTransactions<
                    Item = Arc<reth_transaction_pool::ValidPoolTransaction<BasePooledTransaction>>,
                >,
        > {
            unimplemented!()
        }
        fn contains(&self, _hash: &alloy_primitives::TxHash) -> bool {
            false
        }
        fn get(
            &self,
            _hash: &alloy_primitives::TxHash,
        ) -> Option<Arc<reth_transaction_pool::ValidPoolTransaction<BasePooledTransaction>>>
        {
            None
        }
        fn all_transactions(
            &self,
        ) -> Vec<Arc<reth_transaction_pool::ValidPoolTransaction<BasePooledTransaction>>> {
            Vec::new()
        }
        fn pending_transactions(
            &self,
        ) -> Vec<Arc<reth_transaction_pool::ValidPoolTransaction<BasePooledTransaction>>> {
            Vec::new()
        }
        fn queued_transactions(
            &self,
        ) -> Vec<Arc<reth_transaction_pool::ValidPoolTransaction<BasePooledTransaction>>> {
            Vec::new()
        }
        fn pending_and_queued_txn_count(&self) -> (usize, usize) {
            (0, 0)
        }
        fn all_hashes(&self) -> Vec<alloy_primitives::TxHash> {
            Vec::new()
        }
        fn remove_transactions(
            &self,
            _hashes: &[alloy_primitives::TxHash],
            _reason: RemovalReason,
        ) -> Vec<Arc<reth_transaction_pool::ValidPoolTransaction<BasePooledTransaction>>> {
            Vec::new()
        }
        fn remove_transactions_by_sender(
            &self,
            _sender: alloy_primitives::Address,
        ) -> Vec<Arc<reth_transaction_pool::ValidPoolTransaction<BasePooledTransaction>>> {
            Vec::new()
        }
    }

    #[derive(Debug)]
    struct ProbeExtension {
        pool: Arc<ProbePool>,
    }

    impl BaseNodeExtension for ProbeExtension {
        fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
            hooks
        }

        fn sidecar_pools(&self) -> Vec<Arc<dyn SidecarPool<BasePooledTransaction>>> {
            vec![Arc::clone(&self.pool) as Arc<dyn SidecarPool<BasePooledTransaction>>]
        }
    }

    #[test]
    fn extensions_contribute_sidecar_pools_before_boxes_are_consumed() {
        let extensions: Vec<Box<dyn BaseNodeExtension>> =
            vec![Box::new(ProbeExtension { pool: Arc::new(ProbePool) })];

        // exactly the lines added to `launch_node`
        let sidecar_pools =
            extensions.iter().flat_map(|ext| ext.sidecar_pools()).collect::<Vec<_>>();
        let base_node =
            BaseNode::new(RollupArgs::default()).with_sidecar_pools(sidecar_pools.clone());

        assert_eq!(sidecar_pools.len(), 1);
        assert_eq!(base_node.sidecar_pools.len(), 1);
        assert_eq!(base_node.sidecar_pools[0].name(), "probe");

        // the fold still consumes the same `extensions` afterwards
        let _hooks = extensions.into_iter().fold(NodeHooks::new(), |hooks, ext| ext.apply(hooks));
    }

    #[derive(Debug)]
    struct TestPayloadServiceBuilder;

    impl crate::service::PayloadServiceBuilder for TestPayloadServiceBuilder {
        type ComponentsBuilder = crate::types::BaseComponentsBuilder;

        fn build_components(self, base_node: &BaseNode) -> Self::ComponentsBuilder {
            base_node.components()
        }
    }

    #[test]
    fn service_builder_swap_preserves_shared_runtime_configs() {
        let da_config = BaseDAConfig::new(100, 200);
        let gas_limit_config = GasLimitConfig::new(30_000_000);

        let runner = BaseNodeRunner::new(RollupArgs::default())
            .with_da_config(da_config.clone())
            .with_gas_limit_config(gas_limit_config.clone())
            .with_manifest_precheck_enabled(false)
            .with_service_builder(TestPayloadServiceBuilder);

        assert!(!runner.manifest_precheck_enabled);
        let configured_da = runner.da_config.expect("DA config should be preserved");
        let configured_gas = runner.gas_limit_config.expect("gas-limit config should be preserved");

        assert_eq!(configured_da.max_da_tx_size(), Some(100));
        assert_eq!(configured_da.max_da_block_size(), Some(200));
        assert_eq!(configured_gas.gas_limit(), Some(30_000_000));

        da_config.set_max_da_size(300, 400);
        gas_limit_config.set_gas_limit(40_000_000);

        assert_eq!(configured_da.max_da_tx_size(), Some(300));
        assert_eq!(configured_da.max_da_block_size(), Some(400));
        assert_eq!(configured_gas.gas_limit(), Some(40_000_000));
    }
}
