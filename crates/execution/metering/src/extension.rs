//! Contains the [`MeteringExtension`] which wires up the metering RPC surface
//! on the Base node builder.

use std::{num::NonZeroUsize, sync::Arc};

use alloy_primitives::U256;
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use parking_lot::RwLock;
use tracing::info;

use crate::{
    MeteredOpcodes, MeteringApiImpl, MeteringApiServer, MeteringCache, PriorityFeeEstimator,
    ResourceLimits, estimator::assert_valid_percentile,
};

const TARGET_SEGMENTS_PER_BLOCK_NON_ZERO_MSG: &str =
    "target_segments_per_block must be greater than 0";
const CACHE_SIZE_NON_ZERO_MSG: &str = "cache_size must be greater than 0";

/// Resource limits configuration for priority fee estimation.
#[derive(Debug, Clone, Default)]
pub struct MeteringResourceLimits {
    /// Total gas budget for the block.
    pub gas_limit: Option<u64>,
    /// Execution time budget for the block in microseconds.
    pub execution_time_us: Option<u64>,
    /// Total state root computation budget for the block in microseconds.
    pub state_root_time_us: Option<u64>,
    /// Total data-availability byte budget for the block.
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
#[derive(Debug)]
pub struct MeteringExtension {
    /// Whether metering is enabled.
    pub enabled: bool,
    /// Resource limits for priority fee estimation.
    pub resource_limits: MeteringResourceLimits,
    /// Percentile for priority fee estimation (e.g., 0.5 for median).
    pub priority_fee_percentile: f64,
    /// Default priority fee when resources are uncongested (in wei).
    pub uncongested_priority_fee: u64,
    /// Number of blocks to retain in the metering cache.
    ///
    /// Must be greater than zero when set on an estimator-enabled configuration.
    pub cache_size: usize,
    /// Target number of transaction-selection segments budgeted per block.
    ///
    /// Must be greater than zero when set. Defaults to one whole-block segment when priority fee
    /// estimation is enabled without an explicit value.
    pub target_segments_per_block: Option<usize>,
    /// Opcodes and precompiles to track for gas metering.
    pub metered_opcodes: MeteredOpcodes,
}

impl Default for MeteringExtension {
    fn default() -> Self {
        Self {
            enabled: false,
            resource_limits: MeteringResourceLimits::default(),
            priority_fee_percentile: 0.5,
            uncongested_priority_fee: 1_000_000,
            cache_size: 12,
            target_segments_per_block: None,
            metered_opcodes: MeteredOpcodes::default(),
        }
    }
}

impl MeteringExtension {
    /// Creates a new metering extension.
    pub fn new(enabled: bool) -> Self {
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
            target_segments_per_block: None,
            metered_opcodes: MeteredOpcodes::default(),
        }
    }

    /// Sets the resource limits.
    pub const fn with_resource_limits(mut self, limits: MeteringResourceLimits) -> Self {
        self.resource_limits = limits;
        self
    }

    /// Sets the priority fee percentile.
    pub const fn with_percentile(mut self, percentile: f64) -> Self {
        assert_valid_percentile(percentile);
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

    /// Sets the target number of transaction-selection segments budgeted per block.
    pub const fn with_target_segments_per_block(mut self, count: usize) -> Self {
        self.target_segments_per_block = Some(count);
        self
    }

    /// Sets the opcodes and precompiles to track for gas metering.
    pub fn with_metered_opcodes(mut self, opcodes: MeteredOpcodes) -> Self {
        self.metered_opcodes = opcodes;
        self
    }

    /// Returns true if priority fee estimation is configured (has resource limits).
    const fn has_estimator_config(&self) -> bool {
        self.resource_limits.gas_limit.is_some()
            || self.resource_limits.execution_time_us.is_some()
            || self.resource_limits.state_root_time_us.is_some()
            || self.resource_limits.da_bytes.is_some()
    }

    const fn resolved_cache_size(&self) -> usize {
        NonZeroUsize::new(self.cache_size).expect(CACHE_SIZE_NON_ZERO_MSG).get()
    }

    const fn resolved_target_segments_per_block(&self) -> usize {
        match self.target_segments_per_block {
            Some(count) => std::num::NonZeroUsize::new(count)
                .expect(TARGET_SEGMENTS_PER_BLOCK_NON_ZERO_MSG)
                .get(),
            None => 1,
        }
    }
}

impl BaseNodeExtension for MeteringExtension {
    /// Applies the extension to the supplied hooks.
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        if !self.enabled {
            return hooks;
        }

        let has_estimator = self.has_estimator_config();
        let resource_limits = self.resource_limits.to_resource_limits();
        let percentile = self.priority_fee_percentile;
        let default_fee = U256::from(self.uncongested_priority_fee);
        let cache_size = has_estimator.then(|| self.resolved_cache_size());
        let target_segments_per_block =
            has_estimator.then(|| self.resolved_target_segments_per_block());
        let metered_opcodes = Arc::new(self.metered_opcodes);

        hooks.add_rpc_module(move |ctx| {
            let metering_api = if has_estimator {
                let cache_size = cache_size.expect("estimator configuration validated");
                let target_segments_per_block =
                    target_segments_per_block.expect("estimator configuration validated");

                info!(
                    cache_size = cache_size,
                    percentile = percentile,
                    target_segments_per_block = target_segments_per_block,
                    "starting metering RPC with priority fee estimation",
                );

                let max_segments_per_block = target_segments_per_block
                    .checked_add(1)
                    .expect("max_segments_per_block must fit in usize");
                let cache =
                    Arc::new(RwLock::new(MeteringCache::new(cache_size, max_segments_per_block)));
                let estimator = Arc::new(PriorityFeeEstimator::new(
                    Arc::clone(&cache),
                    percentile,
                    resource_limits,
                    default_fee,
                    target_segments_per_block,
                ));

                MeteringApiImpl::with_estimator(
                    ctx.provider().clone(),
                    estimator,
                    Arc::clone(&metered_opcodes),
                )
            } else {
                info!("starting metering RPC with priority fee estimation disabled");
                MeteringApiImpl::new(ctx.provider().clone(), Arc::clone(&metered_opcodes))
            };

            ctx.modules.merge_configured(metering_api.into_rpc())?;

            Ok(())
        })
    }
}

/// Configuration for building a [`MeteringExtension`].
#[derive(Debug)]
pub struct MeteringConfig {
    /// Whether metering is enabled.
    pub enabled: bool,
    /// Resource limits for priority fee estimation.
    pub resource_limits: MeteringResourceLimits,
    /// Percentile for priority fee estimation.
    pub priority_fee_percentile: f64,
    /// Default priority fee when uncongested.
    pub uncongested_priority_fee: u64,
    /// Number of blocks to retain in the metering cache.
    ///
    /// Must be greater than zero when used for priority fee estimation.
    pub cache_size: usize,
    /// Target number of transaction-selection segments budgeted per block.
    ///
    /// Must be greater than zero when set. Defaults to one whole-block segment when priority fee
    /// estimation is enabled without an explicit value.
    pub target_segments_per_block: Option<usize>,
    /// Opcodes and precompiles to track for gas metering.
    pub metered_opcodes: MeteredOpcodes,
}

impl MeteringConfig {
    /// Creates a configuration with metering disabled.
    pub fn disabled() -> Self {
        Self { enabled: false, ..Self::enabled() }
    }

    /// Creates a configuration with metering enabled.
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            resource_limits: MeteringResourceLimits {
                gas_limit: None,
                execution_time_us: None,
                state_root_time_us: None,
                da_bytes: None,
            },
            priority_fee_percentile: 0.5,
            uncongested_priority_fee: 1_000_000,
            cache_size: 12,
            target_segments_per_block: None,
            metered_opcodes: MeteredOpcodes::default(),
        }
    }

    /// Sets the resource limits.
    pub const fn with_resource_limits(mut self, limits: MeteringResourceLimits) -> Self {
        self.resource_limits = limits;
        self
    }

    /// Sets the priority fee percentile.
    pub const fn with_percentile(mut self, percentile: f64) -> Self {
        assert_valid_percentile(percentile);
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

    /// Sets the target number of transaction-selection segments budgeted per block.
    pub const fn with_target_segments_per_block(mut self, count: usize) -> Self {
        self.target_segments_per_block = Some(count);
        self
    }

    /// Sets the opcodes and precompiles to track for gas metering.
    pub fn with_metered_opcodes(mut self, opcodes: MeteredOpcodes) -> Self {
        self.metered_opcodes = opcodes;
        self
    }
}

impl FromExtensionConfig for MeteringExtension {
    type Config = MeteringConfig;

    fn from_config(config: Self::Config) -> Self {
        assert_valid_percentile(config.priority_fee_percentile);
        Self {
            enabled: config.enabled,
            resource_limits: config.resource_limits,
            priority_fee_percentile: config.priority_fee_percentile,
            uncongested_priority_fee: config.uncongested_priority_fee,
            cache_size: config.cache_size,
            target_segments_per_block: config.target_segments_per_block,
            metered_opcodes: config.metered_opcodes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_default_target_segments_when_optional() {
        let extension = MeteringExtension::default();

        assert_eq!(extension.resolved_target_segments_per_block(), 1);
    }

    #[test]
    fn resolves_configured_target_segments() {
        let extension = MeteringExtension::default().with_target_segments_per_block(4);

        assert_eq!(extension.resolved_target_segments_per_block(), 4);
    }

    #[test]
    fn defaults_target_segments_with_accumulating_resource_limits() {
        let extension = MeteringExtension::default().with_resource_limits(MeteringResourceLimits {
            gas_limit: Some(60_000_000),
            execution_time_us: None,
            state_root_time_us: Some(1_000_000),
            da_bytes: Some(1_572_860),
        });

        assert_eq!(extension.resolved_target_segments_per_block(), 1);
    }

    #[test]
    #[should_panic(expected = "target_segments_per_block must be greater than 0")]
    fn zero_target_segments_panics() {
        let extension = MeteringExtension::default().with_target_segments_per_block(0);

        let _ = extension.resolved_target_segments_per_block();
    }

    #[test]
    #[should_panic(expected = "cache_size must be greater than 0")]
    fn zero_cache_size_panics() {
        let extension = MeteringExtension::default().with_cache_size(0);

        let _ = extension.resolved_cache_size();
    }
}
