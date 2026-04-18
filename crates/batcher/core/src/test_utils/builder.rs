//! Factory helpers for constructing test [`BatchDriver`] instances and [`BatchSubmission`] stubs.

use std::{sync::Arc, time::Duration};

use alloy_primitives::Address;
use base_batcher_encoder::{BatchSubmission, DaType, SubmissionId};
use base_protocol::{ChannelId, Frame};
use base_runtime::Runtime;
use base_tx_manager::TxManager;

use crate::{
    BatchDriver, BatchDriverConfig, DaThrottle, NoopThrottleClient, ThrottleController,
    test_utils::{PendingL1HeadSource, PendingSource, TrackingPipeline},
};

/// Factory methods for [`BatchSubmission`] stubs used in driver tests.
#[derive(Debug)]
pub struct SubmissionStub;

impl SubmissionStub {
    /// Returns a stub submission with id `0`.
    pub fn stub() -> BatchSubmission {
        Self::with_id(0)
    }

    /// Returns a stub submission with the given id.
    pub fn with_id(id: u64) -> BatchSubmission {
        BatchSubmission {
            id: SubmissionId(id),
            channel_id: ChannelId::default(),
            da_type: DaType::Blob,
            frames: vec![Arc::new(Frame::default())],
        }
    }
}

/// Factory methods for [`BatchDriver`] instances wired with standard test
/// components: [`PendingSource`], [`NoopThrottleClient`], and [`PendingL1HeadSource`].
#[derive(Debug)]
pub struct DriverFixture;

impl DriverFixture {
    /// Build a driver with `max_pending_transactions = 1`.
    pub fn build<R: Runtime, TM: TxManager>(
        runtime: R,
        pipeline: TrackingPipeline,
        tx_manager: TM,
    ) -> BatchDriver<
        R,
        TrackingPipeline,
        PendingSource,
        TM,
        Arc<NoopThrottleClient>,
        PendingL1HeadSource,
    > {
        Self::build_with_max_pending(runtime, pipeline, tx_manager, 1)
    }

    /// Build a driver with a configurable `max_pending_transactions` limit.
    pub fn build_with_max_pending<R: Runtime, TM: TxManager>(
        runtime: R,
        pipeline: TrackingPipeline,
        tx_manager: TM,
        max_pending: usize,
    ) -> BatchDriver<
        R,
        TrackingPipeline,
        PendingSource,
        TM,
        Arc<NoopThrottleClient>,
        PendingL1HeadSource,
    > {
        BatchDriver::new(
            runtime,
            pipeline,
            PendingSource,
            tx_manager,
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: max_pending,
                drain_timeout: Duration::from_millis(10),
                force_blobs_when_throttling: true,
            },
            DaThrottle::new(ThrottleController::noop(), Arc::new(NoopThrottleClient)),
            PendingL1HeadSource,
        )
    }
}
