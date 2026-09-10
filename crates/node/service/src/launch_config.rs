//! Concrete inputs for launching a Base execution node.

use base_common_runtime::TaskExecutor;
use base_common_types_payload::TreeConfig;
use base_execution_state_database::DatabaseEnv;

use crate::{BaseNode, BasePayloadServiceConfig, BaseRpcServices, NodeConfig, NodeServices};

/// Operational configuration and resources for one Base node.
#[derive(Debug)]
pub struct NodeLaunch {
    /// Execution, storage, networking, and transport settings.
    pub config: NodeConfig,
    /// Open MDBX environment.
    pub database: DatabaseEnv,
    /// Runtime shared with the binary's consensus services.
    pub task_executor: TaskExecutor,
    /// Engine execution limits, derived once from the node settings.
    pub engine_tree_config: TreeConfig,
    /// Base pool, network, and payload settings.
    pub base: BaseNode,
    /// Full-block service settings supplied by the sequencer.
    pub payload: Option<BasePayloadServiceConfig>,
    /// Runtime inputs for the built-in RPC handlers.
    pub rpc: BaseRpcServices,
    /// Runtime settings for the built-in background services.
    pub services: NodeServices,
}

impl NodeLaunch {
    /// Combines the binary's resolved configuration, database, and runtime.
    pub fn new(
        config: NodeConfig,
        database: impl Into<DatabaseEnv>,
        task_executor: TaskExecutor,
    ) -> Self {
        let engine_tree_config = config.tree_config();
        Self {
            config,
            database: database.into(),
            task_executor,
            engine_tree_config,
            base: BaseNode::default(),
            payload: None,
            rpc: BaseRpcServices::default(),
            services: NodeServices::default(),
        }
    }

    /// Opens an ephemeral node database owned by the launched test node.
    #[cfg(feature = "test-utils")]
    pub fn testing(mut config: NodeConfig, task_executor: TaskExecutor) -> Self {
        let path = base_execution_state_database::test_utils::tempdir_path();
        config.datadir.datadir = path.into();
        let db = base_execution_state_database::test_utils::create_test_rw_db_with_datadir(
            config.datadir().data_dir(),
        );
        Self::new(config, db, task_executor)
    }
}
