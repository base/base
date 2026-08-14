//! Additional configuration for the Base payload builder.

use std::{
    path::Path,
    sync::{Arc, atomic::AtomicU64},
};

use crate::{
    CompiledResourceMeteringSchedule, NoopMeteringProvider, ResourceMeteringError,
    ResourceMeteringSchedule, ResourceThrottlingMode, SharedMeteringProvider,
};

/// Settings for the Base payload builder.
#[derive(Debug, Clone)]
pub struct BaseBuilderConfig {
    /// Data availability configuration for the Base payload builder.
    pub da_config: BaseDAConfig,
    /// Gas limit configuration for the Base payload builder.
    pub gas_limit_config: GasLimitConfig,
    /// Whether to drop positively stale EIP-8130 transactions using their
    /// captured authorization manifest before execution.
    pub manifest_precheck_enabled: bool,
    /// Resource metering and throttling configuration for native payload admission.
    pub resource_metering: ResourceMeteringConfig,
}

impl Default for BaseBuilderConfig {
    fn default() -> Self {
        Self {
            da_config: BaseDAConfig::default(),
            gas_limit_config: GasLimitConfig::default(),
            manifest_precheck_enabled: true,
            resource_metering: ResourceMeteringConfig::default(),
        }
    }
}

impl BaseBuilderConfig {
    /// Creates a new Base payload builder configuration.
    pub fn new(
        da_config: BaseDAConfig,
        gas_limit_config: GasLimitConfig,
        manifest_precheck_enabled: bool,
    ) -> Self {
        Self {
            da_config,
            gas_limit_config,
            manifest_precheck_enabled,
            resource_metering: ResourceMeteringConfig::default(),
        }
    }

    /// Sets resource metering and throttling for native payload admission.
    pub fn with_resource_metering(mut self, resource_metering: ResourceMeteringConfig) -> Self {
        self.resource_metering = resource_metering;
        self
    }

    /// Returns the data availability configuration for the Base payload builder, if it has
    /// configured
    /// constraints.
    pub fn constrained_da_config(&self) -> Option<&BaseDAConfig> {
        if self.da_config.is_empty() { None } else { Some(&self.da_config) }
    }
}

/// Native payload resource metering and throttling.
#[derive(Clone)]
pub struct ResourceMeteringConfig {
    /// Whether metered usage is ignored, observed, or used to throttle selection.
    pub throttling_mode: ResourceThrottlingMode,
    /// Compiled startup schedule.
    pub schedule: Arc<CompiledResourceMeteringSchedule>,
    /// `meterBundle` results used to evaluate the schedule.
    pub provider: SharedMeteringProvider,
}

impl std::fmt::Debug for ResourceMeteringConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceMeteringConfig")
            .field("throttling_mode", &self.throttling_mode)
            .field("schedule_empty", &self.schedule.is_empty())
            .field("provider_enabled", &crate::MeteringProvider::is_enabled(self.provider.as_ref()))
            .finish()
    }
}

impl Default for ResourceMeteringConfig {
    fn default() -> Self {
        Self {
            throttling_mode: ResourceThrottlingMode::Off,
            schedule: Arc::new(
                CompiledResourceMeteringSchedule::compile(ResourceMeteringSchedule::default())
                    .expect("the default resource metering schedule is valid"),
            ),
            provider: Arc::new(NoopMeteringProvider),
        }
    }
}

impl ResourceMeteringConfig {
    /// Builds a shared config from startup flags.
    pub fn from_parts(
        throttling_mode: ResourceThrottlingMode,
        schedule_path: Option<&Path>,
        provider: SharedMeteringProvider,
    ) -> Result<Self, ResourceMeteringError> {
        let schedule = match schedule_path {
            Some(path) => CompiledResourceMeteringSchedule::compile(
                ResourceMeteringSchedule::from_file(path)?,
            )?,
            None => CompiledResourceMeteringSchedule::compile(ResourceMeteringSchedule::default())?,
        };
        Ok(Self { throttling_mode, schedule: Arc::new(schedule), provider })
    }

    /// Builds a config that uses a no-op provider whenever `throttling_mode` is active.
    pub fn enabled(throttling_mode: ResourceThrottlingMode) -> Self {
        Self { throttling_mode, ..Self::default() }
    }
}

/// Contains the data availability configuration for the Base payload builder.
///
/// This type is shareable and can be used to update the DA configuration for the Base payload
/// builder.
#[derive(Debug, Clone, Default)]
pub struct BaseDAConfig {
    inner: Arc<BaseDAConfigInner>,
}

impl BaseDAConfig {
    /// Creates a new Data Availability configuration with the given maximum sizes.
    pub fn new(max_da_tx_size: u64, max_da_block_size: u64) -> Self {
        let this = Self::default();
        this.set_max_da_size(max_da_tx_size, max_da_block_size);
        this
    }

    /// Returns whether the configuration is empty.
    pub fn is_empty(&self) -> bool {
        self.max_da_tx_size().is_none() && self.max_da_block_size().is_none()
    }

    /// Returns the maximum allowed data availability size per transaction, if any.
    pub fn max_da_tx_size(&self) -> Option<u64> {
        let val = self.inner.max_da_tx_size.load(std::sync::atomic::Ordering::Relaxed);
        if val == 0 { None } else { Some(val) }
    }

    /// Returns the max allowed data availability size per block, if any.
    pub fn max_da_block_size(&self) -> Option<u64> {
        let val = self.inner.max_da_block_size.load(std::sync::atomic::Ordering::Relaxed);
        if val == 0 { None } else { Some(val) }
    }

    /// Sets the maximum data availability size currently allowed for inclusion. 0 means no maximum.
    pub fn set_max_da_size(&self, max_da_tx_size: u64, max_da_block_size: u64) {
        self.set_max_tx_size(max_da_tx_size);
        self.set_max_block_size(max_da_block_size);
    }

    /// Sets the maximum data availability size per transaction currently allowed for inclusion. 0
    /// means no maximum.
    pub fn set_max_tx_size(&self, max_da_tx_size: u64) {
        self.inner.max_da_tx_size.store(max_da_tx_size, std::sync::atomic::Ordering::Relaxed);
    }

    /// Sets the maximum data availability size per block currently allowed for inclusion. 0 means
    /// no maximum.
    pub fn set_max_block_size(&self, max_da_block_size: u64) {
        self.inner.max_da_block_size.store(max_da_block_size, std::sync::atomic::Ordering::Relaxed);
    }
}

#[derive(Debug, Default)]
struct BaseDAConfigInner {
    /// Don't include any transactions with data availability size larger than this in any built
    /// block
    ///
    /// 0 means no limit.
    max_da_tx_size: AtomicU64,
    /// Maximum total data availability size for a block
    ///
    /// 0 means no limit.
    max_da_block_size: AtomicU64,
}

/// Contains the gas-limit configuration for the Base payload builder.
///
/// This type is shareable and can be used to update the gas-limit configuration for the Base
/// payload
/// builder.
#[derive(Debug, Clone, Default)]
pub struct GasLimitConfig {
    /// Gas limit for a transaction
    ///
    /// 0 means use the default gas limit.
    gas_limit: Arc<AtomicU64>,
}

impl GasLimitConfig {
    /// Creates a new Gas Limit configuration with the given maximum gas limit.
    pub fn new(max_gas_limit: u64) -> Self {
        let this = Self::default();
        this.set_gas_limit(max_gas_limit);
        this
    }
    /// Returns the gas limit for a transaction, if any.
    pub fn gas_limit(&self) -> Option<u64> {
        let val = self.gas_limit.load(std::sync::atomic::Ordering::Relaxed);
        if val == 0 { None } else { Some(val) }
    }
    /// Sets the gas limit for a transaction. 0 means use the default gas limit.
    pub fn set_gas_limit(&self, gas_limit: u64) {
        self.gas_limit.store(gas_limit, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_da() {
        let da = BaseDAConfig::default();
        assert_eq!(da.max_da_tx_size(), None);
        assert_eq!(da.max_da_block_size(), None);
        da.set_max_da_size(100, 200);
        assert_eq!(da.max_da_tx_size(), Some(100));
        assert_eq!(da.max_da_block_size(), Some(200));
        da.set_max_da_size(0, 0);
        assert_eq!(da.max_da_tx_size(), None);
        assert_eq!(da.max_da_block_size(), None);
    }

    #[test]
    fn test_da_constrained() {
        let config = BaseBuilderConfig::default();
        assert!(config.constrained_da_config().is_none());
    }

    #[test]
    fn new_preserves_manifest_precheck_setting() {
        let config =
            BaseBuilderConfig::new(BaseDAConfig::default(), GasLimitConfig::default(), false);
        assert!(!config.manifest_precheck_enabled);
    }

    #[test]
    fn test_gas_limit() {
        let gas_limit = GasLimitConfig::default();
        assert_eq!(gas_limit.gas_limit(), None);
        gas_limit.set_gas_limit(50000);
        assert_eq!(gas_limit.gas_limit(), Some(50000));
        gas_limit.set_gas_limit(0);
        assert_eq!(gas_limit.gas_limit(), None);
    }
}
