//! Contains the [`MeteringRpcExtension`] which wires up the metering RPC surface
//! on the Base node builder.

use std::sync::Arc;

use alloy_primitives::U256;
use base_primitives::{BaseNodeExtension, ConfigurableBaseNodeExtension, OpBuilder};
use parking_lot::RwLock;
use tokio::sync::mpsc;
use tracing::info;

use crate::{
    AnnotatorCommand, FlashblockInclusion, MeteringApiImpl, MeteringApiServer, MeteringCache,
    MeteredTransaction, PriorityFeeEstimator, ResourceAnnotator, ResourceLimits,
};

/// Resource limits configuration for priority fee estimation.
#[derive(Debug, Clone, Default)]
pub struct MeteringResourceLimits {
    /// Maximum gas per block.
    pub gas_limit: Option<u64>,
    /// Maximum execution time per block in microseconds.
    pub execution_time_us: Option<u64>,
    /// Maximum state root computation time per block in microseconds.
    pub state_root_time_us: Option<u64>,
    /// Maximum data availability bytes per block.
    pub da_bytes: Option<u64>,
}

impl MeteringResourceLimits {
    /// Converts to the internal [`ResourceLimits`] type.
    pub fn to_resource_limits(&self) -> ResourceLimits {
        ResourceLimits {
            gas_used: self.gas_limit,
            execution_time_us: self.execution_time_us.map(|v| v as u128),
            state_root_time_us: self.state_root_time_us.map(|v| v as u128),
            data_availability_bytes: self.da_bytes,
        }
    }
}

/// Helper struct that wires the metering RPC into the node builder.
#[derive(Debug, Clone)]
pub struct MeteringRpcExtension {
    /// Whether metering is enabled.
    pub enabled: bool,
    /// Resource limits for priority fee estimation.
    pub resource_limits: MeteringResourceLimits,
    /// Percentile for priority fee estimation (e.g., 0.5 for median).
    pub priority_fee_percentile: f64,
    /// Default priority fee when resources are uncongested (in wei).
    pub uncongested_priority_fee: u64,
    /// Number of blocks to retain in the metering cache.
    pub cache_size: usize,
}

impl Default for MeteringRpcExtension {
    fn default() -> Self {
        Self {
            enabled: false,
            resource_limits: MeteringResourceLimits::default(),
            priority_fee_percentile: 0.5,
            uncongested_priority_fee: 1_000_000, // 1 gwei default
            cache_size: 12,
        }
    }
}

impl MeteringRpcExtension {
    /// Creates a new metering RPC extension.
    pub const fn new(enabled: bool) -> Self {
        Self {
            enabled,
            resource_limits: MeteringResourceLimits {
                gas_limit: None,
                execution_time_us: None,
                state_root_time_us: None,
                da_bytes: None,
            },
            priority_fee_percentile: 0.5,
            uncongested_priority_fee: 1_000_000,
            cache_size: 12,
        }
    }

    /// Sets the resource limits.
    pub fn with_resource_limits(mut self, limits: MeteringResourceLimits) -> Self {
        self.resource_limits = limits;
        self
    }

    /// Sets the priority fee percentile.
    pub const fn with_percentile(mut self, percentile: f64) -> Self {
        self.priority_fee_percentile = percentile;
        self
    }

    /// Sets the uncongested priority fee.
    pub const fn with_uncongested_fee(mut self, fee: u64) -> Self {
        self.uncongested_priority_fee = fee;
        self
    }

    /// Sets the cache size.
    pub const fn with_cache_size(mut self, size: usize) -> Self {
        self.cache_size = size;
        self
    }

    /// Returns true if priority fee estimation is configured (has resource limits).
    fn has_estimator_config(&self) -> bool {
        self.resource_limits.gas_limit.is_some()
            || self.resource_limits.execution_time_us.is_some()
            || self.resource_limits.da_bytes.is_some()
    }
}

impl BaseNodeExtension for MeteringRpcExtension {
    /// Applies the extension to the supplied builder.
    fn apply(self: Box<Self>, builder: OpBuilder) -> OpBuilder {
        if !self.enabled {
            return builder;
        }

        let has_estimator = self.has_estimator_config();
        let resource_limits = self.resource_limits.to_resource_limits();
        let percentile = self.priority_fee_percentile;
        let default_fee = U256::from(self.uncongested_priority_fee);
        let cache_size = self.cache_size;

        builder.extend_rpc_modules(move |ctx| {
            let metering_api = if has_estimator {
                info!(
                    message = "Starting Metering RPC with priority fee estimation",
                    cache_size = cache_size,
                    percentile = percentile,
                );

                let cache = Arc::new(RwLock::new(MeteringCache::new(cache_size)));
                let estimator =
                    Arc::new(PriorityFeeEstimator::new(cache.clone(), percentile, resource_limits, default_fee));

                // Create channels for the annotator
                let (tx_sender, tx_receiver) = mpsc::unbounded_channel::<MeteredTransaction>();
                let (flashblock_sender, flashblock_receiver) =
                    mpsc::unbounded_channel::<FlashblockInclusion>();
                let (cmd_sender, cmd_receiver) = mpsc::unbounded_channel::<AnnotatorCommand>();

                // Spawn the annotator
                let annotator = ResourceAnnotator::new(
                    cache.clone(),
                    tx_receiver,
                    flashblock_receiver,
                    cmd_receiver,
                );
                tokio::spawn(annotator.run());

                // Note: flashblock_sender would need to be connected to the flashblocks feed
                // For now, it's created but not used - the ingestion flow will be completed
                // in a future PR that integrates with the flashblocks websocket.
                drop(flashblock_sender);

                MeteringApiImpl::with_estimator(
                    ctx.provider().clone(),
                    estimator,
                    tx_sender,
                    cmd_sender,
                )
            } else {
                info!(message = "Starting Metering RPC (priority fee estimation disabled)");
                MeteringApiImpl::new(ctx.provider().clone())
            };

            ctx.modules.merge_configured(metering_api.into_rpc())?;
            Ok(())
        })
    }
}

/// Configuration trait for [`MeteringRpcExtension`].
///
/// Types implementing this trait can be used to construct a [`MeteringRpcExtension`].
pub trait MeteringRpcConfig {
    /// Returns whether metering is enabled.
    fn metering_enabled(&self) -> bool;

    /// Returns the resource limits configuration.
    fn resource_limits(&self) -> MeteringResourceLimits {
        MeteringResourceLimits::default()
    }

    /// Returns the priority fee percentile.
    fn priority_fee_percentile(&self) -> f64 {
        0.5
    }

    /// Returns the uncongested priority fee in wei.
    fn uncongested_priority_fee(&self) -> u64 {
        1_000_000
    }

    /// Returns the cache size.
    fn cache_size(&self) -> usize {
        12
    }
}

impl<C: MeteringRpcConfig> ConfigurableBaseNodeExtension<C> for MeteringRpcExtension {
    fn build(config: &C) -> eyre::Result<Self> {
        Ok(Self {
            enabled: config.metering_enabled(),
            resource_limits: config.resource_limits(),
            priority_fee_percentile: config.priority_fee_percentile(),
            uncongested_priority_fee: config.uncongested_priority_fee(),
            cache_size: config.cache_size(),
        })
    }
}
