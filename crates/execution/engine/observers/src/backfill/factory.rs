use std::{ops::RangeInclusive, time::Duration};

use alloy_primitives::BlockNumber;
use base_execution_evm_blocks::BaseEvmConfig;
use base_execution_state_types::{ExecutionStageThresholds, PruneModes};

use super::stream::DEFAULT_PARALLELISM;
use crate::BackfillJob;

/// Factory for creating new backfill jobs.
#[derive(Debug, Clone)]
pub struct BackfillJobFactory<P> {
    evm_config: BaseEvmConfig,
    provider: P,
    prune_modes: PruneModes,
    thresholds: ExecutionStageThresholds,
    stream_parallelism: usize,
}

impl<P> BackfillJobFactory<P> {
    /// Creates a new [`BackfillJobFactory`].
    pub fn new(evm_config: BaseEvmConfig, provider: P) -> Self {
        Self {
            evm_config,
            provider,
            prune_modes: PruneModes::default(),
            thresholds: ExecutionStageThresholds {
                // Default duration for a database transaction to be considered long-lived is
                // 60 seconds, so we limit the backfill job to the half of it to be sure we finish
                // before the warning is logged.
                //
                // See `base_execution_state_database::implementation::mdbx::tx::LONG_TRANSACTION_DURATION`.
                max_duration: Some(Duration::from_secs(30)),
                ..Default::default()
            },
            stream_parallelism: DEFAULT_PARALLELISM,
        }
    }

    /// Sets the prune modes
    pub fn with_prune_modes(mut self, prune_modes: PruneModes) -> Self {
        self.prune_modes = prune_modes;
        self
    }

    /// Sets the thresholds
    pub const fn with_thresholds(mut self, thresholds: ExecutionStageThresholds) -> Self {
        self.thresholds = thresholds;
        self
    }

    /// Sets the stream parallelism.
    ///
    /// Configures the [`StreamBackfillJob`](super::stream::StreamBackfillJob) created via
    /// [`BackfillJob::into_stream`].
    pub const fn with_stream_parallelism(mut self, stream_parallelism: usize) -> Self {
        self.stream_parallelism = stream_parallelism;
        self
    }
}

impl<P: Clone> BackfillJobFactory<P> {
    /// Creates a new backfill job for the given range.
    pub fn backfill(&self, range: RangeInclusive<BlockNumber>) -> BackfillJob<P> {
        BackfillJob {
            evm_config: self.evm_config.clone(),
            provider: self.provider.clone(),
            prune_modes: self.prune_modes.clone(),
            range,
            thresholds: self.thresholds.clone(),
            stream_parallelism: self.stream_parallelism,
        }
    }
}
