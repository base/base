//! Builder for test [`BatchDriver`] instances, and block and [`BatchSubmission`] stubs.

use std::{sync::Arc, time::Duration};

use alloy_consensus::Header;
use alloy_primitives::Address;
use base_batcher_encoder::{BatchPipeline, BatchSubmission, BlobPayload, SubmissionId};
use base_batcher_source::{L1HeadSource, UnsafeBlockSource};
use base_common_consensus::BaseBlock;
use base_protocol::{BlockInfo, Frame};
use base_runtime::Runtime;
use base_tx_manager::TxManager;
use tokio::sync::mpsc;

use crate::{
    AdminHandle, BatchDriver, BatchDriverConfig, BatchDriverInputs, DaThrottle, DerivationStatus,
    NoopThrottleClient, ThrottleClient, ThrottleController,
    test_utils::{PendingL1HeadSource, PendingSource},
};

/// Factory for empty L2 block stubs used in driver tests.
#[derive(Debug)]
pub struct BlockStub;

impl BlockStub {
    /// Returns an empty block with the given number. Pick one above the driver's safe head,
    /// or the driver drops it as already safe.
    pub fn with_number(number: u64) -> BaseBlock {
        BaseBlock { header: Header { number, ..Default::default() }, body: Default::default() }
    }
}

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
        BatchSubmission::blobs(
            SubmissionId(id),
            vec![BlobPayload::new(vec![Arc::new(Frame::default())])],
        )
    }
}

/// Builds a [`BatchDriver`] for tests, with a parked source, a parked L1 head source, a
/// disabled throttle and at most one in-flight transaction unless told otherwise.
///
/// The driver starts from L1 head 0 and, unless [`initial_status`](Self::initial_status)
/// says otherwise, from the L2 genesis (block 0) as safe head, so it drops blocks
/// numbered 0 as already safe.
///
/// [`build`](Self::build) also creates the derivation-status and admin channels and hands
/// their sending sides back as [`DriverHandles`]. Keep them alive while the driver runs:
/// dropping the derivation sender is fatal to the driver, dropping the admin handle silences
/// its admin arm.
#[derive(Debug)]
pub struct DriverFixture<
    R,
    P,
    TM,
    S = PendingSource,
    L = PendingL1HeadSource,
    TC = Arc<NoopThrottleClient>,
> where
    TC: ThrottleClient,
{
    runtime: R,
    pipeline: P,
    tx_manager: TM,
    source: S,
    l1_head_source: L,
    throttle: DaThrottle<TC>,
    max_pending: usize,
    initial_status: DerivationStatus,
}

/// The sending sides of a fixture-built driver's channels.
#[derive(Debug)]
pub struct DriverHandles {
    /// Admin commands.
    pub admin: AdminHandle,
    /// Derivation-status updates. The driver exits once this is dropped.
    pub derivation_status_tx: mpsc::Sender<DerivationStatus>,
}

impl<R: Runtime, P: BatchPipeline, TM: TxManager> DriverFixture<R, P, TM> {
    /// Start a fixture around the three components every driver test provides.
    pub fn new(runtime: R, pipeline: P, tx_manager: TM) -> Self {
        Self {
            runtime,
            pipeline,
            tx_manager,
            source: PendingSource,
            l1_head_source: PendingL1HeadSource,
            throttle: DaThrottle::new(ThrottleController::disabled(), Arc::new(NoopThrottleClient)),
            max_pending: 1,
            initial_status: DerivationStatus::from_safe_l2(BlockInfo::default()),
        }
    }
}

impl<R, P, TM, S, L, TC> DriverFixture<R, P, TM, S, L, TC>
where
    R: Runtime,
    P: BatchPipeline,
    TM: TxManager,
    S: UnsafeBlockSource,
    L: L1HeadSource,
    TC: ThrottleClient,
{
    /// Replace the L2 block source.
    pub fn source<S2: UnsafeBlockSource>(self, source: S2) -> DriverFixture<R, P, TM, S2, L, TC> {
        DriverFixture {
            runtime: self.runtime,
            pipeline: self.pipeline,
            tx_manager: self.tx_manager,
            source,
            l1_head_source: self.l1_head_source,
            throttle: self.throttle,
            max_pending: self.max_pending,
            initial_status: self.initial_status,
        }
    }

    /// Replace the L1 head source.
    pub fn l1_head_source<L2: L1HeadSource>(
        self,
        l1_head_source: L2,
    ) -> DriverFixture<R, P, TM, S, L2, TC> {
        DriverFixture {
            runtime: self.runtime,
            pipeline: self.pipeline,
            tx_manager: self.tx_manager,
            source: self.source,
            l1_head_source,
            throttle: self.throttle,
            max_pending: self.max_pending,
            initial_status: self.initial_status,
        }
    }

    /// Replace the DA throttle.
    pub fn throttle<TC2: ThrottleClient>(
        self,
        throttle: DaThrottle<TC2>,
    ) -> DriverFixture<R, P, TM, S, L, TC2> {
        DriverFixture {
            runtime: self.runtime,
            pipeline: self.pipeline,
            tx_manager: self.tx_manager,
            source: self.source,
            l1_head_source: self.l1_head_source,
            throttle,
            max_pending: self.max_pending,
            initial_status: self.initial_status,
        }
    }

    /// Set `max_pending_transactions`.
    pub const fn max_pending(mut self, max_pending: usize) -> Self {
        self.max_pending = max_pending;
        self
    }

    /// Set the derivation status the driver starts from.
    pub const fn initial_status(mut self, initial_status: DerivationStatus) -> Self {
        self.initial_status = initial_status;
        self
    }

    /// Build the driver and the handles that feed it.
    pub fn build(self) -> (BatchDriver<R, P, S, TM, TC, L>, DriverHandles) {
        let (admin, admin_rx) = AdminHandle::channel();
        let (derivation_status_tx, derivation_status_rx) = mpsc::channel(1);
        let driver = BatchDriver::new(
            self.runtime,
            self.pipeline,
            self.tx_manager,
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: self.max_pending,
                drain_timeout: Duration::from_millis(10),
                force_blobs_when_throttling: true,
                stopped: false,
            },
            self.throttle,
            BatchDriverInputs {
                source: self.source,
                l1_head_source: self.l1_head_source,
                initial_l1_head: 0,
                initial_status: self.initial_status,
                derivation_status_rx,
                admin_rx,
            },
        );
        (driver, DriverHandles { admin, derivation_status_tx })
    }
}
