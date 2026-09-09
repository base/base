use std::fmt;

use crate::full_node::FullNode;

/// Container for all the configurable hook functions.
pub struct NodeHooks {
    /// Hook to run once core components are initialized.
    pub on_component_initialized:
        Box<dyn FnOnce(base_node_context::BaseNodeContext) -> eyre::Result<()> + Send>,
    /// Hook to run once the node is started.
    pub on_node_started: Box<dyn FnOnce(FullNode) -> eyre::Result<()> + Send>,
}

impl NodeHooks {
    /// Creates a new, empty [`NodeHooks`] instance for the given node type.
    pub fn new() -> Self {
        Self {
            on_component_initialized: Box::new(|_| Ok(())),
            on_node_started: Box::new(|_| Ok(())),
        }
    }

    /// Sets the hook that is run once the node's components are initialized.
    pub fn set_on_component_initialized<F>(&mut self, hook: F) -> &mut Self
    where
        F: FnOnce(base_node_context::BaseNodeContext) -> eyre::Result<()> + Send + 'static,
    {
        self.on_component_initialized = Box::new(hook);
        self
    }

    /// Sets the hook that is run once the node has started.
    pub fn set_on_node_started<F>(&mut self, hook: F) -> &mut Self
    where
        F: FnOnce(FullNode) -> eyre::Result<()> + Send + 'static,
    {
        self.on_node_started = Box::new(hook);
        self
    }
}

impl Default for NodeHooks {
    fn default() -> Self {
        Self::new()
    }
}
impl fmt::Debug for NodeHooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeHooks")
            .field("on_component_initialized", &"...")
            .field("on_node_started", &"...")
            .finish()
    }
}
