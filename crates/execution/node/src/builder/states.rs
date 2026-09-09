//! Node builder states and helper traits.
//!
//! Keeps track of the current state of the node builder.
//!
//! The node builder process is essentially a state machine that transitions through various states
//! before the node can be launched.

use reth_node_core::node_config::NodeConfig;
use reth_provider::providers::RocksDBProvider;

use crate::{launch::LaunchNode, launch_components::ComponentBuilder};

/// A fully type configured node builder.
///
/// Supports adding additional addons to the node.
pub struct NodeBuilderWithComponents {
    /// All settings for how the node should be configured.
    pub config: NodeConfig,
    /// Adapter for the underlying node types and database
    pub database: reth_db::DatabaseEnv,
    /// An optional [`RocksDBProvider`] to use instead of creating one during launch.
    pub rocksdb_provider: Option<RocksDBProvider>,
    /// container for type specific components
    pub components_builder: ComponentBuilder,
    /// Additional node extensions.
    pub add_ons: crate::BaseAddOns,
    /// Hooks invoked as the node starts.
    pub services: crate::NodeServices,
}

impl NodeBuilderWithComponents {
    /// Advances the state of the node builder to the next state where all customizable
    /// Base RPC services are configured.
    pub fn with_add_ons(self, add_ons: crate::BaseAddOns) -> NodeBuilderWithComponents {
        let Self { config, database, rocksdb_provider, components_builder, services, .. } = self;

        NodeBuilderWithComponents {
            config,
            database,
            rocksdb_provider,
            components_builder,
            add_ons,
            services,
        }
    }
}

impl NodeBuilderWithComponents {
    /// Launches the node with the given closure.
    pub fn launch_with_fn<L, R>(self, launcher: L) -> R
    where
        L: FnOnce(Self) -> R,
    {
        launcher(self)
    }

    /// Check that the builder can be launched
    ///
    /// This is useful when writing tests to ensure that the builder is configured correctly.
    pub const fn check_launch(self) -> Self {
        self
    }
}

impl NodeBuilderWithComponents {
    /// Launches the node with the given launcher.
    pub fn launch_with<L>(self, launcher: L) -> L::Future
    where
        L: LaunchNode<Self>,
    {
        launcher.launch_node(self)
    }
}
