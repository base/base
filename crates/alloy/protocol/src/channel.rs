//! Channel Types

use alloc::{vec, vec::Vec};
use alloy_primitives::{map::HashMap, Bytes};
use op_alloy_genesis::RollupConfig;

use crate::{block::BlockInfo, frame::Frame};

/// The best compression.
const BEST_COMPRESSION: u8 = 9;

/// The frame overhead.
const FRAME_V0_OVERHEAD: usize = 23;

/// [CHANNEL_ID_LENGTH] is the length of the channel ID.
pub const CHANNEL_ID_LENGTH: usize = 16;

/// [ChannelId] is an opaque identifier for a channel.
pub type ChannelId = [u8; CHANNEL_ID_LENGTH];

/// An error returned by the [ChannelOut] when adding single batches.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ChannelOutError {
    /// The channel is closed.
    #[error("The channel is already closed")]
    ChannelClosed,
    /// The max frame size is too small.
    #[error("The max frame size is too small")]
    MaxFrameSizeTooSmall,
    /// Missing compressed batch data.
    #[error("Missing compressed batch data")]
    MissingData,
    /// An error from brotli compression.
    #[error("Error from Brotli compression")]
    BrotliCompression,
    /// An error encoding the `Batch`.
    #[error("Error encoding the batch")]
    BatchEncoding,
}

/// [ChannelOut] constructs a channel from compressed, encoded batch data.
#[derive(Debug, Clone)]
pub struct ChannelOut<'a> {
    /// The unique identifier for the channel.
    pub id: ChannelId,
    /// A reference to the [RollupConfig] used to
    /// check the max RLP bytes per channel when
    /// encoding and accepting the
    pub config: &'a RollupConfig,
    /// The rlp length of the channel.
    pub rlp_length: u64,
    /// Whether the channel is closed.
    pub closed: bool,
    /// The frame number.
    pub frame_number: u16,
    /// Compressed batch data.
    pub compressed: Option<Bytes>,
}

impl<'a> ChannelOut<'a> {
    /// Creates a new [ChannelOut] with the given [ChannelId].
    pub const fn new(id: ChannelId, config: &'a RollupConfig) -> Self {
        Self { id, config, rlp_length: 0, frame_number: 0, closed: false, compressed: None }
    }

    /// Accepts the given [crate::Batch] data into the [ChannelOut], compressing it
    /// into frames.
    #[cfg(feature = "std")]
    pub fn add_batch(&mut self, batch: crate::Batch) -> Result<(), ChannelOutError> {
        if self.closed {
            return Err(ChannelOutError::ChannelClosed);
        }

        // Encode the batch.
        let mut buf = vec![];
        batch.encode(&mut buf).map_err(|_| ChannelOutError::BatchEncoding)?;

        // Validate that the RLP length is within the channel's limits.
        let max_rlp_bytes_per_channel = self.config.max_rlp_bytes_per_channel(batch.timestamp());
        if self.rlp_length + buf.len() as u64 > max_rlp_bytes_per_channel {
            return Err(ChannelOutError::ChannelClosed);
        }

        if self.config.is_fjord_active(batch.timestamp()) {
            let level = crate::BrotliLevel::Brotli10;
            let compressed = crate::compress_brotli(&buf, level)
                .map_err(|_| ChannelOutError::BrotliCompression)?;
            self.compressed = Some(compressed.into());
        } else {
            self.compressed =
                Some(miniz_oxide::deflate::compress_to_vec(&buf, BEST_COMPRESSION).into());
        }

        Ok(())
    }

    /// Returns the number of bytes ready to be output to a frame.
    pub fn ready_bytes(&self) -> usize {
        self.compressed.as_ref().map_or(0, |c| c.len())
    }

    /// Closes the channel if not already closed.
    pub fn close(&mut self) {
        self.closed = true;
    }

    /// Outputs a [Frame] from the [ChannelOut].
    pub fn output_frame(&mut self, max_size: usize) -> Result<Frame, ChannelOutError> {
        if max_size < FRAME_V0_OVERHEAD {
            return Err(ChannelOutError::MaxFrameSizeTooSmall);
        }

        // Construct an empty frame.
        let mut frame =
            Frame { id: self.id, number: self.frame_number, is_last: self.closed, data: vec![] };

        let mut max_size = max_size - FRAME_V0_OVERHEAD;
        if max_size > self.ready_bytes() {
            max_size = self.ready_bytes();
        }

        // Read `max_size` bytes from the compressed data.
        let data = if let Some(data) = &self.compressed {
            &data[..max_size]
        } else {
            return Err(ChannelOutError::MissingData);
        };
        frame.data.extend_from_slice(data);

        // Update the compressed data.
        self.compressed = self.compressed.as_mut().map(|b| b.split_off(max_size));
        self.frame_number += 1;
        Ok(frame)
    }
}

/// [MAX_RLP_BYTES_PER_CHANNEL] is the maximum amount of bytes that will be read from
/// a channel. This limit is set when decoding the RLP.
pub const MAX_RLP_BYTES_PER_CHANNEL: u64 = 10_000_000;

/// [FJORD_MAX_RLP_BYTES_PER_CHANNEL] is the maximum amount of bytes that will be read from
/// a channel when the Fjord Hardfork is activated. This limit is set when decoding the RLP.
pub const FJORD_MAX_RLP_BYTES_PER_CHANNEL: u64 = 100_000_000;

/// An error returned when adding a frame to a channel.
#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelError {
    /// The frame id does not match the channel id.
    #[error("Frame id does not match channel id")]
    FrameIdMismatch,
    /// The channel is closed.
    #[error("Channel is closed")]
    ChannelClosed,
    /// The frame number is already in the channel.
    #[error("Frame number {0} already exists")]
    FrameNumberExists(usize),
    /// The frame number is beyond the end frame.
    #[error("Frame number {0} is beyond end frame")]
    FrameBeyondEndFrame(usize),
}

/// A Channel is a set of batches that are split into at least one, but possibly multiple frames.
///
/// Frames are allowed to be ingested out of order.
/// Each frame is ingested one by one. Once a frame with `closed` is added to the channel, the
/// channel may mark itself as ready for reading once all intervening frames have been added
#[derive(Debug, Clone, Default)]
pub struct Channel {
    /// The unique identifier for this channel
    id: ChannelId,
    /// The block that the channel is currently open at
    open_block: BlockInfo,
    /// Estimated memory size, used to drop the channel if we have too much data
    estimated_size: usize,
    /// True if the last frame has been buffered
    closed: bool,
    /// The highest frame number that has been ingested
    highest_frame_number: u16,
    /// The frame number of the frame where `is_last` is true
    /// No other frame number may be higher than this
    last_frame_number: u16,
    /// Store a map of frame number to frame for constant time ordering
    inputs: HashMap<u16, Frame>,
    /// The highest L1 inclusion block that a frame was included in
    highest_l1_inclusion_block: BlockInfo,
}

impl Channel {
    /// Create a new [Channel] with the given [ChannelId] and [BlockInfo].
    pub fn new(id: ChannelId, open_block: BlockInfo) -> Self {
        Self { id, open_block, inputs: HashMap::default(), ..Default::default() }
    }

    /// Returns the current [ChannelId] for the channel.
    pub const fn id(&self) -> ChannelId {
        self.id
    }

    /// Returns the number of frames ingested.
    pub fn len(&self) -> usize {
        self.inputs.len()
    }

    /// Returns if the channel is empty.
    pub fn is_empty(&self) -> bool {
        self.inputs.is_empty()
    }

    /// Add a frame to the channel.
    ///
    /// ## Takes
    /// - `frame`: The frame to add to the channel
    /// - `l1_inclusion_block`: The block that the frame was included in
    ///
    /// ## Returns
    /// - `Ok(()):` If the frame was successfully buffered
    /// - `Err(_):` If the frame was invalid
    pub fn add_frame(
        &mut self,
        frame: Frame,
        l1_inclusion_block: BlockInfo,
    ) -> Result<(), ChannelError> {
        // Ensure that the frame ID is equal to the channel ID.
        if frame.id != self.id {
            return Err(ChannelError::FrameIdMismatch);
        }
        if frame.is_last && self.closed {
            return Err(ChannelError::ChannelClosed);
        }
        if self.inputs.contains_key(&frame.number) {
            return Err(ChannelError::FrameNumberExists(frame.number as usize));
        }
        if self.closed && frame.number >= self.last_frame_number {
            return Err(ChannelError::FrameBeyondEndFrame(frame.number as usize));
        }

        // Guaranteed to succeed at this point. Update the channel state.
        if frame.is_last {
            self.last_frame_number = frame.number;
            self.closed = true;

            // Prune frames with a higher number than the last frame number when we receive a
            // closing frame.
            if self.last_frame_number < self.highest_frame_number {
                self.inputs.retain(|id, frame| {
                    self.estimated_size -= frame.size();
                    *id < self.last_frame_number
                });
                self.highest_frame_number = self.last_frame_number;
            }
        }

        // Update the highest frame number.
        if frame.number > self.highest_frame_number {
            self.highest_frame_number = frame.number;
        }

        if self.highest_l1_inclusion_block.number < l1_inclusion_block.number {
            self.highest_l1_inclusion_block = l1_inclusion_block;
        }

        self.estimated_size += frame.size();
        self.inputs.insert(frame.number, frame);
        Ok(())
    }

    /// Returns the block number of the L1 block that contained the first [Frame] in this channel.
    pub const fn open_block_number(&self) -> u64 {
        self.open_block.number
    }

    /// Returns the estimated size of the channel including [Frame] overhead.
    pub const fn size(&self) -> usize {
        self.estimated_size
    }

    /// Returns `true` if the channel is ready to be read.
    pub fn is_ready(&self) -> bool {
        // Must have buffered the last frame before the channel is ready.
        if !self.closed {
            return false;
        }

        // Must have the possibility of contiguous frames.
        if self.inputs.len() != (self.last_frame_number + 1) as usize {
            return false;
        }

        // Check for contiguous frames.
        for i in 0..=self.last_frame_number {
            if !self.inputs.contains_key(&i) {
                return false;
            }
        }

        true
    }

    /// Returns all of the channel's [Frame]s concatenated together.
    ///
    /// ## Returns
    ///
    /// - `Some(Bytes)`: The concatenated frame data
    /// - `None`: If the channel is missing frames
    pub fn frame_data(&self) -> Option<Bytes> {
        let mut data = Vec::with_capacity(self.size());
        (0..=self.last_frame_number).try_for_each(|i| {
            let frame = self.inputs.get(&i)?;
            data.extend_from_slice(&frame.data);
            Some(())
        })?;
        Some(data.into())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use alloc::{
        string::{String, ToString},
        vec,
    };

    struct FrameValidityTestCase {
        #[allow(dead_code)]
        name: String,
        frames: Vec<Frame>,
        should_error: Vec<bool>,
        sizes: Vec<u64>,
    }

    fn run_frame_validity_test(test_case: FrameValidityTestCase) {
        // #[cfg(feature = "std")]
        // println!("Running test: {}", test_case.name);

        let id = [0xFF; 16];
        let block = BlockInfo::default();
        let mut channel = Channel::new(id, block);

        if test_case.frames.len() != test_case.should_error.len()
            || test_case.frames.len() != test_case.sizes.len()
        {
            panic!("Test case length mismatch");
        }

        for (i, frame) in test_case.frames.iter().enumerate() {
            let result = channel.add_frame(frame.clone(), block);
            if test_case.should_error[i] {
                assert!(result.is_err());
            } else {
                assert!(result.is_ok());
            }
            assert_eq!(channel.size(), test_case.sizes[i] as usize);
        }
    }

    #[test]
    fn test_frame_validity() {
        let id = [0xFF; 16];
        let test_cases = [
            FrameValidityTestCase {
                name: "wrong channel".to_string(),
                frames: vec![Frame { id: [0xEE; 16], ..Default::default() }],
                should_error: vec![true],
                sizes: vec![0],
            },
            FrameValidityTestCase {
                name: "double close".to_string(),
                frames: vec![
                    Frame { id, is_last: true, number: 2, data: b"four".to_vec() },
                    Frame { id, is_last: true, number: 1, ..Default::default() },
                ],
                should_error: vec![false, true],
                sizes: vec![204, 204],
            },
            FrameValidityTestCase {
                name: "duplicate frame".to_string(),
                frames: vec![
                    Frame { id, number: 2, data: b"four".to_vec(), ..Default::default() },
                    Frame { id, number: 2, data: b"seven".to_vec(), ..Default::default() },
                ],
                should_error: vec![false, true],
                sizes: vec![204, 204],
            },
            FrameValidityTestCase {
                name: "duplicate closing frames".to_string(),
                frames: vec![
                    Frame { id, number: 2, is_last: true, data: b"four".to_vec() },
                    Frame { id, number: 2, is_last: true, data: b"seven".to_vec() },
                ],
                should_error: vec![false, true],
                sizes: vec![204, 204],
            },
            FrameValidityTestCase {
                name: "frame past closing".to_string(),
                frames: vec![
                    Frame { id, number: 2, is_last: true, data: b"four".to_vec() },
                    Frame { id, number: 10, data: b"seven".to_vec(), ..Default::default() },
                ],
                should_error: vec![false, true],
                sizes: vec![204, 204],
            },
            FrameValidityTestCase {
                name: "prune after close frame".to_string(),
                frames: vec![
                    Frame { id, number: 10, is_last: false, data: b"seven".to_vec() },
                    Frame { id, number: 2, is_last: true, data: b"four".to_vec() },
                ],
                should_error: vec![false, false],
                sizes: vec![205, 204],
            },
            FrameValidityTestCase {
                name: "multiple valid frames".to_string(),
                frames: vec![
                    Frame { id, number: 10, data: b"seven__".to_vec(), ..Default::default() },
                    Frame { id, number: 2, data: b"four".to_vec(), ..Default::default() },
                ],
                should_error: vec![false, false],
                sizes: vec![207, 411],
            },
        ];

        test_cases.into_iter().for_each(run_frame_validity_test);
    }
}
