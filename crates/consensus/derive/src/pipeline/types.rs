//! Type aliases for the stages in the derivation pipeline.

use crate::{
    AttributesQueue, BatchStream, BatchValidator, ChannelAssembler, ChannelReader, FrameQueue,
    L1Retrieval, PollingTraversal,
};

/// Type alias for the [`L1Retrieval`] stage.
pub type L1RetrievalStage<DAP, T> = L1Retrieval<DAP, T>;

/// Type alias for the [`FrameQueue`] stage.
pub type FrameQueueStage<DAP, T> = FrameQueue<L1RetrievalStage<DAP, T>>;

/// Type alias for the [`ChannelAssembler`] stage.
pub type ChannelAssemblerStage<DAP, T> = ChannelAssembler<FrameQueueStage<DAP, T>>;

/// Type alias for the [`ChannelReader`] stage.
pub type ChannelReaderStage<DAP, T> = ChannelReader<ChannelAssemblerStage<DAP, T>>;

/// Type alias for the [`BatchStream`] stage.
pub type BatchStreamStage<DAP, T, F> = BatchStream<ChannelReaderStage<DAP, T>, F>;

/// Type alias for the [`BatchValidator`] stage.
pub type BatchValidatorStage<DAP, T, F> = BatchValidator<BatchStreamStage<DAP, T, F>, F>;

/// Type alias for the [`AttributesQueue`] stage.
pub type AttributesQueueStage<DAP, T, F, B> = AttributesQueue<BatchValidatorStage<DAP, T, F>, B>;

/// Type alias for the [`AttributesQueue`] stage that uses a [`PollingTraversal`] stage.
pub type PolledAttributesQueueStage<DAP, P, F, B> =
    AttributesQueueStage<DAP, PollingTraversal<P>, F, B>;
