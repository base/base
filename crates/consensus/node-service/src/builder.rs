//! Builder for the unified rollup node.

use std::sync::Arc;

use base_engine_actor::DirectEngineApi;
use kona_genesis::RollupConfig;

use crate::UnifiedRollupNode;

/// Builder for [`UnifiedRollupNode`].
#[derive(Debug)]
pub struct UnifiedRollupNodeBuilder<E: DirectEngineApi> {
    config: Option<Arc<RollupConfig>>,
    engine_driver: Option<Arc<E>>,
    channel_buffer_size: usize,
}

impl<E: DirectEngineApi + std::fmt::Debug + 'static> Default for UnifiedRollupNodeBuilder<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: DirectEngineApi + std::fmt::Debug + 'static> UnifiedRollupNodeBuilder<E> {
    /// Creates a new builder with default values.
    pub const fn new() -> Self {
        Self { config: None, engine_driver: None, channel_buffer_size: 256 }
    }

    /// Sets the rollup configuration.
    pub fn config(mut self, config: Arc<RollupConfig>) -> Self {
        self.config = Some(config);
        self
    }

    /// Sets the engine driver.
    pub fn engine_driver(mut self, engine_driver: Arc<E>) -> Self {
        self.engine_driver = Some(engine_driver);
        self
    }

    /// Sets the channel buffer size for inter-actor communication.
    pub const fn channel_buffer_size(mut self, size: usize) -> Self {
        self.channel_buffer_size = size;
        self
    }

    /// Builds the [`UnifiedRollupNode`].
    ///
    /// # Panics
    ///
    /// Panics if `config` or `engine_driver` is not set.
    pub fn build(self) -> UnifiedRollupNode<E> {
        let config = self.config.expect("config is required");
        let engine_driver = self.engine_driver.expect("engine_driver is required");

        UnifiedRollupNode::new_with_buffer_size(config, engine_driver, self.channel_buffer_size)
    }

    /// Attempts to build the [`UnifiedRollupNode`], returning `None` if any
    /// required field is missing.
    pub fn try_build(self) -> Option<UnifiedRollupNode<E>> {
        Some(UnifiedRollupNode::new_with_buffer_size(
            self.config?,
            self.engine_driver?,
            self.channel_buffer_size,
        ))
    }
}
