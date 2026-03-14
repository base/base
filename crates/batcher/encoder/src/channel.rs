//! Channel state machine types.

use std::{fmt, ops::Range};

use base_comp::{ChannelOut, ShadowCompressor};
use base_protocol::{ChannelId, Frame};

/// A channel currently being built (accepting batches).
pub struct OpenChannel {
    /// The underlying channel writer.
    pub out: ChannelOut<ShadowCompressor>,
    /// L1 block number when this channel was opened (for `MaxChannelDuration`).
    pub opened_at_l1: u64,
}

impl fmt::Debug for OpenChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenChannel")
            .field("channel_id", &self.out.id)
            .field("opened_at_l1", &self.opened_at_l1)
            .finish()
    }
}

/// A channel that has been closed and is ready for frame submission.
#[derive(Debug)]
pub struct ReadyChannel {
    /// The channel identifier.
    pub id: ChannelId,
    /// All frames, in order.
    pub frames: Vec<Frame>,
    /// Next frame index to submit (cursor). Rewound on requeue.
    pub cursor: usize,
    /// Which input blocks this channel covers (indices into the encoder's block queue).
    pub block_range: Range<usize>,
    /// Number of in-flight submissions for this channel.
    pub pending_confirmations: usize,
    /// Number of frames that have been confirmed.
    pub confirmed_count: usize,
}

/// Tracks a pending submission back to its channel and frame.
#[derive(Debug, Clone)]
pub struct PendingRef {
    /// Index into the `ready_channels` deque.
    pub channel_idx: usize,
    /// Which frame in the ready channel this submission covers.
    pub frame_idx: usize,
}
