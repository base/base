//! Core [`RollupNode`] service, composing the available [`NodeActor`]s into various modes of
//! operation.
//!
//! [`NodeActor`]: kona_node_service::NodeActor

mod builder;
pub use builder::RollupNodeBuilder;

mod node;
pub use node::RollupNode;
