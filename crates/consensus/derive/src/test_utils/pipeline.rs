//! Test Utilities for the [`DerivationPipeline`]
//! as well as its stages and providers.

use alloc::{boxed::Box, sync::Arc};

use alloy_eips::BlockNumHash;
use base_common_genesis::{RollupConfig, SystemConfig};
use base_protocol::{AttributesWithParent, BlockInfo, L2BlockInfo};

// Re-export these types used internally to the test pipeline.
use crate::{
    AttributesQueue, BatchStream, ChannelAssembler, ChannelReader, DerivationPipeline, FrameQueue,
    L1Retrieval, NextAttributes, OriginAdvancer, OriginProvider, PipelineBuilder, PipelineError,
    PollingTraversal, StageReset,
    test_utils::{TestAttributesBuilder, TestDAP},
};
use crate::{
    BatchValidator, PipelineResult,
    test_utils::{TestChainProvider, TestL2ChainProvider},
};

/// A fully custom [`NextAttributes`].
#[derive(Default, Debug, Clone)]
pub struct TestNextAttributes {
    /// The next [`AttributesWithParent`] to return.
    pub next_attributes: Option<AttributesWithParent>,
}

#[async_trait::async_trait]
impl StageReset for TestNextAttributes {
    async fn reset(&mut self, _: BlockNumHash, _: SystemConfig) -> PipelineResult<()> {
        Ok(())
    }

    async fn activate(&mut self) -> PipelineResult<()> {
        Ok(())
    }

    async fn flush_channel(&mut self) -> PipelineResult<()> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl OriginProvider for TestNextAttributes {
    /// Returns the current origin.
    fn origin(&self) -> Option<BlockInfo> {
        Some(BlockInfo::default())
    }
}

#[async_trait::async_trait]
impl OriginAdvancer for TestNextAttributes {
    /// Advances the origin to the given block.
    async fn advance_origin(&mut self) -> PipelineResult<()> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl NextAttributes for TestNextAttributes {
    /// Returns the next valid [`AttributesWithParent`].
    async fn next_attributes(&mut self, _: L2BlockInfo) -> PipelineResult<AttributesWithParent> {
        self.next_attributes.take().ok_or(PipelineError::Eof.temp())
    }
}

/// A [`PollingTraversal`] using test providers and sources.
pub type TestPollingTraversal = PollingTraversal<TestChainProvider>;

/// An [`L1Retrieval`] stage using test providers and sources.
pub type TestL1Retrieval = L1Retrieval<TestDAP, TestPollingTraversal>;

/// A [`FrameQueue`] using test providers and sources.
pub type TestFrameQueue = FrameQueue<TestL1Retrieval>;

/// A [`ChannelAssembler`] using test providers and sources.
pub type TestChannelAssembler = ChannelAssembler<TestFrameQueue>;

/// A [`ChannelReader`] using test providers and sources.
pub type TestChannelReader = ChannelReader<TestChannelAssembler>;

/// A [`BatchStream`] using test providers and sources.
pub type TestBatchStream = BatchStream<TestChannelReader, TestL2ChainProvider>;

/// A [`BatchValidator`] using test providers and sources.
pub type TestBatchValidator = BatchValidator<TestBatchStream, TestL2ChainProvider>;

/// An [`AttributesQueue`] using test providers and sources.
pub type TestAttributesQueue = AttributesQueue<TestBatchValidator, TestAttributesBuilder>;

/// A [`DerivationPipeline`] using test providers and sources.
pub type TestPipeline = DerivationPipeline<TestAttributesQueue, TestL2ChainProvider>;

/// Constructs a [`DerivationPipeline`] using test providers and sources.
pub fn new_test_pipeline() -> TestPipeline {
    PipelineBuilder::new()
        .rollup_config(Arc::new(RollupConfig::default()))
        .origin(BlockInfo::default())
        .dap_source(TestDAP::default())
        .builder(TestAttributesBuilder::default())
        .chain_provider(TestChainProvider::default())
        .l2_chain_provider(TestL2ChainProvider::default())
        .build_polled()
}
