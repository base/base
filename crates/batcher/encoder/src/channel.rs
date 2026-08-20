//! One derivation channel and its admission / close types.

use std::{collections::VecDeque, fmt, sync::Arc};

use alloy_primitives::Bytes;
use alloy_rlp::{Encodable, Header};
use base_common_genesis::RollupConfig;
use base_comp::CompressionStream;
use base_protocol::{
    BLOB_DERIVATION_PREFIX_SIZE, BLOB_MAX_DATA_SIZE, BatchType, ChannelId, Frame, SingleBatch,
};

use crate::{BatcherMetrics, CompressionError, EncoderConfig, EncoderConfigError};

/// Result of appending one complete `SingleBatch` to a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelAddOutcome {
    /// The batch was committed and the channel remains below its soft target.
    Accepted,
    /// The batch was committed and reached the optional soft target.
    TargetReached,
    /// The batch was not committed because a hard channel limit would be exceeded.
    Rejected(ChannelLimit),
}

/// Hard derivation limit preventing one batch from joining a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ChannelLimit {
    /// Uncompressed channel RLP bytes exceed the fork-specific decoder limit.
    #[error("channel RLP requires {required} bytes, maximum is {maximum}")]
    RlpBytes {
        /// Required bytes after appending the candidate batch.
        required: usize,
        /// Fork-specific maximum.
        maximum: usize,
    },
    /// The finished channel cannot be represented by sequential `u16` frame numbers.
    #[error("channel requires {required} frames, maximum is {maximum}")]
    FrameCount {
        /// Conservatively required frames.
        required: usize,
        /// Maximum representable frame count.
        maximum: usize,
    },
    /// Compressed bytes and frame storage exceed the assembled-channel limit.
    #[error("assembled channel may require {required} bytes, maximum is {maximum}")]
    AssembledBytes {
        /// Conservative assembled size.
        required: usize,
        /// Fork-specific maximum.
        maximum: usize,
    },
}

/// Why a writable channel stopped accepting batches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelCloseReason {
    /// The optional compressed-size target was reached.
    SoftTarget,
    /// The next batch would exceed a hard derivation limit.
    ProtocolLimit,
    /// The channel reached its operational L1 block timeout.
    Timeout,
    /// The caller explicitly requested an administrative flush.
    Flush,
}

impl ChannelCloseReason {
    /// Returns the bounded metric label for this reason.
    pub const fn metric_label(self) -> &'static str {
        match self {
            Self::SoftTarget => BatcherMetrics::REASON_SOFT_TARGET,
            Self::ProtocolLimit => BatcherMetrics::REASON_PROTOCOL_LIMIT,
            Self::Timeout => BatcherMetrics::REASON_TIMEOUT,
            Self::Flush => BatcherMetrics::REASON_FLUSH,
        }
    }
}

/// Error returned by a channel state transition.
#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    /// Compression failed.
    #[error(transparent)]
    Compression(#[from] CompressionError),
    /// A batch was appended after the channel closed.
    #[error("cannot append a batch to a closed channel")]
    Closed,
    /// A frame requests compressed bytes not present in the channel output.
    #[error("frame requires {requested} bytes, but only {available} are available")]
    OutputUnderflow {
        /// Requested compressed bytes.
        requested: usize,
        /// Available compressed bytes.
        available: usize,
    },
    /// The finished channel cannot be represented by sequential `u16` frame numbers.
    #[error("channel requires {frame_count} frames, exceeding the maximum {maximum}")]
    TooManyFrames {
        /// Number of frames required for the channel.
        frame_count: usize,
        /// Maximum representable frame count.
        maximum: usize,
    },
    /// A terminal frame was requested before the closed channel tail was complete.
    #[error("terminal frame does not consume the complete closed channel tail")]
    InvalidTerminalTransition,
}

/// One encoding channel, from first batch until the safe head covers it.
pub struct ChannelRecord {
    /// Unique derivation channel identifier.
    id: ChannelId,
    /// Rollup rules used for timestamp-dependent protocol limits.
    rollup_config: Arc<RollupConfig>,
    /// Compressor present only while the channel accepts batches.
    compressor: Option<CompressionStream>,
    /// Compressed bytes emitted but not yet assigned to immutable DA artifacts.
    output: VecDeque<Bytes>,
    /// Number of bytes currently available in `output`.
    available_output: usize,
    /// Total compressed bytes emitted by this channel.
    compressed_bytes: usize,
    /// Maximum serialized frame size.
    max_frame_size: usize,
    /// Optional soft compressed-size target.
    compressed_size_target: Option<usize>,
    /// Number of accepted uncompressed RLP bytes.
    input_bytes: usize,
    /// Reused buffer for one encoded `SingleBatch` wire value.
    candidate_scratch: Vec<u8>,
    /// Index of the first buffered L2 block encoded into the channel.
    block_start: usize,
    /// Number of buffered L2 blocks encoded into the channel.
    blocks_added: usize,
    /// Estimated DA bytes represented by the accepted L2 blocks.
    da_backlog_bytes: u64,
    /// L1 block observed when the channel opened.
    opened_l1_block: u64,
    /// Absolute L1 deadline for closure, then partial-tail release.
    deadline_l1_block: u64,
    /// Next frame number assigned by DA egress.
    next_frame_number: usize,
    /// Whether the terminal frame still needs to be emitted.
    terminal_pending: bool,
    /// Earliest L1 inclusion block among confirmed artifacts.
    first_confirmed_l1_block: Option<u64>,
    /// Latest L1 inclusion block among confirmed artifacts.
    last_confirmed_l1_block: Option<u64>,
}

impl fmt::Debug for ChannelRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChannelRecord")
            .field("id", &self.id)
            .field("block_start", &self.block_start)
            .field("blocks_added", &self.blocks_added)
            .field("available_output", &self.available_output)
            .field("compressed_bytes", &self.compressed_bytes)
            .field("opened_l1_block", &self.opened_l1_block)
            .field("deadline_l1_block", &self.deadline_l1_block)
            .field("next_frame_number", &self.next_frame_number)
            .field("terminal_pending", &self.terminal_pending)
            .finish()
    }
}

impl ChannelRecord {
    /// Max frames per channel. Derivation cannot reassemble frame number `u16::MAX`.
    pub const MAX_FRAMES: usize = u16::MAX as usize;

    /// Creates an empty writable channel at the tail of the FIFO.
    ///
    /// # Errors
    ///
    /// Returns [`EncoderConfigError`] when `config` violates an encoder limit.
    pub fn new(
        id: ChannelId,
        rollup_config: Arc<RollupConfig>,
        config: &EncoderConfig,
        block_start: usize,
        opened_l1_block: u64,
    ) -> Result<Self, EncoderConfigError> {
        config.validate()?;

        let duration_blocks = config.max_channel_duration - config.sub_safety_margin;
        Ok(Self {
            id,
            rollup_config,
            compressor: Some(CompressionStream::new(config.compression_algo)),
            output: VecDeque::new(),
            available_output: 0,
            compressed_bytes: 0,
            max_frame_size: config.max_frame_size,
            compressed_size_target: config.compressed_size_target,
            input_bytes: 0,
            candidate_scratch: Vec::new(),
            block_start,
            blocks_added: 0,
            da_backlog_bytes: 0,
            opened_l1_block,
            deadline_l1_block: opened_l1_block + duration_blocks,
            next_frame_number: 0,
            terminal_pending: false,
            first_confirmed_l1_block: None,
            last_confirmed_l1_block: None,
        })
    }

    /// Returns the channel identifier.
    pub const fn id(&self) -> ChannelId {
        self.id
    }

    /// Returns whether the channel still accepts batches.
    pub const fn is_open(&self) -> bool {
        self.compressor.is_some()
    }

    /// Returns whether no batch has been accepted.
    pub const fn is_empty(&self) -> bool {
        self.blocks_added == 0
    }

    /// Returns the number of accepted uncompressed RLP bytes.
    pub fn input_bytes(&self) -> u64 {
        u64::try_from(self.input_bytes).unwrap_or(u64::MAX)
    }

    /// Returns the total compressed stream bytes emitted so far.
    pub fn compressed_bytes(&self) -> u64 {
        u64::try_from(self.compressed_bytes).unwrap_or(u64::MAX)
    }

    /// Returns compressed bytes not yet assigned to a DA artifact.
    pub const fn available_output(&self) -> usize {
        self.available_output
    }

    /// Returns the maximum data bytes carried by one frame.
    pub const fn max_frame_data(&self) -> usize {
        self.max_frame_size - Frame::ENCODED_OVERHEAD
    }

    /// Returns the L1 block observed when the channel opened.
    pub const fn opened_l1_block(&self) -> u64 {
        self.opened_l1_block
    }

    /// Whether `l1_head` has reached the channel deadline.
    pub const fn deadline_due(&self, l1_head: u64) -> bool {
        l1_head >= self.deadline_l1_block
    }

    /// Make a closed partial tail eligible at `l1_head` without postponing the deadline.
    pub fn release_at(&mut self, l1_head: u64) {
        self.deadline_l1_block = self.deadline_l1_block.min(l1_head);
    }

    /// Returns the accepted buffered block range.
    pub const fn block_range(&self) -> std::ops::Range<usize> {
        self.block_start..self.block_start + self.blocks_added
    }

    /// Returns the number of accepted L2 blocks.
    pub const fn blocks_added(&self) -> usize {
        self.blocks_added
    }

    /// Returns the estimated DA backlog represented by accepted blocks.
    pub const fn da_backlog_bytes(&self) -> u64 {
        self.da_backlog_bytes
    }

    /// Returns whether the terminal frame has been emitted.
    pub const fn framing_complete(&self) -> bool {
        self.compressor.is_none() && !self.terminal_pending
    }

    /// Returns whether a terminal frame remains after the available output.
    pub const fn terminal_pending(&self) -> bool {
        self.terminal_pending
    }

    /// Returns the number of frames assigned so far.
    pub const fn frame_count(&self) -> usize {
        self.next_frame_number
    }

    /// Append `batch` if hard limits hold; otherwise reject without mutating the stream.
    pub fn add_batch(
        &mut self,
        batch: SingleBatch,
        da_backlog_bytes: u64,
    ) -> Result<ChannelAddOutcome, ChannelError> {
        let timestamp = batch.timestamp;

        // Wire form of one Single batch: RLP string header, type byte, payload.
        self.candidate_scratch.clear();
        Header { list: false, payload_length: 1 + batch.length() }
            .encode(&mut self.candidate_scratch);
        self.candidate_scratch.push(BatchType::Single as u8);
        batch.encode(&mut self.candidate_scratch);

        let next_input_bytes = self.input_bytes.saturating_add(self.candidate_scratch.len());
        let max_channel_bytes =
            usize::try_from(self.rollup_config.max_rlp_bytes_per_channel(timestamp))
                .unwrap_or(usize::MAX);
        let max_frame_data = self.max_frame_data();
        let Some(compressor) = self.compressor.as_mut() else {
            return Err(ChannelError::Closed);
        };

        // Worst-case compressed size of the finished channel, including this batch.
        let max_compressed_bytes = compressor.max_output_size(next_input_bytes);
        let max_frame_count = max_compressed_bytes.div_ceil(max_frame_data);

        // Blob packing can split output at blob boundaries, so a channel may
        // need more frames than `max_frame_size` packing alone would suggest.
        let blob_payload_capacity = BLOB_MAX_DATA_SIZE - BLOB_DERIVATION_PREFIX_SIZE;
        let max_blob_boundary_frames = max_compressed_bytes
            .div_ceil(blob_payload_capacity - Frame::ENCODED_OVERHEAD)
            .saturating_add(1);
        let max_total_frames = max_frame_count.saturating_add(max_blob_boundary_frames);
        let max_assembled_bytes =
            max_compressed_bytes.saturating_add(max_total_frames.saturating_mul(Frame::OVERHEAD));

        // Reject against projected limits before mutating the stream.
        if next_input_bytes > max_channel_bytes {
            return Ok(ChannelAddOutcome::Rejected(ChannelLimit::RlpBytes {
                required: next_input_bytes,
                maximum: max_channel_bytes,
            }));
        }
        if max_total_frames > Self::MAX_FRAMES {
            return Ok(ChannelAddOutcome::Rejected(ChannelLimit::FrameCount {
                required: max_total_frames,
                maximum: Self::MAX_FRAMES,
            }));
        }
        if max_assembled_bytes > max_channel_bytes {
            return Ok(ChannelAddOutcome::Rejected(ChannelLimit::AssembledBytes {
                required: max_assembled_bytes,
                maximum: max_channel_bytes,
            }));
        }

        // Commit once. Newly stable bytes go into the FIFO.
        let output = compressor.append(&self.candidate_scratch)?;
        let total_output = compressor.output_size();

        self.push_output(output);
        self.input_bytes = next_input_bytes;
        self.blocks_added += 1;
        self.da_backlog_bytes += da_backlog_bytes;

        if self.compressed_size_target.is_some_and(|target| total_output >= target) {
            Ok(ChannelAddOutcome::TargetReached)
        } else {
            Ok(ChannelAddOutcome::Accepted)
        }
    }

    /// Stop accepting batches and finish the compressor.
    pub fn close(&mut self) -> Result<(), ChannelError> {
        let Some(compressor) = self.compressor.take() else {
            return Ok(());
        };

        self.push_output(compressor.finish()?);
        self.candidate_scratch = Vec::new();
        self.terminal_pending = true;
        Ok(())
    }

    /// Cut the next numbered frame from the compressed FIFO.
    pub fn take_frame(&mut self, data_len: usize, is_last: bool) -> Result<Frame, ChannelError> {
        if data_len > self.available_output {
            return Err(ChannelError::OutputUnderflow {
                requested: data_len,
                available: self.available_output,
            });
        }
        if self.next_frame_number >= Self::MAX_FRAMES {
            return Err(ChannelError::TooManyFrames {
                frame_count: self.next_frame_number + 1,
                maximum: Self::MAX_FRAMES,
            });
        }

        // `is_last` is valid only after close, on the exact remaining tail.
        if is_last
            && (self.compressor.is_some()
                || data_len != self.available_output
                || !self.terminal_pending)
        {
            return Err(ChannelError::InvalidTerminalTransition);
        }

        let number = self.next_frame_number as u16;
        let data = self.take_output(data_len);

        self.next_frame_number += 1;
        if is_last {
            self.terminal_pending = false;
        }

        Ok(Frame { id: self.id, number, data, is_last })
    }

    /// Drain `len` bytes from the compressed FIFO.
    fn take_output(&mut self, len: usize) -> Vec<u8> {
        debug_assert!(len <= self.available_output);
        debug_assert_eq!(
            self.available_output,
            self.output.iter().map(|chunk| chunk.len()).sum::<usize>()
        );

        let mut remaining = len;
        let mut output = Vec::with_capacity(len);
        while remaining > 0 {
            let chunk = self.output.pop_front().expect("output length validated before mutation");
            let consumed = remaining.min(chunk.len());
            output.extend_from_slice(&chunk[..consumed]);
            remaining -= consumed;

            if consumed < chunk.len() {
                self.output.push_front(chunk.slice(consumed..));
            }
        }

        self.available_output -= len;
        output
    }

    /// Record L1 inclusion for timeout/replay (min/max over artifacts).
    pub fn record_confirmation(&mut self, l1_block: u64) {
        self.first_confirmed_l1_block =
            Some(self.first_confirmed_l1_block.map_or(l1_block, |first| first.min(l1_block)));
        self.last_confirmed_l1_block =
            Some(self.last_confirmed_l1_block.map_or(l1_block, |last| last.max(l1_block)));
    }

    /// Returns the earliest confirmed artifact inclusion block.
    pub const fn first_confirmed_l1_block(&self) -> Option<u64> {
        self.first_confirmed_l1_block
    }

    /// Returns the latest confirmed artifact inclusion block.
    pub const fn last_confirmed_l1_block(&self) -> Option<u64> {
        self.last_confirmed_l1_block
    }

    /// Shifts `block_start` after the encoder drops a safe prefix of `blocks`.
    pub const fn rebase_after_prune(&mut self, prune_count: usize) {
        let old_end = self.block_start + self.blocks_added;
        self.block_start = self.block_start.saturating_sub(prune_count);
        self.blocks_added = old_end.saturating_sub(prune_count).saturating_sub(self.block_start);
    }

    /// Enqueues newly transferred compressor bytes for later framing.
    fn push_output(&mut self, output: Vec<u8>) {
        if output.is_empty() {
            return;
        }
        self.available_output += output.len();
        self.compressed_bytes += output.len();
        self.output.push_back(Bytes::from(output));
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;

    use super::*;
    use crate::CompressionAlgo;

    fn batch(transaction_len: usize) -> SingleBatch {
        let mut state = 1u64;
        let transaction = (0..transaction_len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect::<Vec<_>>();
        SingleBatch {
            parent_hash: B256::ZERO,
            epoch_num: 0,
            epoch_hash: B256::ZERO,
            timestamp: 0,
            transactions: vec![Bytes::from(transaction)],
        }
    }

    fn channel(config: EncoderConfig) -> ChannelRecord {
        ChannelRecord::new(ChannelId::default(), Arc::new(RollupConfig::default()), &config, 0, 0)
            .unwrap()
    }

    #[test]
    fn output_fifo_preserves_transferred_stream_order() {
        let mut channel = channel(EncoderConfig::default());
        channel.push_output(vec![1, 2]);
        channel.push_output(vec![3, 4, 5]);

        assert_eq!(channel.take_output(4), vec![1, 2, 3, 4]);
        assert_eq!(channel.take_output(1), vec![5]);
        assert_eq!(channel.available_output(), 0);
    }

    #[test]
    fn soft_target_accepts_complete_batch_before_closing() {
        let config = EncoderConfig {
            compression_algo: CompressionAlgo::Zlib,
            compressed_size_target: Some(1),
            ..EncoderConfig::default()
        };
        let mut channel = channel(config);

        assert_eq!(
            channel.add_batch(batch(100_000), 100_000).unwrap(),
            ChannelAddOutcome::TargetReached
        );
        assert_eq!(channel.blocks_added(), 1);
        assert!(channel.input_bytes() > 0);
    }

    #[test]
    fn cumulative_rlp_limit_rejects_without_mutating_stream() {
        let mut channel = channel(EncoderConfig::default());
        let maximum = channel.rollup_config.max_rlp_bytes_per_channel(0) as usize;
        channel.input_bytes = maximum;

        assert!(matches!(
            channel.add_batch(batch(1), 1).unwrap(),
            ChannelAddOutcome::Rejected(ChannelLimit::RlpBytes { .. })
        ));
        assert_eq!(channel.input_bytes, maximum);
        assert_eq!(channel.blocks_added(), 0);
        assert_eq!(channel.compressed_bytes(), 0);
    }

    #[test]
    fn frame_number_limit_rejects_without_mutating_stream() {
        let config = EncoderConfig {
            max_frame_size: Frame::ENCODED_OVERHEAD + 1,
            ..EncoderConfig::default()
        };
        let mut channel = channel(config);

        assert!(matches!(
            channel.add_batch(batch(ChannelRecord::MAX_FRAMES + 1), 1).unwrap(),
            ChannelAddOutcome::Rejected(ChannelLimit::FrameCount { .. })
        ));
        assert!(channel.is_empty());
        assert_eq!(channel.compressed_bytes(), 0);
    }
}
