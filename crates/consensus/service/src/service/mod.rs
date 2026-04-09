//! Core [`RollupNode`] service, composing the available [`NodeActor`]s into various modes of
//! operation.
//!
//! [`NodeActor`]: crate::NodeActor

mod builder;
pub use builder::{DerivationDelegateConfig, L1ConfigBuilder, RollupNodeBuilder};

mod follow;
pub use follow::FollowNode;

mod mode;
pub use mode::NodeMode;

mod node;
pub use node::{HEAD_STREAM_POLL_INTERVAL, L1Config, RollupNode};

mod util;
pub use util::ShutdownSignal;
pub(crate) use util::spawn_and_wait;
