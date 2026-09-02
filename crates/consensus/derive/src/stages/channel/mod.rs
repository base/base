//! Stages pertaining to the reading and decoding of channels.
//!
//! Sitting after the [`FrameQueue`] stage, the [`ChannelAssembler`] stage is responsible for
//! assembling the [Frame]s into a raw compressed [Channel]. The [`ChannelReader`] stage then
//! decodes the [Channel] into [Batch]es and forwards them to the [`BatchStream`] stage.
//!
//! [Frame]: base_protocol::Frame
//! [Channel]: base_protocol::Channel
//! [Batch]: base_protocol::Batch
//! [FrameQueue]: crate::stages::FrameQueue
//! [BatchStream]: crate::stages::BatchStream

use alloc::boxed::Box;

use async_trait::async_trait;
use base_protocol::Frame;

use crate::types::PipelineResult;

mod channel_assembler;
pub use channel_assembler::ChannelAssembler;

mod channel_reader;
pub use channel_reader::{ChannelReader, ChannelReaderProvider};

/// Provides frames for the [`ChannelAssembler`] stage.
#[async_trait]
pub trait NextFrameProvider {
    /// Retrieves the next [`Frame`] from the [`FrameQueue`] stage.
    ///
    /// [`FrameQueue`]: crate::stages::FrameQueue
    async fn next_frame(&mut self) -> PipelineResult<Frame>;
}
