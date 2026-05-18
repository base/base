//! Block subscription implementations for the batcher service.

use base_batcher_source::{KeepAliveSubscription, PendingSubscription};
use base_common_consensus::BaseBlock;

/// A WebSocket-backed block subscription.
///
/// Owns the WS provider so the underlying connection is not dropped when the
/// stream is handed to [`HybridBlockSource`].
///
/// [`HybridBlockSource`]: base_batcher_source::HybridBlockSource
pub type WsBlockSubscription = KeepAliveSubscription<BaseBlock>;

/// A no-op block subscription that never yields blocks.
///
/// Used when the L2 RPC is not a WebSocket URL and subscription is unavailable;
/// [`HybridBlockSource`] will rely entirely on the polling path.
///
/// [`HybridBlockSource`]: base_batcher_source::HybridBlockSource
pub type NullSubscription = PendingSubscription<BaseBlock>;
