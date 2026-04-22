//! Contains the core derivation pipeline.

use alloc::{boxed::Box, collections::VecDeque, string::ToString, sync::Arc};
use core::fmt::Debug;

use alloy_eips::BlockNumHash;
use async_trait::async_trait;
use base_common_genesis::{RollupConfig, SystemConfig};
use base_protocol::{AttributesWithParent, BatchValidationProvider, BlockInfo, L2BlockInfo};

use crate::{
    ActivationSignal, L2ChainProvider, Metrics, NextAttributes, OriginAdvancer, OriginProvider,
    Pipeline, PipelineError, PipelineErrorKind, PipelineResult, ResetSignal, Signal,
    SignalReceiver, StageReset, StepResult,
};

/// The derivation pipeline is responsible for deriving L2 inputs from L1 data.
#[derive(Debug)]
pub struct DerivationPipeline<S, P>
where
    S: NextAttributes + StageReset + OriginProvider + OriginAdvancer + Debug + Send,
    P: L2ChainProvider + Send + Sync + Debug,
{
    /// A handle to the next attributes.
    pub attributes: S,
    /// Reset provider for the pipeline.
    /// A list of prepared [`AttributesWithParent`] to be used by the derivation pipeline
    /// consumer.
    pub prepared: VecDeque<AttributesWithParent>,
    /// The rollup config.
    pub rollup_config: Arc<RollupConfig>,
    /// The L2 Chain Provider used to fetch the system config on reset.
    pub l2_chain_provider: P,
}

impl<S, P> DerivationPipeline<S, P>
where
    S: NextAttributes + StageReset + OriginProvider + OriginAdvancer + Debug + Send,
    P: L2ChainProvider + Send + Sync + Debug,
{
    /// Creates a new instance of the [`DerivationPipeline`].
    pub const fn new(
        attributes: S,
        rollup_config: Arc<RollupConfig>,
        l2_chain_provider: P,
    ) -> Self {
        Self { attributes, prepared: VecDeque::new(), rollup_config, l2_chain_provider }
    }

    /// Walks back the L2 chain from `l2_safe_head` until the walked-back block's L1 origin
    /// is at most `channel_timeout` L1 blocks behind the safe head's L1 origin, then returns
    /// that block's L1 origin and system config.
    ///
    /// This matches the reference node's `initialReset` behavior: using the system config from a
    /// potentially older L2 block ensures we see any batcher-address changes that could
    /// affect channels still open within the channel timeout window.
    async fn initial_reset(
        &mut self,
        l2_safe_head: L2BlockInfo,
    ) -> PipelineResult<(BlockNumHash, SystemConfig)>
    where
        <P as BatchValidationProvider>::Error: Into<PipelineErrorKind>,
    {
        let channel_timeout = self.rollup_config.channel_timeout(l2_safe_head.block_info.timestamp);
        let l1_origin_number = l2_safe_head.l1_origin.number;
        let mut current = l2_safe_head;

        loop {
            if current.block_info.number == self.rollup_config.genesis.l2.number {
                break;
            }
            if current.l1_origin.number + channel_timeout <= l1_origin_number {
                break;
            }
            current = self
                .l2_chain_provider
                .l2_block_info_by_number(current.block_info.number - 1)
                .await
                .map_err(Into::into)?;
        }

        let system_config = self
            .l2_chain_provider
            .system_config_by_number(current.block_info.number, Arc::clone(&self.rollup_config))
            .await
            .map_err(Into::into)?;

        Ok((current.l1_origin, system_config))
    }
}

impl<S, P> OriginProvider for DerivationPipeline<S, P>
where
    S: NextAttributes + StageReset + OriginProvider + OriginAdvancer + Debug + Send,
    P: L2ChainProvider + Send + Sync + Debug,
{
    fn origin(&self) -> Option<BlockInfo> {
        self.attributes.origin()
    }
}

impl<S, P> Iterator for DerivationPipeline<S, P>
where
    S: NextAttributes + StageReset + OriginProvider + OriginAdvancer + Debug + Send + Sync,
    P: L2ChainProvider + Send + Sync + Debug,
{
    type Item = AttributesWithParent;

    fn next(&mut self) -> Option<Self::Item> {
        Metrics::pipeline_payload_attributes_buffer()
            .set(self.prepared.len().saturating_sub(1) as f64);
        self.prepared.pop_front()
    }
}

#[async_trait]
impl<S, P> SignalReceiver for DerivationPipeline<S, P>
where
    S: NextAttributes + StageReset + OriginProvider + OriginAdvancer + Debug + Send + Sync,
    P: L2ChainProvider + Send + Sync + Debug,
    <P as BatchValidationProvider>::Error: Into<PipelineErrorKind>,
{
    /// Signals the pipeline by dispatching typed [`StageReset`] methods to the stage chain.
    ///
    /// During a [`Signal::Reset`], [`initial_reset`] walks back the L2 chain to find the
    /// correct L1 origin and system config before propagating the reset downward. This fixes
    /// a bug where the pipeline used the system config from exactly the safe head block,
    /// missing batcher-address changes within the channel timeout window.
    ///
    /// [`Signal::Activation`] performs a soft reset (clears buffers, preserves origin/config).
    ///
    /// [`initial_reset`]: Self::initial_reset
    async fn signal(&mut self, signal: Signal) -> PipelineResult<()> {
        match signal {
            Signal::Reset(ResetSignal { l2_safe_head }) => {
                let (l1_origin, system_config) = self.initial_reset(l2_safe_head).await?;
                match self.attributes.reset(l1_origin, system_config).await {
                    Ok(()) => trace!(target: "pipeline", "Stages reset"),
                    Err(err) => {
                        if let PipelineErrorKind::Temporary(PipelineError::Eof) = err {
                            trace!(target: "pipeline", "Stages reset with EOF");
                        } else {
                            error!(target: "pipeline", error = ?err, "Stage reset errored");
                            return Err(err);
                        }
                    }
                }
            }
            Signal::Activation(ActivationSignal { l2_safe_head: _ }) => {
                match self.attributes.activate().await {
                    Ok(()) => trace!(target: "pipeline", "Stages activated"),
                    Err(err) => {
                        if let PipelineErrorKind::Temporary(PipelineError::Eof) = err {
                            trace!(target: "pipeline", "Stages activated with EOF");
                        } else {
                            error!(target: "pipeline", error = ?err, "Stage activation errored");
                            return Err(err);
                        }
                    }
                }
            }
            Signal::FlushChannel => {
                self.attributes.flush_channel().await?;
            }
        }
        Metrics::pipeline_signals(signal.to_string()).increment(1.0);
        Ok(())
    }
}

#[async_trait]
impl<S, P> Pipeline for DerivationPipeline<S, P>
where
    S: NextAttributes + StageReset + OriginProvider + OriginAdvancer + Debug + Send + Sync,
    P: L2ChainProvider + Send + Sync + Debug,
{
    /// Peeks at the next prepared [`AttributesWithParent`] from the pipeline.
    fn peek(&self) -> Option<&AttributesWithParent> {
        self.prepared.front()
    }

    /// Returns the rollup config.
    fn rollup_config(&self) -> &RollupConfig {
        &self.rollup_config
    }

    /// Returns the [`SystemConfig`] by L2 number.
    async fn system_config_by_number(
        &mut self,
        number: u64,
    ) -> Result<SystemConfig, PipelineErrorKind> {
        self.l2_chain_provider
            .system_config_by_number(number, Arc::clone(&self.rollup_config))
            .await
            .map_err(Into::into)
    }

    /// Attempts to progress the pipeline.
    ///
    /// ## Returns
    ///
    /// A [`PipelineError::Eof`] is returned if the pipeline is blocked by waiting for new L1 data.
    /// Any other error is critical and the derivation pipeline should be reset.
    /// An error is expected when the underlying source closes.
    ///
    /// When [`DerivationPipeline::step`] returns [Ok(())], it should be called again, to continue the
    /// derivation process.
    ///
    /// [`PipelineError`]: crate::errors::PipelineError
    async fn step(&mut self, cursor: L2BlockInfo) -> StepResult {
        Metrics::pipeline_steps().increment(1.0);
        Metrics::pipeline_step_block().set(cursor.block_info.number as f64);
        match self.attributes.next_attributes(cursor).await {
            Ok(a) => {
                trace!(target: "pipeline", attributes = ?a, "Prepared L2 attributes");
                Metrics::pipeline_payload_attributes_buffer().increment(1.0);
                Metrics::pipeline_latest_payload_tx_count()
                    .set(a.attributes.transactions.as_ref().map_or(0.0, |txs| txs.len() as f64));
                if !a.is_last_in_span {
                    Metrics::pipeline_derived_span_size().increment(1.0);
                } else {
                    Metrics::pipeline_derived_span_size().set(0);
                }
                self.prepared.push_back(a);
                Metrics::pipeline_prepared_attributes().increment(1.0);
                StepResult::PreparedAttributes
            }
            Err(err) => match err {
                PipelineErrorKind::Temporary(PipelineError::Eof) => {
                    trace!(target: "pipeline", "Pipeline advancing origin");
                    if let Err(e) = self.attributes.advance_origin().await {
                        return StepResult::OriginAdvanceErr(e);
                    }
                    StepResult::AdvancedOrigin
                }
                PipelineErrorKind::Temporary(_) => {
                    trace!(target: "pipeline", error = ?err, "Attributes queue step failed due to temporary error");
                    StepResult::StepFailed(err)
                }
                _ => {
                    warn!(target: "pipeline", error = ?err, "Attributes queue step failed");
                    StepResult::StepFailed(err)
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::{string::ToString, sync::Arc};

    use alloy_eips::BlockNumHash;
    use alloy_rpc_types_engine::PayloadAttributes;
    use base_common_genesis::{RollupConfig, SystemConfig};
    use base_common_rpc_types_engine::BasePayloadAttributes;
    use base_protocol::{AttributesWithParent, L2BlockInfo};

    use super::*;
    use crate::{
        DerivationPipeline,
        test_utils::{TestL2ChainProvider, TestNextAttributes, new_test_pipeline},
    };

    fn default_test_payload_attributes() -> AttributesWithParent {
        AttributesWithParent {
            attributes: BasePayloadAttributes {
                payload_attributes: PayloadAttributes {
                    timestamp: 0,
                    prev_randao: Default::default(),
                    suggested_fee_recipient: Default::default(),
                    withdrawals: None,
                    parent_beacon_block_root: None,
                },
                transactions: None,
                no_tx_pool: None,
                gas_limit: None,
                eip_1559_params: None,
                min_base_fee: None,
            },
            parent: Default::default(),
            derived_from: Default::default(),
            is_last_in_span: false,
        }
    }

    #[test]
    fn test_pipeline_next_attributes_empty() {
        let mut pipeline = new_test_pipeline();
        let result = pipeline.next();
        assert_eq!(result, None);
    }

    #[test]
    fn test_pipeline_next_attributes_with_peek() {
        let mut pipeline = new_test_pipeline();
        let expected = default_test_payload_attributes();
        pipeline.prepared.push_back(expected.clone());

        let result = pipeline.peek();
        assert_eq!(result, Some(&expected));

        let result = pipeline.next();
        assert_eq!(result, Some(expected));
    }

    #[tokio::test]
    async fn test_derivation_pipeline_missing_block() {
        let mut pipeline = new_test_pipeline();
        let cursor = L2BlockInfo::default();
        let result = pipeline.step(cursor).await;
        assert_eq!(
            result,
            StepResult::OriginAdvanceErr(
                PipelineError::Provider("Block not found".to_string()).temp()
            )
        );
    }

    #[tokio::test]
    async fn test_derivation_pipeline_prepared_attributes() {
        let rollup_config = Arc::new(RollupConfig::default());
        let l2_chain_provider = TestL2ChainProvider::default();
        let expected = default_test_payload_attributes();
        let attributes = TestNextAttributes { next_attributes: Some(expected) };
        let mut pipeline = DerivationPipeline::new(attributes, rollup_config, l2_chain_provider);

        // Step on the pipeline and expect the result.
        let cursor = L2BlockInfo::default();
        let result = pipeline.step(cursor).await;
        assert_eq!(result, StepResult::PreparedAttributes);
    }

    #[tokio::test]
    async fn test_derivation_pipeline_advance_origin() {
        let rollup_config = Arc::new(RollupConfig::default());
        let l2_chain_provider = TestL2ChainProvider::default();
        let attributes = TestNextAttributes::default();
        let mut pipeline = DerivationPipeline::new(attributes, rollup_config, l2_chain_provider);

        // Step on the pipeline and expect the result.
        let cursor = L2BlockInfo::default();
        let result = pipeline.step(cursor).await;
        assert_eq!(result, StepResult::AdvancedOrigin);
    }

    #[tokio::test]
    async fn test_derivation_pipeline_signal_activation() {
        let rollup_config = Arc::new(RollupConfig::default());
        let mut l2_chain_provider = TestL2ChainProvider::default();
        l2_chain_provider.system_configs.insert(0, SystemConfig::default());
        let attributes = TestNextAttributes::default();
        let mut pipeline = DerivationPipeline::new(attributes, rollup_config, l2_chain_provider);

        // Signal the pipeline to activate.
        let result = pipeline.signal(ActivationSignal::default().signal()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_derivation_pipeline_flush_channel() {
        let rollup_config = Arc::new(RollupConfig::default());
        let l2_chain_provider = TestL2ChainProvider::default();
        let attributes = TestNextAttributes::default();
        let mut pipeline = DerivationPipeline::new(attributes, rollup_config, l2_chain_provider);

        // Signal the pipeline to flush channel.
        let result = pipeline.signal(Signal::FlushChannel).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_derivation_pipeline_signal_reset_missing_sys_config() {
        let rollup_config = Arc::new(RollupConfig::default());
        let l2_chain_provider = TestL2ChainProvider::default();
        let attributes = TestNextAttributes::default();
        let mut pipeline = DerivationPipeline::new(attributes, rollup_config, l2_chain_provider);

        // Signal the pipeline to reset — fails because system config is not found.
        let result = pipeline.signal(ResetSignal::default().signal()).await.unwrap_err();
        assert_eq!(result, PipelineError::Provider("System config not found".to_string()).temp());
    }

    #[tokio::test]
    async fn test_derivation_pipeline_signal_reset_ok() {
        let rollup_config = Arc::new(RollupConfig::default());
        let mut l2_chain_provider = TestL2ChainProvider::default();
        l2_chain_provider.system_configs.insert(0, SystemConfig::default());
        let attributes = TestNextAttributes::default();
        let mut pipeline = DerivationPipeline::new(attributes, rollup_config, l2_chain_provider);

        // Signal the pipeline to reset.
        let result = pipeline.signal(ResetSignal::default().signal()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_derivation_pipeline_initial_reset_walks_back() {
        let rollup_config = Arc::new(RollupConfig {
            // channel_timeout = 100 so the walk-back will stop at genesis (block 0)
            ..Default::default()
        });
        let mut l2_chain_provider = TestL2ChainProvider::default();
        l2_chain_provider.system_configs.insert(0, SystemConfig::default());
        let attributes = TestNextAttributes::default();
        let mut pipeline = DerivationPipeline::new(attributes, rollup_config, l2_chain_provider);

        // With L2 safe head at genesis (block 0), initial_reset should stop at genesis.
        let l2_safe_head = L2BlockInfo {
            l1_origin: BlockNumHash { number: 5, hash: Default::default() },
            ..Default::default()
        };
        let result = pipeline.initial_reset(l2_safe_head).await;
        assert!(result.is_ok());
    }
}
