//! Stages pertaining to the reading and decoding of channels.
//!
//! Sitting after the [`FrameQueue`] stage, the [`ChannelBank`] and [`ChannelAssembler`] stages are
//! responsible for reading and decoding the [Frame]s into [Channel]s. The [`ChannelReader`] stage
//! is responsible for decoding the [Channel]s into [Batch]es, forwarding the [Batch]es to the
//! [`BatchQueue`] stage.
//!
//! [Frame]: base_protocol::Frame
//! [Channel]: base_protocol::Channel
//! [Batch]: base_protocol::Batch
//! [FrameQueue]: crate::stages::FrameQueue
//! [BatchQueue]: crate::stages::BatchQueue

use alloc::boxed::Box;

use async_trait::async_trait;
use base_protocol::Frame;

use crate::types::PipelineResult;

pub(crate) mod channel_provider;
pub use channel_provider::ChannelProvider;

pub(crate) mod channel_bank;
pub use channel_bank::ChannelBank;

pub(crate) mod channel_assembler;
pub use channel_assembler::ChannelAssembler;

pub(crate) mod channel_reader;
pub use channel_reader::{ChannelReader, ChannelReaderProvider};

/// Provides frames for the [`ChannelBank`] and [`ChannelAssembler`] stages.
#[async_trait]
pub trait NextFrameProvider {
    /// Retrieves the next [`Frame`] from the [`FrameQueue`] stage.
    ///
    /// [`FrameQueue`]: crate::stages::FrameQueue
    async fn next_frame(&mut self) -> PipelineResult<Frame>;
}
