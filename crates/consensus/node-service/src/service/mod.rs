//! Core [`BaseNode`] service, composing kona's [`NodeActor`]s into various modes of operation.
//!
//! [`NodeActor`]: kona_node_service::NodeActor

pub(crate) mod builder;
pub(crate) mod node;

pub use builder::{BaseNodeBuilder, L1ConfigBuilder};
pub use node::{BaseNode, L1Config};

pub(crate) mod util;
pub(crate) use util::{shutdown_signal, spawn_and_wait};
