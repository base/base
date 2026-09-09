use std::fmt;

use base_node_context::NodeAddOns;

use crate::full_node::FullNode;

/// Container for all the configurable hook functions.
pub struct NodeHooks<AddOns: NodeAddOns> {
    /// Hook to run once core components are initialized.
    pub on_component_initialized:
        Box<dyn OnComponentInitializedHook<base_node_context::BaseNodeContext>>,
    /// Hook to run once the node is started.
    pub on_node_started: Box<dyn OnNodeStartedHook<AddOns>>,
}

impl<AddOns> NodeHooks<AddOns>
where
    AddOns: NodeAddOns,
{
    /// Creates a new, empty [`NodeHooks`] instance for the given node type.
    pub fn new() -> Self {
        Self {
            on_component_initialized: Box::<()>::default(),
            on_node_started: Box::<()>::default(),
        }
    }

    /// Sets the hook that is run once the node's components are initialized.
    pub(crate) fn set_on_component_initialized<F>(&mut self, hook: F) -> &mut Self
    where
        F: OnComponentInitializedHook<base_node_context::BaseNodeContext> + 'static,
    {
        self.on_component_initialized = Box::new(hook);
        self
    }

    /// Sets the hook that is run once the node's components are initialized.
    #[expect(unused)]
    pub(crate) fn on_component_initialized<F>(mut self, hook: F) -> Self
    where
        F: OnComponentInitializedHook<base_node_context::BaseNodeContext> + 'static,
    {
        self.set_on_component_initialized(hook);
        self
    }

    /// Sets the hook that is run once the node has started.
    pub(crate) fn set_on_node_started<F>(&mut self, hook: F) -> &mut Self
    where
        F: OnNodeStartedHook<AddOns> + 'static,
    {
        self.on_node_started = Box::new(hook);
        self
    }

    /// Sets the hook that is run once the node has started.
    #[expect(unused)]
    pub(crate) fn on_node_started<F>(mut self, hook: F) -> Self
    where
        F: OnNodeStartedHook<AddOns> + 'static,
    {
        self.set_on_node_started(hook);
        self
    }
}

impl<AddOns> Default for NodeHooks<AddOns>
where
    AddOns: NodeAddOns,
{
    fn default() -> Self {
        Self::new()
    }
}
impl<AddOns> fmt::Debug for NodeHooks<AddOns>
where
    AddOns: NodeAddOns,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeHooks")
            .field("on_component_initialized", &"...")
            .field("on_node_started", &"...")
            .finish()
    }
}

/// A helper trait for the event hook that is run once the node is initialized.
pub trait OnComponentInitializedHook<Node>: Send {
    /// Consumes the event hook and runs it.
    ///
    /// If this returns an error, the node launch will be aborted.
    fn on_event(self: Box<Self>, node: Node) -> eyre::Result<()>;
}

impl<Node, F> OnComponentInitializedHook<Node> for F
where
    F: FnOnce(Node) -> eyre::Result<()> + Send,
{
    fn on_event(self: Box<Self>, node: Node) -> eyre::Result<()> {
        (*self)(node)
    }
}

/// A helper trait that is run once the node is started.
pub trait OnNodeStartedHook<AddOns: NodeAddOns>: Send {
    /// Consumes the event hook and runs it.
    ///
    /// If this returns an error, the node launch will be aborted.
    fn on_event(self: Box<Self>, node: FullNode<AddOns>) -> eyre::Result<()>;
}

impl<AddOns, F> OnNodeStartedHook<AddOns> for F
where
    AddOns: NodeAddOns,
    F: FnOnce(FullNode<AddOns>) -> eyre::Result<()> + Send,
{
    fn on_event(self: Box<Self>, node: FullNode<AddOns>) -> eyre::Result<()> {
        (*self)(node)
    }
}

impl<Node> OnComponentInitializedHook<Node> for () {
    fn on_event(self: Box<Self>, _node: Node) -> eyre::Result<()> {
        Ok(())
    }
}

impl<AddOns> OnNodeStartedHook<AddOns> for ()
where
    AddOns: NodeAddOns,
{
    fn on_event(self: Box<Self>, _node: FullNode<AddOns>) -> eyre::Result<()> {
        Ok(())
    }
}
