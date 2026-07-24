//! Shared test doubles for the telemetry service.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use tokio::sync::Semaphore;

use crate::{
    ReachabilityProber, RlpxProbeOutcome, RlpxProbeResult, RlpxProbeStage, RlpxProbeTarget,
};

/// Prober whose probes park until the test releases them.
///
/// Hand-rolled instead of [`MockReachabilityProber`](crate::MockReachabilityProber)
/// because the double must coordinate with the test while calls are in flight
/// (parking on semaphores inside `probe`), which mockall's synchronous
/// expectation closures cannot express.
#[derive(Debug, Clone)]
pub struct BlockingProber {
    /// Gains one permit each time a probe enters.
    pub entered: Arc<Semaphore>,
    /// Probes park here until the test adds permits.
    pub release: Arc<Semaphore>,
}

#[async_trait]
impl ReachabilityProber for BlockingProber {
    async fn probe(&self, _: RlpxProbeTarget) -> RlpxProbeResult {
        self.entered.add_permits(1);
        self.release.acquire().await.unwrap().forget();
        RlpxProbeResult {
            outcome: RlpxProbeOutcome::Reachable,
            stage: RlpxProbeStage::Devp2pHello,
            elapsed: Duration::from_millis(1),
            client_version: None,
        }
    }
}
