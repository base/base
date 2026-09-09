//! Utilities for running e2e tests against a node or a network of nodes.

use std::{collections::HashMap, fmt::Debug};

use alloy_primitives::B256;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelopeV3;
use base_execution_payload_builder::PayloadId;
use base_execution_payload_types::BasePayloadBuilderAttributes;
use base_node_core::{ComponentBuilder, RethRpcAddOns};
use eyre::Result;
use jsonrpsee::http_client::HttpClient;

use crate::testsuite::actions::{Action, ActionBox};
pub mod actions;
pub mod setup;
use std::sync::Arc;

use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_engine::{ForkchoiceState, PayloadAttributes};
use base_execution_payload_builder::BaseExecutionHandle;
use reth_engine_primitives::ConsensusEngineHandle;
use url::Url;

use crate::testsuite::setup::Setup;

/// Client handles for both regular RPC and Engine API endpoints
#[derive(Clone)]
pub struct NodeClient {
    /// Regular JSON-RPC client
    pub rpc: HttpClient,
    /// Engine API client
    pub engine: BaseExecutionHandle,
    /// Beacon consensus engine handle for direct interaction with the consensus engine
    pub beacon_engine_handle: Option<ConsensusEngineHandle>,
    /// Alloy provider for interacting with the node
    provider: Arc<dyn Provider + Send + Sync>,
}

impl NodeClient {
    /// Instantiates a new [`NodeClient`] with the given handles and RPC URL
    pub fn new(rpc: HttpClient, engine: BaseExecutionHandle, url: Url) -> Self {
        let provider =
            Arc::new(ProviderBuilder::new().connect_http(url)) as Arc<dyn Provider + Send + Sync>;
        Self { rpc, engine, beacon_engine_handle: None, provider }
    }

    /// Instantiates a new [`NodeClient`] with the given handles, RPC URL, and beacon engine handle
    pub fn new_with_beacon_engine(
        rpc: HttpClient,
        engine: BaseExecutionHandle,
        url: Url,
        beacon_engine_handle: ConsensusEngineHandle,
    ) -> Self {
        let provider =
            Arc::new(ProviderBuilder::new().connect_http(url)) as Arc<dyn Provider + Send + Sync>;
        Self { rpc, engine, beacon_engine_handle: Some(beacon_engine_handle), provider }
    }

    /// Get a block by number using the alloy provider
    pub async fn get_block_by_number(
        &self,
        number: alloy_eips::BlockNumberOrTag,
    ) -> Result<Option<alloy_rpc_types_eth::Block>> {
        self.provider
            .get_block_by_number(number)
            .await
            .map_err(|e| eyre::eyre!("Failed to get block by number: {}", e))
    }

    /// Check if the node is ready by attempting to get the latest block
    pub async fn is_ready(&self) -> bool {
        self.get_block_by_number(alloy_eips::BlockNumberOrTag::Latest).await.is_ok()
    }
}

impl Debug for NodeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeClient")
            .field("rpc", &self.rpc)
            .field("engine", &self.engine)
            .field("beacon_engine_handle", &self.beacon_engine_handle.is_some())
            .field("provider", &"<Provider>")
            .finish()
    }
}

/// Represents complete block information.
#[derive(Debug, Clone, Copy)]
pub struct BlockInfo {
    /// Hash of the block
    pub hash: B256,
    /// Number of the block
    pub number: u64,
    /// Timestamp of the block
    pub timestamp: u64,
}

/// Per-node state tracking for multi-node environments
#[derive(Clone)]
pub struct NodeState {
    /// Current block information for this node
    pub current_block_info: Option<BlockInfo>,
    /// Stores payload attributes indexed by block number for this node
    pub payload_attributes: HashMap<u64, PayloadAttributes>,
    /// Tracks the latest block header timestamp for this node
    pub latest_header_time: u64,
    /// Stores payload IDs returned by this node, indexed by block number
    pub payload_id_history: HashMap<u64, PayloadId>,
    /// Stores the next expected payload ID for this node
    pub next_payload_id: Option<PayloadId>,
    /// Stores the latest fork choice state for this node
    pub latest_fork_choice_state: ForkchoiceState,
    /// Stores the most recent built execution payload for this node
    pub latest_payload_built: Option<PayloadAttributes>,
    /// Stores the most recent executed payload for this node
    pub latest_payload_executed: Option<PayloadAttributes>,
    /// Stores the most recent built execution payload envelope for this node
    pub latest_payload_envelope: Option<BaseExecutionPayloadEnvelopeV3>,
    /// Fork base block number for validation (if this node is currently on a fork)
    pub current_fork_base: Option<u64>,
}

impl Default for NodeState {
    fn default() -> Self {
        Self {
            current_block_info: None,
            payload_attributes: HashMap::new(),
            latest_header_time: 0,
            payload_id_history: HashMap::new(),
            next_payload_id: None,
            latest_fork_choice_state: ForkchoiceState::default(),
            latest_payload_built: None,
            latest_payload_executed: None,
            latest_payload_envelope: None,
            current_fork_base: None,
        }
    }
}

impl Debug for NodeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeState")
            .field("current_block_info", &self.current_block_info)
            .field("payload_attributes", &self.payload_attributes)
            .field("latest_header_time", &self.latest_header_time)
            .field("payload_id_history", &self.payload_id_history)
            .field("next_payload_id", &self.next_payload_id)
            .field("latest_fork_choice_state", &self.latest_fork_choice_state)
            .field("latest_payload_built", &self.latest_payload_built)
            .field("latest_payload_executed", &self.latest_payload_executed)
            .field("latest_payload_envelope", &"<ExecutionPayloadEnvelopeV3>")
            .field("current_fork_base", &self.current_fork_base)
            .finish()
    }
}

/// Represents a test environment.
#[derive(Debug)]
pub struct Environment {
    /// Combined clients with both RPC and Engine API endpoints
    pub node_clients: Vec<NodeClient>,
    /// Per-node state tracking
    pub node_states: Vec<NodeState>,
    /// Last producer index
    pub last_producer_idx: Option<usize>,
    /// Defines the increment for block timestamps (default: 2 seconds)
    pub block_timestamp_increment: u64,
    /// Number of slots until a block is considered safe
    pub slots_to_safe: u64,
    /// Number of slots until a block is considered finalized
    pub slots_to_finalized: u64,
    /// Registry for tagged blocks, mapping tag names to block info and node index
    pub block_registry: HashMap<String, (BlockInfo, usize)>,
    /// Currently active node index for backward compatibility with single-node actions
    pub active_node_idx: usize,
    /// Converts shared payload attributes to the node's configured attributes.
    pub payload_attributes_converter: Option<fn(PayloadAttributes) -> BasePayloadBuilderAttributes>,
}

impl Default for Environment {
    fn default() -> Self {
        Self {
            payload_attributes_converter: None,
            node_clients: vec![],
            node_states: vec![],
            last_producer_idx: None,
            block_timestamp_increment: 2,
            slots_to_safe: 0,
            slots_to_finalized: 0,
            block_registry: HashMap::new(),
            active_node_idx: 0,
        }
    }
}

impl Environment {
    /// Get the number of nodes in the environment
    pub const fn node_count(&self) -> usize {
        self.node_clients.len()
    }

    /// Get mutable reference to a specific node's state
    pub fn node_state_mut(&mut self, node_idx: usize) -> Result<&mut NodeState, eyre::Error> {
        let node_count = self.node_count();
        self.node_states.get_mut(node_idx).ok_or_else(|| {
            eyre::eyre!("Node index {} out of bounds (have {} nodes)", node_idx, node_count)
        })
    }

    /// Get immutable reference to a specific node's state
    pub fn node_state(&self, node_idx: usize) -> Result<&NodeState, eyre::Error> {
        self.node_states.get(node_idx).ok_or_else(|| {
            eyre::eyre!("Node index {} out of bounds (have {} nodes)", node_idx, self.node_count())
        })
    }

    /// Get the currently active node's state
    pub fn active_node_state(&self) -> Result<&NodeState, eyre::Error> {
        self.node_state(self.active_node_idx)
    }

    /// Get mutable reference to the currently active node's state
    pub fn active_node_state_mut(&mut self) -> Result<&mut NodeState, eyre::Error> {
        let idx = self.active_node_idx;
        self.node_state_mut(idx)
    }

    /// Set the active node index
    pub fn set_active_node(&mut self, node_idx: usize) -> Result<(), eyre::Error> {
        if node_idx >= self.node_count() {
            return Err(eyre::eyre!(
                "Node index {} out of bounds (have {} nodes)",
                node_idx,
                self.node_count()
            ));
        }
        self.active_node_idx = node_idx;
        Ok(())
    }

    /// Initialize node states when nodes are created
    pub fn initialize_node_states(&mut self, node_count: usize) {
        self.node_states = (0..node_count).map(|_| NodeState::default()).collect();
    }

    /// Get current block info from active node
    pub fn current_block_info(&self) -> Option<BlockInfo> {
        self.active_node_state().ok()?.current_block_info
    }

    /// Set current block info on active node
    pub fn set_current_block_info(&mut self, block_info: BlockInfo) -> Result<(), eyre::Error> {
        self.active_node_state_mut()?.current_block_info = Some(block_info);
        Ok(())
    }
}

/// Builder for creating test scenarios
#[expect(missing_debug_implementations)]
pub struct TestBuilder {
    setup: Option<Setup>,
    actions: Vec<ActionBox>,
    env: Environment,
}

impl Default for TestBuilder {
    fn default() -> Self {
        Self { setup: None, actions: Vec::new(), env: Default::default() }
    }
}

impl TestBuilder {
    /// Create a new test builder
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the test setup
    pub fn with_setup(mut self, setup: Setup) -> Self {
        self.setup = Some(setup);
        self
    }

    /// Add an action to the test
    pub fn with_action<A>(mut self, action: A) -> Self
    where
        A: Action,
    {
        self.actions.push(ActionBox::new(action));
        self
    }

    /// Add multiple actions to the test
    pub fn with_actions<II, A>(mut self, actions: II) -> Self
    where
        II: IntoIterator<Item = A>,
        A: Action,
    {
        self.actions.extend(actions.into_iter().map(ActionBox::new));
        self
    }

    /// Run the test scenario
    pub async fn run<AO>(
        mut self,
        node_factory: impl Fn() -> (ComponentBuilder<crate::TmpNodeAdapter>, AO) + Send + Sync,
    ) -> Result<()>
    where
        AO: RethRpcAddOns<crate::TmpDB> + 'static,
    {
        let mut setup = self.setup.take();

        if let Some(ref mut s) = setup {
            s.apply(&mut self.env, node_factory).await?;
        }

        let actions = std::mem::take(&mut self.actions);

        for action in actions {
            action.execute(&mut self.env).await?;
        }

        // explicitly drop the setup to shutdown the nodes
        // after all actions have completed
        drop(setup);

        Ok(())
    }
}
