use std::{fmt::Debug, sync::Arc};

use alloy_genesis::ChainConfig;
use base_action_harness::{
    ActionBlobProvider, ActionL1ChainProvider, ActionL2ChainProvider, SharedL1Chain,
    block_info_from,
};
use base_common_chains::ChainConfig as BaseChainConfig;
use base_common_genesis::RollupConfig;
use base_consensus_derive::{
    EthereumDataSource, Pipeline, PipelineBuilder, PipelineError, PipelineErrorKind,
    StatefulAttributesBuilder, StepResult,
};
use base_protocol::{AttributesWithParent, L2BlockInfo};

use crate::{ActionFixture, ActionFixtureAdapter, FixtureAdapterError};

/// Replays captured derivation fixtures through the production derivation pipeline.
#[derive(Debug, Clone, Copy, Default)]
pub struct DerivationFixtureReplayer;

impl DerivationFixtureReplayer {
    /// Derive payload attributes for every expected L2 block in the fixture.
    pub async fn derive_payloads(
        fixture: &ActionFixture,
    ) -> Result<Vec<AttributesWithParent>, FixtureReplayError> {
        Self::derive_payloads_with_rollup_config(fixture, Self::rollup_config(fixture)?).await
    }

    /// Derive payload attributes using an explicit rollup config.
    pub async fn derive_payloads_with_rollup_config(
        fixture: &ActionFixture,
        rollup_config: RollupConfig,
    ) -> Result<Vec<AttributesWithParent>, FixtureReplayError> {
        let derivation =
            fixture.derivation.as_ref().ok_or(FixtureReplayError::MissingDerivationFixture)?;
        let rollup_config = Arc::new(rollup_config);
        let shared_l1 = ActionFixtureAdapter::shared_l1_chain(fixture)?;
        let l2_provider = Self::seed_l2_provider(fixture, rollup_config.as_ref())?;
        let mut cursor = derivation.safe_head;
        let mut pipeline = Self::build_pipeline(
            &shared_l1,
            l2_provider.clone(),
            Arc::clone(&rollup_config),
            derivation.safe_head,
        )?;
        let mut payloads = Vec::with_capacity(fixture.l2_blocks.len());

        for block in &fixture.l2_blocks {
            let payload = Self::next_payload(&mut pipeline, cursor).await?;
            let base_block = ActionFixtureAdapter::l2_block(block)?;
            cursor = ActionFixtureAdapter::l2_block_info(block, &rollup_config.genesis)?;
            l2_provider.insert_block(cursor);
            l2_provider.insert_base_block(block.header.number, base_block);
            payloads.push(payload);
        }

        Ok(payloads)
    }

    /// Return the rollup config for a fixture's network.
    pub fn rollup_config(fixture: &ActionFixture) -> Result<RollupConfig, FixtureReplayError> {
        let chain_id = match fixture.manifest.network.as_str() {
            "base-mainnet" => 8453,
            "base-sepolia" => 84532,
            other => {
                return Err(FixtureReplayError::UnsupportedNetwork { network: other.to_owned() });
            }
        };
        let mut config = BaseChainConfig::by_chain_id(chain_id)
            .map(BaseChainConfig::rollup_config)
            .ok_or(FixtureReplayError::MissingRollupConfig { chain_id })?;
        if let Some(derivation) = &fixture.derivation {
            config.genesis.system_config = Some(derivation.system_config);
        }
        Ok(config)
    }

    /// Seed an action L2 provider with the safe-head anchor and fixture history.
    pub fn seed_l2_provider(
        fixture: &ActionFixture,
        rollup_config: &RollupConfig,
    ) -> Result<ActionL2ChainProvider, FixtureReplayError> {
        let derivation =
            fixture.derivation.as_ref().ok_or(FixtureReplayError::MissingDerivationFixture)?;
        let provider = ActionL2ChainProvider::default();
        provider.insert_block(derivation.safe_head);
        provider
            .insert_system_config(derivation.safe_head.block_info.number, derivation.system_config);
        for block in &derivation.l2_history {
            let block_info = ActionFixtureAdapter::l2_block_info(block, &rollup_config.genesis)?;
            let base_block = ActionFixtureAdapter::l2_block(block)?;
            provider.insert_block(block_info);
            provider.insert_base_block(block.header.number, base_block);
        }
        Ok(provider)
    }

    /// Build a derivation pipeline over captured in-memory L1 data.
    pub fn build_pipeline(
        shared_l1: &SharedL1Chain,
        l2_provider: ActionL2ChainProvider,
        rollup_config: Arc<RollupConfig>,
        safe_head: L2BlockInfo,
    ) -> Result<impl Pipeline + Debug, FixtureReplayError> {
        let l1_provider = ActionL1ChainProvider::new(shared_l1.clone());
        let blob_provider = ActionBlobProvider::new(shared_l1.clone());
        let dap_source =
            EthereumDataSource::new_from_parts(l1_provider.clone(), blob_provider, &rollup_config);
        let l1_origin = Self::origin_from_shared_l1(shared_l1, safe_head)?;
        let attrs_builder = StatefulAttributesBuilder::new(
            Arc::clone(&rollup_config),
            Arc::new(ChainConfig::default()),
            l2_provider.clone(),
            l1_provider.clone(),
        );

        Ok(PipelineBuilder::new()
            .rollup_config(rollup_config)
            .origin(l1_origin)
            .chain_provider(l1_provider)
            .dap_source(dap_source)
            .l2_chain_provider(l2_provider)
            .builder(attrs_builder)
            .build_polled())
    }

    /// Return the pipeline origin matching the seeded safe head.
    pub fn origin_from_shared_l1(
        shared_l1: &SharedL1Chain,
        safe_head: L2BlockInfo,
    ) -> Result<base_protocol::BlockInfo, FixtureReplayError> {
        if let Some(block) = shared_l1.block_by_hash(safe_head.l1_origin.hash) {
            return Ok(block_info_from(&block));
        }
        Err(FixtureReplayError::MissingL1Origin)
    }

    /// Step a pipeline until the next payload attributes are prepared.
    pub async fn next_payload<P>(
        pipeline: &mut P,
        cursor: L2BlockInfo,
    ) -> Result<AttributesWithParent, FixtureReplayError>
    where
        P: Pipeline + Debug,
    {
        for _ in 0..10_000 {
            match pipeline.step(cursor).await {
                StepResult::PreparedAttributes => {
                    return pipeline.next().ok_or(FixtureReplayError::PreparedPayloadMissing);
                }
                StepResult::AdvancedOrigin
                | StepResult::StepFailed(PipelineErrorKind::Temporary(
                    PipelineError::NotEnoughData | PipelineError::ChannelReaderEmpty,
                )) => {}
                StepResult::OriginAdvanceErr(error) => {
                    return Err(FixtureReplayError::Pipeline {
                        stage: "origin advance",
                        source: Box::new(error),
                    });
                }
                StepResult::StepFailed(error) => {
                    return Err(FixtureReplayError::Pipeline {
                        stage: "step",
                        source: Box::new(error),
                    });
                }
            }
        }
        Err(FixtureReplayError::StepLimit { limit: 10_000 })
    }
}

/// Derivation fixture replay failure.
#[derive(Debug, thiserror::Error)]
pub enum FixtureReplayError {
    /// The fixture does not contain derivation replay state.
    #[error("fixture does not contain derivation replay state")]
    MissingDerivationFixture,
    /// The fixture network is not mapped to a known rollup config.
    #[error("unsupported fixture network: {network}")]
    UnsupportedNetwork {
        /// Fixture network.
        network: String,
    },
    /// The rollup config registry is missing a chain ID.
    #[error("missing rollup config for chain ID {chain_id}")]
    MissingRollupConfig {
        /// L2 chain ID.
        chain_id: u64,
    },
    /// No captured L1 origin block was available.
    #[error("missing captured L1 origin block")]
    MissingL1Origin,
    /// The pipeline reported prepared attributes but did not yield them.
    #[error("pipeline prepared attributes but no payload was available")]
    PreparedPayloadMissing,
    /// The pipeline did not produce payload attributes within the step limit.
    #[error("pipeline did not prepare attributes within {limit} steps")]
    StepLimit {
        /// Step limit.
        limit: u64,
    },
    /// Fixture adapter conversion failed.
    #[error(transparent)]
    Adapter(#[from] FixtureAdapterError),
    /// Pipeline stepping failed.
    #[error("pipeline {stage} failed: {source}")]
    Pipeline {
        /// Pipeline stage label.
        stage: &'static str,
        /// Pipeline error.
        source: Box<PipelineErrorKind>,
    },
}
