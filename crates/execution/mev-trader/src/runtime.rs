//! Default-off Phase A runtime installation and idle ingress.

use std::sync::Arc;

use thiserror::Error;

use crate::{
    DedicatedAnalysisPool, FixturePoolRegistry, GlobalLifecycle, LatestSlot, LifecycleError,
    RegistryError, RegistryHasher, SoleWorker, VictimFrame, Watchdog, WorkerClaim,
};

/// Exact telemetry marker for the intentionally idle A0 measurement runtime.
pub const A0_IDLE_STATUS: &str = "A0_MEASUREMENT_IDLE_NO_LIVE_INGRESS";

/// Failures while constructing the empty, provider-free Phase A runtime.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeInstallError {
    /// Empty registry digest construction or validation failed.
    #[error("empty Phase A registry construction failed")]
    Registry,
    /// Sole-worker or dedicated Rayon4 construction failed.
    #[error("idle Phase A lifecycle construction failed")]
    Lifecycle,
}

impl From<RegistryError> for RuntimeInstallError {
    fn from(_error: RegistryError) -> Self {
        Self::Registry
    }
}

impl From<LifecycleError> for RuntimeInstallError {
    fn from(_error: LifecycleError) -> Self {
        Self::Lifecycle
    }
}

/// Exact empty-registry configuration used by the A0 runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MevTraderRuntimeConfig {
    registry: FixturePoolRegistry,
}

impl MevTraderRuntimeConfig {
    /// Constructs and validates the canonical empty fixture registry.
    pub fn empty() -> Result<Self, RuntimeInstallError> {
        let descriptors = Vec::new();
        let digest = RegistryHasher::digest(&descriptors)?;
        let registry = FixturePoolRegistry::new(descriptors, digest)?;
        Ok(Self { registry })
    }

    /// Returns true because A0 production wiring accepts only an empty registry.
    pub const fn is_empty(&self) -> bool {
        self.registry.is_empty()
    }
}

/// Provider-free idle runtime held by the sole node-start worker.
#[derive(Debug)]
pub struct MevTraderIdleRuntime {
    registry: FixturePoolRegistry,
    lifecycle: Arc<GlobalLifecycle>,
    ingress: Arc<LatestSlot<VictimFrame>>,
    analysis: DedicatedAnalysisPool,
    worker: WorkerClaim,
    watchdog: Watchdog,
}

impl MevTraderIdleRuntime {
    /// Starts the empty registry, capacity-one slot, sole worker, and dedicated Rayon4 pool.
    pub fn start(config: MevTraderRuntimeConfig) -> Result<Self, RuntimeInstallError> {
        let lifecycle = Arc::new(GlobalLifecycle::default());
        let ingress = Arc::new(LatestSlot::new(Arc::clone(&lifecycle)));
        let worker = SoleWorker::default().claim()?;
        let analysis = DedicatedAnalysisPool::new()?;
        Ok(Self {
            registry: config.registry,
            lifecycle,
            ingress,
            analysis,
            worker,
            watchdog: Watchdog,
        })
    }

    /// Returns the exact A0 idle telemetry marker.
    pub const fn status(&self) -> &'static str {
        A0_IDLE_STATUS
    }

    /// Returns true only while the required production registry remains empty.
    pub const fn registry_is_empty(&self) -> bool {
        self.registry.is_empty()
    }

    /// Returns false because A0 exports and creates no live victim producer.
    pub const fn has_live_victim_producer(&self) -> bool {
        false
    }

    /// Returns the exact dedicated Rayon4 thread count.
    pub fn analysis_thread_count(&self) -> usize {
        self.analysis.thread_count()
    }

    /// Returns whether the decoded-frame slot remains idle.
    pub fn ingress_is_idle(&self) -> bool {
        self.ingress.try_take().is_none()
    }

    /// Returns shared fail-closed lifecycle state for the idle control task.
    pub const fn lifecycle(&self) -> &Arc<GlobalLifecycle> {
        &self.lifecycle
    }

    /// Returns the separate classification-only watchdog.
    pub const fn watchdog(&self) -> Watchdog {
        self.watchdog
    }

    /// Proves that the irreversible sole-worker claim is held by this runtime.
    pub const fn worker_is_claimed(&self) -> bool {
        self.worker.marker();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ANALYSIS_THREADS;

    #[test]
    fn runtime_is_empty_idle_and_has_no_live_victim_producer() {
        let config = MevTraderRuntimeConfig::empty().expect("empty config");
        assert!(config.is_empty());
        let runtime = MevTraderIdleRuntime::start(config).expect("idle runtime");
        assert_eq!(runtime.status(), A0_IDLE_STATUS);
        assert!(runtime.registry_is_empty());
        assert!(!runtime.has_live_victim_producer());
        assert_eq!(runtime.analysis_thread_count(), ANALYSIS_THREADS);
        assert!(runtime.ingress_is_idle());
        assert!(runtime.worker_is_claimed());
    }
}
