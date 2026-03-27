mod error;
pub use error::{OpProofStoragePrunerResult, PrunerError, PrunerOutput};

mod pruner;
pub use pruner::OpProofStoragePruner;

#[cfg(feature = "metrics")]
mod metrics;
#[cfg(not(feature = "metrics"))]
#[allow(missing_docs)]
mod metrics {
    use crate::PrunerOutput;

    pub(super) struct Metrics;

    impl Metrics {
        #[inline(always)]
        pub(super) fn record_prune_result(_: PrunerOutput) {}
    }
}

mod task;
pub use task::OpProofStoragePrunerTask;
