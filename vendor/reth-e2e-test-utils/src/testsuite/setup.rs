//! Test setup utilities for configuring the initial state.

use std::sync::Arc;

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceState, PayloadAttributes};
use base_common_consensus::BaseTxEnvelope;
use base_execution_chainspec::BaseChainSpec;
use eyre::{Result, eyre};
use reth_chainspec::ChainSpec;
use reth_ethereum_primitives::Block;
use reth_node_api::TreeConfig;
use reth_node_core::primitives::RecoveredBlock;
use reth_payload_primitives::BasePayloadBuilderAttributes;
use revm::state::EvmState;
use tokio::{
    sync::mpsc,
    time::{Duration, sleep},
};
use tracing::debug;

use crate::{E2ETestSetupBuilder, testsuite::Environment};

/// Configuration for setting up test environment
#[derive(Debug)]
pub struct Setup {
    /// Chain specification to use
    pub chain_spec: Option<Arc<ChainSpec>>,
    /// Genesis block to use
    pub genesis: Option<Genesis>,
    /// Blocks to replay during setup
    pub blocks: Vec<RecoveredBlock<Block>>,
    /// Initial state to load
    pub state: Option<EvmState>,
    /// Network configuration
    pub network: NetworkSetup,
    /// Engine tree configuration
    pub tree_config: TreeConfig,
    /// Shutdown channel to stop nodes when setup is dropped
    shutdown_tx: Option<mpsc::Sender<()>>,
    /// Is this setup in dev mode
    pub is_dev: bool,
    /// Conversion for chain-specific payload attributes.
    pub payload_attributes_converter:
        Option<fn(PayloadAttributes) -> BasePayloadBuilderAttributes<BaseTxEnvelope>>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            chain_spec: None,
            genesis: None,
            blocks: Vec::new(),
            state: None,
            network: NetworkSetup::default(),
            tree_config: TreeConfig::default(),
            shutdown_tx: None,
            is_dev: true,

            payload_attributes_converter: None,
        }
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        // Send shutdown signal if the channel exists
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.try_send(());
        }
    }
}

impl Setup {
    /// Supplies chain-specific fields when the framework creates payload attributes.
    pub fn with_payload_attributes_converter(
        mut self,
        converter: fn(PayloadAttributes) -> BasePayloadBuilderAttributes<BaseTxEnvelope>,
    ) -> Self {
        self.payload_attributes_converter = Some(converter);
        self
    }

    /// Set the chain specification
    pub fn with_chain_spec(mut self, chain_spec: Arc<ChainSpec>) -> Self {
        self.chain_spec = Some(chain_spec);
        self
    }

    /// Set the genesis block
    pub const fn with_genesis(mut self, genesis: Genesis) -> Self {
        self.genesis = Some(genesis);
        self
    }

    /// Add a block to replay during setup
    pub fn with_block(mut self, block: RecoveredBlock<Block>) -> Self {
        self.blocks.push(block);
        self
    }

    /// Add multiple blocks to replay during setup
    pub fn with_blocks(mut self, blocks: Vec<RecoveredBlock<Block>>) -> Self {
        self.blocks.extend(blocks);
        self
    }

    /// Set the initial state
    pub fn with_state(mut self, state: EvmState) -> Self {
        self.state = Some(state);
        self
    }

    /// Set the network configuration
    pub const fn with_network(mut self, network: NetworkSetup) -> Self {
        self.network = network;
        self
    }

    /// Set dev mode
    pub const fn with_dev_mode(mut self, is_dev: bool) -> Self {
        self.is_dev = is_dev;
        self
    }

    /// Set the engine tree configuration
    pub const fn with_tree_config(mut self, tree_config: TreeConfig) -> Self {
        self.tree_config = tree_config;
        self
    }

    /// Apply the setup to the environment
    pub async fn apply<N>(&mut self, env: &mut Environment) -> Result<()>
    where
        N: Default
            + reth_node_builder::Node<
                crate::TmpNodeAdapter,
                Network: reth_network_api::test_utils::PeersHandleProvider,
                AddOns: reth_node_builder::rpc::RethRpcAddOns<crate::Adapter<N>>
                            + reth_node_builder::rpc::EngineValidatorAddOn<crate::Adapter<N>>,
            >,
    {
        // Note: this future is quite large so we box it
        Box::pin(self.apply_::<N>(env)).await
    }

    /// Apply the setup to the environment
    async fn apply_<N>(&mut self, env: &mut Environment) -> Result<()>
    where
        N: Default
            + reth_node_builder::Node<
                crate::TmpNodeAdapter,
                Network: reth_network_api::test_utils::PeersHandleProvider,
                AddOns: reth_node_builder::rpc::RethRpcAddOns<crate::Adapter<N>>
                            + reth_node_builder::rpc::EngineValidatorAddOn<crate::Adapter<N>>,
            >,
    {
        let chain_spec =
            self.chain_spec.clone().ok_or_else(|| eyre!("Chain specification is required"))?;

        let (shutdown_tx, mut shutdown_rx) = mpsc::channel(1);
        self.shutdown_tx = Some(shutdown_tx);

        let is_dev = self.is_dev;
        let node_count = self.network.node_count;
        let tree_config = self.tree_config.clone();

        let converter = self.payload_attributes_converter;
        env.payload_attributes_converter = converter;
        let attributes_generator = move |timestamp| {
            let attributes = Self::static_attributes(timestamp);
            converter
                .map_or_else(|| attributes.clone().into(), |convert| convert(attributes.clone()))
        };

        let builder = E2ETestSetupBuilder::new(
            node_count,
            Arc::<BaseChainSpec>::new((*chain_spec).clone().into()),
            attributes_generator,
        )
        .with_tree_config_modifier(move |base| {
            tree_config.clone().with_cross_block_cache_size(base.cross_block_cache_size())
        })
        .with_node_config_modifier(move |config| config.set_dev(is_dev))
        .with_connect_nodes(self.network.connect_nodes);

        let result = builder.build::<N>().await;

        let mut node_clients = Vec::new();
        match result {
            Ok((nodes, _wallet)) => {
                // create HTTP clients for each node's RPC and Engine API endpoints
                for node in &nodes {
                    node_clients.push(node.to_node_client()?);
                }

                // spawn a separate task just to handle the shutdown
                tokio::spawn(async move {
                    // keep nodes in scope to ensure they're not dropped
                    let _nodes = nodes;
                    // Wait for shutdown signal
                    let _ = shutdown_rx.recv().await;
                    // nodes will be dropped here when the test completes
                });
            }
            Err(e) => {
                return Err(eyre!("Failed to setup nodes: {}", e));
            }
        }

        // Finalize setup
        self.finalize_setup(env, node_clients, false).await
    }

    /// Creates the shared portion of payload attributes.
    pub fn static_attributes(timestamp: u64) -> PayloadAttributes {
        PayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: alloy_primitives::Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
            slot_number: None,
            target_gas_limit: None,
        }
    }

    /// Common finalization logic for both apply methods
    async fn finalize_setup(
        &self,
        env: &mut Environment,
        node_clients: Vec<crate::testsuite::NodeClient>,
        use_latest_block: bool,
    ) -> Result<()> {
        if node_clients.is_empty() {
            return Err(eyre!("No nodes were created"));
        }

        // Wait for all nodes to be ready
        self.wait_for_nodes_ready(&node_clients).await?;

        env.node_clients = node_clients;
        env.initialize_node_states(self.network.node_count);

        // Get initial block info (genesis or latest depending on use_latest_block)
        let (initial_block_info, genesis_block_info) = if use_latest_block {
            // For imported chain, get both latest and genesis
            let latest =
                self.get_block_info(&env.node_clients[0], BlockNumberOrTag::Latest).await?;
            let genesis =
                self.get_block_info(&env.node_clients[0], BlockNumberOrTag::Number(0)).await?;
            (latest, genesis)
        } else {
            // For fresh chain, both are genesis
            let genesis =
                self.get_block_info(&env.node_clients[0], BlockNumberOrTag::Number(0)).await?;
            (genesis, genesis)
        };

        // Initialize all node states
        for (node_idx, node_state) in env.node_states.iter_mut().enumerate() {
            node_state.current_block_info = Some(initial_block_info);
            node_state.latest_header_time = initial_block_info.timestamp;
            node_state.latest_fork_choice_state = ForkchoiceState {
                head_block_hash: initial_block_info.hash,
                safe_block_hash: initial_block_info.hash,
                finalized_block_hash: genesis_block_info.hash,
            };

            debug!(
                "Node {} initialized with block {} (hash: {})",
                node_idx, initial_block_info.number, initial_block_info.hash
            );
        }

        debug!(
            "Environment initialized with {} nodes, starting from block {} (hash: {})",
            self.network.node_count, initial_block_info.number, initial_block_info.hash
        );

        Ok(())
    }

    /// Wait for all nodes to be ready to accept RPC requests
    async fn wait_for_nodes_ready(
        &self,
        node_clients: &[crate::testsuite::NodeClient],
    ) -> Result<()> {
        for (idx, client) in node_clients.iter().enumerate() {
            let mut retry_count = 0;
            const MAX_RETRIES: usize = 10;

            while retry_count < MAX_RETRIES {
                if client.is_ready().await {
                    debug!("Node {idx} RPC endpoint is ready");
                    break;
                }

                retry_count += 1;
                debug!("Node {idx} RPC endpoint not ready, retry {retry_count}/{MAX_RETRIES}");
                sleep(Duration::from_millis(500)).await;
            }

            if retry_count == MAX_RETRIES {
                return Err(eyre!(
                    "Failed to connect to node {idx} RPC endpoint after {MAX_RETRIES} retries"
                ));
            }
        }
        Ok(())
    }

    /// Get block info for a given block number or tag
    async fn get_block_info(
        &self,
        client: &crate::testsuite::NodeClient,
        block: BlockNumberOrTag,
    ) -> Result<crate::testsuite::BlockInfo> {
        let block = client
            .get_block_by_number(block)
            .await?
            .ok_or_else(|| eyre!("Block {:?} not found", block))?;

        Ok(crate::testsuite::BlockInfo {
            hash: block.header.hash,
            number: block.header.number,
            timestamp: block.header.timestamp,
        })
    }
}

/// Genesis block configuration
#[derive(Debug)]
pub struct Genesis {}

/// Network configuration for setup
#[derive(Debug, Default)]
pub struct NetworkSetup {
    /// Number of nodes to create
    pub node_count: usize,
    /// Whether nodes should be connected to each other
    pub connect_nodes: bool,
}

impl NetworkSetup {
    /// Create a new network setup with a single node
    pub const fn single_node() -> Self {
        Self { node_count: 1, connect_nodes: true }
    }

    /// Create a new network setup with multiple nodes (connected)
    pub const fn multi_node(count: usize) -> Self {
        Self { node_count: count, connect_nodes: true }
    }

    /// Create a new network setup with multiple nodes (disconnected)
    pub const fn multi_node_unconnected(count: usize) -> Self {
        Self { node_count: count, connect_nodes: false }
    }
}
