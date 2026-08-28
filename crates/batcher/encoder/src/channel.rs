//! Channel state machine types.

use std::{fmt, ops::Range, sync::Arc};

use base_common_genesis::RollupConfig;
use base_comp::{ChannelOut, ChannelOutError, CompressorError, ShadowCompressor};
use base_protocol::{ChannelId, Frame, SingleBatch};

use crate::EncoderConfig;

/// Why a candidate block did not fit in an open channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ChannelFullReason {
    /// The candidate exceeded the configured compressed output target.
    #[error("compressed output target exceeded")]
    CompressedOutput,
    /// The candidate exceeded the protocol RLP input limit.
    #[error("maximum RLP input bytes reached")]
    RlpInput,
}

/// Result of attempting to add one L2 block to an open channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelAddOutcome {
    /// The block was accepted and the channel can accept more blocks.
    Accepted,
    /// The block was not accepted; the caller decides whether it can be retried.
    Rejected(ChannelFullReason),
}

/// Failure while building or finalizing an open channel.
#[derive(Debug, thiserror::Error)]
pub enum OpenChannelError {
    /// A single-batch channel or frame operation failed.
    #[error("channel output failed: {0}")]
    Output(#[from] ChannelOutError),
}

/// Single-batch channel currently accepting L2 blocks.
///
/// The encoder owns at most one `OpenChannel`. This type keeps the compressed
/// channel output together with its lifecycle metadata.
pub struct OpenChannel {
    /// Incrementally compressed Single-batch channel output.
    pub out: ChannelOut<ShadowCompressor>,
    /// Index of the first block encoded into this channel.
    pub block_start: usize,
    /// L1 block number when this channel was opened (for `MaxChannelDuration`).
    pub opened_at_l1: u64,
    /// Number of L2 blocks fed into this channel so far.
    pub blocks_added: usize,
    /// Estimated DA bytes for blocks fed into this channel.
    pub da_backlog_bytes: u64,
}

impl fmt::Debug for OpenChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenChannel")
            .field("channel_id", &self.id())
            .field("block_start", &self.block_start)
            .field("opened_at_l1", &self.opened_at_l1)
            .field("blocks_added", &self.blocks_added)
            .field("da_backlog_bytes", &self.da_backlog_bytes)
            .finish()
    }
}

impl OpenChannel {
    /// Creates the channel requested by the encoder when `step` needs one.
    pub fn new(
        id: ChannelId,
        rollup_config: Arc<RollupConfig>,
        config: &EncoderConfig,
        block_start: usize,
        opened_at_l1: u64,
    ) -> Self {
        let compressor =
            ShadowCompressor::new(config.target_frame_size as u64, config.compression_algo);
        let out = ChannelOut::new(id, rollup_config, compressor);
        Self { out, block_start, opened_at_l1, blocks_added: 0, da_backlog_bytes: 0 }
    }

    /// Returns the channel identifier.
    pub const fn id(&self) -> ChannelId {
        self.out.id()
    }

    /// Returns the accepted uncompressed RLP input length.
    pub const fn input_bytes(&self) -> u64 {
        self.out.input_bytes()
    }

    /// Attempts to add one L2 block to the open channel.
    ///
    /// The encoder calls this once per `step`. Accepted outcomes also update the
    /// channel's block count and DA backlog before the caller advances its cursor.
    pub fn add_block(
        &mut self,
        batch: SingleBatch,
        da_backlog_bytes: u64,
    ) -> Result<ChannelAddOutcome, OpenChannelError> {
        let outcome = match self.out.add_single_batch(batch) {
            Ok(()) => ChannelAddOutcome::Accepted,
            Err(ChannelOutError::Compression(CompressorError::Full)) => {
                ChannelAddOutcome::Rejected(ChannelFullReason::CompressedOutput)
            }
            Err(ChannelOutError::ExceedsMaxRlpBytesPerChannel) => {
                ChannelAddOutcome::Rejected(ChannelFullReason::RlpInput)
            }
            Err(error) => return Err(error.into()),
        };

        if outcome == ChannelAddOutcome::Accepted {
            self.blocks_added += 1;
            self.da_backlog_bytes += da_backlog_bytes;
        }
        Ok(outcome)
    }

    /// Consumes the open channel and returns all frames ready for submission.
    ///
    /// The encoder calls this exactly once when size, timeout, or a flush
    /// request closes the channel.
    pub fn into_frames(self, max_frame_size: usize) -> Result<Vec<Frame>, OpenChannelError> {
        Ok(self.out.into_frames(max_frame_size)?)
    }
}

/// Submission lifecycle of a frame in a closed channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameState {
    /// The frame is available for submission.
    Ready,
    /// The frame was submitted and is awaiting an outcome.
    Pending,
    /// The frame was confirmed on L1.
    Confirmed,
}

/// A closed channel retained while its frames and derivation progress are tracked.
#[derive(Debug)]
pub struct ReadyChannel {
    /// The channel identifier.
    pub id: ChannelId,
    /// All frames, in order. Wrapped in [`Arc`] so that the slice handed to
    /// [`BatchSubmission`] is a cheap pointer copy rather than a deep clone of
    /// the frame payload (up to `max_frame_size` bytes per frame).
    pub frames: Vec<Arc<Frame>>,
    /// Submission state for each frame.
    pub frame_states: Vec<FrameState>,
    /// Exact input block range encoded into this channel.
    pub encoded_block_range: Range<usize>,
    /// DA bytes still represented by this closed channel's frames.
    pub da_backlog_bytes: u64,
    /// Earliest L1 block that confirmed any frame from this channel.
    pub first_confirmed_l1_block: Option<u64>,
    /// Latest L1 block that confirmed any frame from this channel.
    pub last_confirmed_l1_block: Option<u64>,
}

impl ReadyChannel {
    /// Returns the first contiguous range of frames available for submission.
    ///
    /// Per-frame state replaces a monotonic cursor so retries can make only the
    /// affected frames ready again without resubmitting confirmed frames.
    pub fn next_ready_frame_range(&self) -> Option<Range<usize>> {
        let start = self.frame_states.iter().position(|state| *state == FrameState::Ready)?;
        let count = self.frame_states[start..]
            .iter()
            .take_while(|state| **state == FrameState::Ready)
            .count();
        Some(start..start + count)
    }

    /// Marks a ready frame range as awaiting an L1 submission outcome.
    pub fn mark_pending(&mut self, range: Range<usize>) {
        debug_assert!(
            self.frame_states[range.clone()].iter().all(|state| *state == FrameState::Ready)
        );
        self.frame_states[range].fill(FrameState::Pending);
    }

    /// Marks a pending frame range as confirmed on L1.
    pub fn mark_confirmed(&mut self, range: Range<usize>) {
        debug_assert!(
            self.frame_states[range.clone()].iter().all(|state| *state == FrameState::Pending)
        );
        self.frame_states[range].fill(FrameState::Confirmed);
    }

    /// Returns a pending frame range to the submission queue.
    pub fn mark_ready(&mut self, range: Range<usize>) {
        debug_assert!(
            self.frame_states[range.clone()].iter().all(|state| *state == FrameState::Pending)
        );
        self.frame_states[range].fill(FrameState::Ready);
    }

    /// Returns `true` once every frame in the channel is confirmed on L1.
    pub fn is_fully_confirmed(&self) -> bool {
        self.frame_states.iter().all(|state| *state == FrameState::Confirmed)
    }
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use base_protocol::Frame;

    use super::{FrameState, ReadyChannel};

    fn channel_with_states(frame_states: Vec<FrameState>) -> ReadyChannel {
        let frames = frame_states.iter().map(|_| Arc::new(Frame::default())).collect();
        ReadyChannel {
            id: Default::default(),
            frames,
            frame_states,
            encoded_block_range: 0..0,
            da_backlog_bytes: 0,
            first_confirmed_l1_block: None,
            last_confirmed_l1_block: None,
        }
    }

    #[test]
    fn returns_first_contiguous_ready_range() {
        let channel = channel_with_states(vec![
            FrameState::Confirmed,
            FrameState::Ready,
            FrameState::Ready,
            FrameState::Pending,
            FrameState::Ready,
        ]);

        assert_eq!(channel.next_ready_frame_range(), Some(1..3));
    }

    #[test]
    fn transitions_only_selected_frame_range() {
        let mut channel = channel_with_states(vec![
            FrameState::Ready,
            FrameState::Ready,
            FrameState::Pending,
            FrameState::Confirmed,
        ]);

        channel.mark_pending(0..2);
        channel.mark_confirmed(0..1);
        channel.mark_ready(1..3);

        assert_eq!(
            channel.frame_states,
            [FrameState::Confirmed, FrameState::Ready, FrameState::Ready, FrameState::Confirmed,]
        );
        assert_eq!(channel.next_ready_frame_range(), Some(1..3));
    }
}
