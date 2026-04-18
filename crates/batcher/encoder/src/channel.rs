//! Channel state machine types.

use std::{fmt, ops::Range, sync::Arc};

use base_comp::{ChannelOut, ShadowCompressor};
use base_protocol::{ChannelId, Frame};

/// A channel currently being built (accepting batches).
pub struct OpenChannel {
    /// The underlying channel writer.
    pub out: ChannelOut<ShadowCompressor>,
    /// L1 block number when this channel was opened (for `MaxChannelDuration`).
    pub opened_at_l1: u64,
    /// Number of L2 blocks fed into this channel so far.
    pub blocks_added: usize,
}

impl fmt::Debug for OpenChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenChannel")
            .field("channel_id", &self.out.id)
            .field("opened_at_l1", &self.opened_at_l1)
            .field("blocks_added", &self.blocks_added)
            .finish()
    }
}

/// A channel that has been closed and is ready for frame submission.
#[derive(Debug)]
pub struct ReadyChannel {
    /// The channel identifier.
    pub id: ChannelId,
    /// All frames, in order. Wrapped in [`Arc`] so that the slice handed to
    /// [`BatchSubmission`] is a cheap pointer copy rather than a deep clone of
    /// the frame payload (up to `max_frame_size` bytes per frame).
    pub frames: Vec<Arc<Frame>>,
    /// Next frame index to submit (cursor). Rewound on requeue.
    pub cursor: usize,
    /// Which input blocks this channel covers (indices into the encoder's block queue).
    pub block_range: Range<usize>,
    /// Number of in-flight submissions for this channel.
    pub pending_confirmations: usize,
    /// Number of frames that have been confirmed.
    pub confirmed_count: usize,
}

/// Tracks a pending submission back to its channel and frame range.
#[derive(Debug, Clone)]
pub struct PendingRef {
    /// Index into the `ready_channels` deque.
    pub channel_idx: usize,
    /// Index of the first frame in the ready channel covered by this submission.
    pub frame_start: usize,
    /// Number of frames included in this submission (1 when `target_num_frames == 1`).
    pub frame_count: usize,
}
