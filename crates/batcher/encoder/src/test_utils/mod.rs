//! Test utilities for the batcher encoder.

use std::collections::VecDeque;

use base_common_consensus::BaseBlock;
use base_protocol::{BlockInfo, ChannelId, Frame};

use crate::{
    BatchPipeline, BatchSubmission, Channel, DerivationReconciliation, ReorgError, StepError,
    StepResult, SubmissionId,
};

/// A mock implementation of [`BatchPipeline`] for testing downstream consumers
/// such as the [`BatchDriver`](crate::BatchPipeline).
///
/// Records all method calls for assertion in tests.
#[derive(Debug, Default)]
pub struct MockBatchPipeline {
    /// Blocks that were added via [`add_block`](BatchPipeline::add_block).
    pub blocks_added: Vec<BaseBlock>,
    /// Number of times [`step`](BatchPipeline::step) was called.
    pub steps_taken: usize,
    /// Submissions to return from [`next_submission`](BatchPipeline::next_submission).
    pub submissions: VecDeque<BatchSubmission>,
    /// Confirmed submissions (id, `l1_block`).
    pub confirmed: Vec<(SubmissionId, u64)>,
    /// Requeued submission ids.
    pub requeued: Vec<SubmissionId>,
    /// Number of times [`reset`](BatchPipeline::reset) was called.
    pub resets: usize,
    /// L1 heads that were advanced to.
    pub l1_heads: Vec<u64>,
    /// Safe L2 block numbers passed to derivation reconciliation.
    pub safe_l2_numbers_reconciled: Vec<u64>,
}

impl BatchPipeline for MockBatchPipeline {
    fn add_block(&mut self, block: BaseBlock) -> Result<(), (ReorgError, Box<BaseBlock>)> {
        self.blocks_added.push(block);
        Ok(())
    }

    fn step(&mut self) -> Result<StepResult, StepError> {
        self.steps_taken += 1;
        Ok(StepResult::Idle)
    }

    fn next_submission(&mut self) -> Option<BatchSubmission> {
        self.submissions.pop_front()
    }

    fn has_ready_submission(&self) -> bool {
        !self.submissions.is_empty()
    }

    fn confirm(&mut self, id: SubmissionId, l1_block: u64) {
        self.confirmed.push((id, l1_block));
    }

    fn requeue(&mut self, id: SubmissionId) {
        self.requeued.push(id);
    }

    fn flush(&mut self) -> Result<(), StepError> {
        Ok(())
    }

    fn advance_l1_head(&mut self, l1_block: u64) {
        self.l1_heads.push(l1_block);
    }

    fn reconcile_derivation(
        &mut self,
        safe_l2: BlockInfo,
        _: Option<u64>,
    ) -> DerivationReconciliation {
        self.safe_l2_numbers_reconciled.push(safe_l2.number);
        DerivationReconciliation::Consistent
    }

    fn reset(&mut self) {
        self.resets += 1;
    }

    fn da_backlog_bytes(&self) -> u64 {
        0
    }
}

/// One-shot framing for Span fixtures and tests.
///
/// Production encoding cuts frames incrementally via [`Channel::take_frame`].
/// This helper still splits a fully compressed channel, which the action harness
/// uses to inject historical Span payloads.
#[derive(Debug)]
pub struct ChannelFramer;

/// Failure from [`ChannelFramer::split`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ChannelFramerError {
    /// The configured frame size cannot carry channel data.
    #[error("max frame size {actual} is smaller than the minimum {minimum}")]
    FrameSizeTooSmall {
        /// Configured serialized frame size.
        actual: usize,
        /// Smallest serialized frame that can carry one data byte.
        minimum: usize,
    },
    /// The configured frame size exceeds the protocol decoder limit.
    #[error("max frame size {actual} exceeds the protocol maximum {maximum}")]
    FrameSizeTooLarge {
        /// Configured serialized frame size.
        actual: usize,
        /// Largest serialized frame accepted by the protocol decoder.
        maximum: usize,
    },
    /// The compressed channel requires more frames than derivation can reassemble.
    #[error("compressed channel requires {frame_count} frames, exceeding the maximum {maximum}")]
    TooManyFrames {
        /// Number of frames required for the channel.
        frame_count: usize,
        /// Largest frame count accepted by derivation.
        maximum: usize,
    },
}

impl ChannelFramer {
    /// Splits `channel_data` into ordered frames no larger than `max_frame_size`.
    pub fn split(
        id: ChannelId,
        channel_data: Vec<u8>,
        max_frame_size: usize,
    ) -> Result<Vec<Frame>, ChannelFramerError> {
        if channel_data.is_empty() {
            return Ok(Vec::new());
        }

        let minimum = Frame::ENCODED_OVERHEAD + 1;
        if max_frame_size < minimum {
            return Err(ChannelFramerError::FrameSizeTooSmall { actual: max_frame_size, minimum });
        }

        let maximum = Frame::ENCODED_OVERHEAD + Frame::MAX_LEN;
        if max_frame_size > maximum {
            return Err(ChannelFramerError::FrameSizeTooLarge { actual: max_frame_size, maximum });
        }

        let payload_size = max_frame_size - Frame::ENCODED_OVERHEAD;
        let frame_count = channel_data.len().div_ceil(payload_size);
        if frame_count > Channel::MAX_FRAMES {
            return Err(ChannelFramerError::TooManyFrames {
                frame_count,
                maximum: Channel::MAX_FRAMES,
            });
        }

        let mut bytes = channel_data.into_iter();
        let mut frames = Vec::with_capacity(frame_count);
        for index in 0..frame_count {
            let data = bytes.by_ref().take(payload_size).collect();
            frames.push(Frame::new(id, index as u16, data, index + 1 == frame_count));
        }
        Ok(frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_channel_has_no_frames() {
        let frames = ChannelFramer::split(ChannelId::default(), Vec::new(), 100).unwrap();
        assert!(frames.is_empty());
    }

    #[test]
    fn rejects_frame_size_without_payload_capacity() {
        let err = ChannelFramer::split(ChannelId::default(), vec![1], Frame::ENCODED_OVERHEAD)
            .unwrap_err();

        assert_eq!(
            err,
            ChannelFramerError::FrameSizeTooSmall {
                actual: Frame::ENCODED_OVERHEAD,
                minimum: Frame::ENCODED_OVERHEAD + 1,
            }
        );
    }

    #[test]
    fn rejects_frame_size_above_protocol_limit() {
        let actual = Frame::ENCODED_OVERHEAD + Frame::MAX_LEN + 1;
        let err = ChannelFramer::split(ChannelId::default(), vec![1], actual).unwrap_err();

        assert_eq!(
            err,
            ChannelFramerError::FrameSizeTooLarge {
                actual,
                maximum: Frame::ENCODED_OVERHEAD + Frame::MAX_LEN,
            }
        );
    }

    #[test]
    fn splits_and_numbers_frames() {
        let id = [0xAB; 16];
        let max_frame_size = Frame::ENCODED_OVERHEAD + 2;
        let frames = ChannelFramer::split(id, vec![1, 2, 3, 4, 5], max_frame_size).unwrap();

        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0], Frame::new(id, 0, vec![1, 2], false));
        assert_eq!(frames[1], Frame::new(id, 1, vec![3, 4], false));
        assert_eq!(frames[2], Frame::new(id, 2, vec![5], true));
    }

    #[test]
    fn rejects_non_derivable_frame_count() {
        let channel_data = vec![0; Channel::MAX_FRAMES + 1];
        let err =
            ChannelFramer::split(ChannelId::default(), channel_data, Frame::ENCODED_OVERHEAD + 1)
                .unwrap_err();

        assert_eq!(
            err,
            ChannelFramerError::TooManyFrames {
                frame_count: Channel::MAX_FRAMES + 1,
                maximum: Channel::MAX_FRAMES,
            }
        );
    }
}
