//! Flashblocks state processor.

use std::{
    collections::BTreeMap,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard, RwLock as StdRwLock},
    time::Instant,
};

use alloy_consensus::{
    Block, BlockBody, Header,
    transaction::{Recovered, SignerRecoverable},
};
use alloy_eips::{BlockNumberOrTag, Decodable2718};
use alloy_network::TransactionResponse;
use alloy_primitives::{Address, BlockNumber};
use alloy_rpc_types_eth::state::StateOverride;
use arc_swap::ArcSwapOption;
use base_common_chains::Upgrades;
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_flashblocks::Flashblock;
use base_execution_evm::{BaseEvmConfig, BaseNextBlockEnvAttributes};
use rayon::prelude::*;
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_evm::ConfigureEvm;
use reth_primitives_traits::RecoveredBlock;
use reth_provider::{BlockReaderIdExt, StateProviderBox, StateProviderFactory};
use reth_revm::{State, database::StateProviderDatabase};
use revm_database::states::bundle_state::BundleRetention;
use tokio::sync::{Mutex, broadcast::Sender, mpsc::UnboundedReceiver};

#[cfg(feature = "edge-measurement")]
use crate::cache::CacheInsertObservation;
#[cfg(feature = "edge-measurement")]
use crate::edge_measurement::{PendingSendJournalMarkerV2, ProcessorTerminalInputV1};
use crate::{
    AssembledBlock, BlockAssembler, ExecutionError, FlashblockCache, PendingBlocks,
    PendingBlocksBuilder, PendingFrameObserver, PendingStateBuilder, ProviderError, Result,
    StateProcessorError,
    metrics::Metrics,
    validation::{
        CanonicalBlockReconciler, FlashblockSequenceValidator, ReconciliationStrategy,
        ReorgDetector, SequenceValidationResult,
    },
};
#[cfg(feature = "edge-measurement")]
use crate::{
    BuildError, EdgeMeasurementGlobal, PendingRegistrationAttemptV2, ProcessorBaseDispositionV1,
    ProcessorObserverDispositionV1, ProcessorPublishDispositionV1, ProtocolError,
};

#[cfg(feature = "edge-measurement")]
macro_rules! measured {
    ($result:expr, $disposition:expr) => {
        $result.map(|processed| processed.with_measurement_disposition($disposition))
    };
}

#[cfg(not(feature = "edge-measurement"))]
macro_rules! measured {
    ($result:expr, $disposition:expr) => {{
        let _ = stringify!($disposition);
        $result
    }};
}

type PendingExecutionDb = State<StateProviderDatabase<StateProviderBox>>;

#[derive(Debug)]
struct LivePendingState {
    db: PendingExecutionDb,
    state_overrides: StateOverride,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObserverDelivery {
    Absent,
    Delivered,
    Panicked,
}

/// Messages consumed by the state processor.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum StateUpdate {
    /// New canonical block to reconcile against pending state.
    Canonical(RecoveredBlock<BaseBlock>),
    /// Incoming flashblock payload to extend pending state.
    Flashblock(Flashblock),
}

/// Processes flashblocks and canonical blocks to keep pending state updated.
#[derive(Debug)]
pub struct StateProcessor<Client> {
    rx: Arc<Mutex<UnboundedReceiver<StateUpdate>>>,
    pending_blocks: Arc<ArcSwapOption<PendingBlocks>>,
    max_depth: u64,
    client: Client,
    sender: Sender<Arc<PendingBlocks>>,
    cache: Arc<Mutex<FlashblockCache>>,
    live_state: StdMutex<Option<LivePendingState>>,
    pending_frame_observer: Arc<StdRwLock<Option<Arc<dyn PendingFrameObserver>>>>,
}

#[derive(Debug)]
struct ProcessedFlashblock {
    pending_blocks: Option<Arc<PendingBlocks>>,
    advanced: bool,
    #[cfg(feature = "edge-measurement")]
    measurement_registration: Option<PendingRegistrationAttemptV2>,
    #[cfg(feature = "edge-measurement")]
    measurement_send_marker: Option<PendingSendJournalMarkerV2>,
    #[cfg(feature = "edge-measurement")]
    measurement_base_disposition: ProcessorBaseDispositionV1,
    #[cfg(feature = "edge-measurement")]
    measurement_observer_disposition: ProcessorObserverDispositionV1,
}

impl ProcessedFlashblock {
    const fn advanced(pending_blocks: Option<Arc<PendingBlocks>>) -> Self {
        Self {
            pending_blocks,
            advanced: true,
            #[cfg(feature = "edge-measurement")]
            measurement_registration: None,
            #[cfg(feature = "edge-measurement")]
            measurement_send_marker: None,
            #[cfg(feature = "edge-measurement")]
            measurement_base_disposition: ProcessorBaseDispositionV1::UnknownProcessorBranch,
            #[cfg(feature = "edge-measurement")]
            measurement_observer_disposition: ProcessorObserverDispositionV1::Absent,
        }
    }

    const fn unchanged(pending_blocks: Option<Arc<PendingBlocks>>) -> Self {
        Self {
            pending_blocks,
            advanced: false,
            #[cfg(feature = "edge-measurement")]
            measurement_registration: None,
            #[cfg(feature = "edge-measurement")]
            measurement_send_marker: None,
            #[cfg(feature = "edge-measurement")]
            measurement_base_disposition: ProcessorBaseDispositionV1::UnknownProcessorBranch,
            #[cfg(feature = "edge-measurement")]
            measurement_observer_disposition: ProcessorObserverDispositionV1::Absent,
        }
    }

    #[cfg(feature = "edge-measurement")]
    const fn with_measurement_disposition(
        mut self,
        disposition: ProcessorBaseDispositionV1,
    ) -> Self {
        self.measurement_base_disposition = disposition;
        self
    }
}

fn notify_pending_frame_observer(
    observer: Option<Arc<dyn PendingFrameObserver>>,
    pending_blocks: &PendingBlocks,
) -> ObserverDelivery {
    let Some(observer) = observer else {
        Metrics::pending_frame_observer_skipped().increment(1);
        return ObserverDelivery::Absent;
    };

    let start_time = Instant::now();
    let result = catch_unwind(AssertUnwindSafe(|| observer.on_pending_frame(pending_blocks)));
    Metrics::pending_frame_observer_duration().record(start_time.elapsed());
    if result.is_err() {
        Metrics::pending_frame_observer_panics().increment(1);
        warn!(message = "pending-frame observer panicked; continuing flashblock processing");
        ObserverDelivery::Panicked
    } else {
        ObserverDelivery::Delivered
    }
}
fn notify_processed_pending_frame_observer(
    observer: Option<Arc<dyn PendingFrameObserver>>,
    processed: &ProcessedFlashblock,
) -> ObserverDelivery {
    if processed.advanced
        && let Some(ref pb) = processed.pending_blocks
    {
        notify_pending_frame_observer(observer, pb)
    } else {
        ObserverDelivery::Absent
    }
}

#[cfg(feature = "edge-measurement")]
const fn processor_error_product(
    error: &StateProcessorError,
) -> (ProcessorBaseDispositionV1, &'static str) {
    match error {
        StateProcessorError::Protocol(ProtocolError::InvalidSequence) => {
            (ProcessorBaseDispositionV1::ProcessErrorProtocol, "InvalidSequence")
        }
        StateProcessorError::Protocol(ProtocolError::MissingBase) => {
            (ProcessorBaseDispositionV1::ProcessErrorProtocol, "MissingBase")
        }
        StateProcessorError::Protocol(ProtocolError::EmptyFlashblocks) => {
            (ProcessorBaseDispositionV1::ProcessErrorProtocol, "EmptyFlashblocks")
        }
        StateProcessorError::Provider(ProviderError::MissingCanonicalHeader { .. }) => {
            (ProcessorBaseDispositionV1::ProcessErrorProvider, "MissingCanonicalHeaderUncacheable")
        }
        StateProcessorError::Provider(ProviderError::StateProvider(_)) => {
            (ProcessorBaseDispositionV1::ProcessErrorProvider, "StateProvider")
        }
        StateProcessorError::Execution(ExecutionError::TransactionFailed { .. }) => {
            (ProcessorBaseDispositionV1::ProcessErrorExecution, "TransactionFailed")
        }
        StateProcessorError::Execution(ExecutionError::SenderRecovery(_)) => {
            (ProcessorBaseDispositionV1::ProcessErrorExecution, "SenderRecovery")
        }
        StateProcessorError::Execution(ExecutionError::DepositReceiptMismatch) => {
            (ProcessorBaseDispositionV1::ProcessErrorExecution, "DepositReceiptMismatch")
        }
        StateProcessorError::Execution(ExecutionError::GasOverflow) => {
            (ProcessorBaseDispositionV1::ProcessErrorExecution, "GasOverflow")
        }
        StateProcessorError::Execution(ExecutionError::EvmEnv(_)) => {
            (ProcessorBaseDispositionV1::ProcessErrorExecution, "EvmEnv")
        }
        StateProcessorError::Execution(ExecutionError::L1BlockInfo(_)) => {
            (ProcessorBaseDispositionV1::ProcessErrorExecution, "L1BlockInfo")
        }
        StateProcessorError::Execution(ExecutionError::BlockConversion(_)) => {
            (ProcessorBaseDispositionV1::ProcessErrorExecution, "BlockConversion")
        }
        StateProcessorError::Execution(ExecutionError::DepositAccountLoad) => {
            (ProcessorBaseDispositionV1::ProcessErrorExecution, "DepositAccountLoad")
        }
        StateProcessorError::Execution(ExecutionError::RpcReceiptBuild(_)) => {
            (ProcessorBaseDispositionV1::ProcessErrorExecution, "RpcReceiptBuild")
        }
        StateProcessorError::Execution(ExecutionError::DaFootprintEstimation(_)) => {
            (ProcessorBaseDispositionV1::ProcessErrorExecution, "DaFootprintEstimation")
        }
        StateProcessorError::Build(BuildError::MissingHeaders) => {
            (ProcessorBaseDispositionV1::ProcessErrorBuild, "MissingHeaders")
        }
        StateProcessorError::Build(BuildError::NoFlashblocks) => {
            (ProcessorBaseDispositionV1::ProcessErrorBuild, "NoFlashblocks")
        }
        StateProcessorError::Build(BuildError::MissingReceipt { .. }) => {
            (ProcessorBaseDispositionV1::ProcessErrorBuild, "MissingReceipt")
        }
        StateProcessorError::Build(BuildError::DuplicateTransaction { .. }) => {
            (ProcessorBaseDispositionV1::ProcessErrorBuild, "DuplicateTransaction")
        }
        StateProcessorError::MissingFirstFlashblock => {
            (ProcessorBaseDispositionV1::MissingFirstUncacheable, "MissingFirstFlashblock")
        }
    }
}

impl<Client> StateProcessor<Client>
where
    Client: StateProviderFactory
        + ChainSpecProvider<ChainSpec: EthChainSpec<Header = Header> + Upgrades>
        + BlockReaderIdExt<Header = Header>
        + Clone
        + 'static,
{
    fn lock_live_state(&self) -> StdMutexGuard<'_, Option<LivePendingState>> {
        self.live_state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn clear_live_state(&self) {
        *self.lock_live_state() = None;
    }

    fn set_live_state(&self, db: PendingExecutionDb, state_overrides: StateOverride) {
        *self.lock_live_state() = Some(LivePendingState { db, state_overrides });
    }

    fn publish_pending_blocks(
        &self,
        mut pending_blocks_builder: PendingBlocksBuilder,
        mut db: PendingExecutionDb,
        state_overrides: StateOverride,
    ) -> Result<Option<Arc<PendingBlocks>>> {
        db.merge_transitions(BundleRetention::Reverts);
        pending_blocks_builder.with_bundle_state(db.bundle_state.clone());
        pending_blocks_builder.with_state_overrides(state_overrides.clone());

        let pending_blocks = Arc::new(pending_blocks_builder.build()?);
        self.set_live_state(db, state_overrides);

        Ok(Some(pending_blocks))
    }

    /// Creates a new state processor wired to the provided channels and state.
    pub fn new(
        client: Client,
        pending_blocks: Arc<ArcSwapOption<PendingBlocks>>,
        max_depth: u64,
        rx: Arc<Mutex<UnboundedReceiver<StateUpdate>>>,
        sender: Sender<Arc<PendingBlocks>>,
        pending_frame_observer: Arc<StdRwLock<Option<Arc<dyn PendingFrameObserver>>>>,
    ) -> Self {
        let cache = client
            .best_block_number()
            .map_or_else(|_| FlashblockCache::new(0), FlashblockCache::new);

        Self {
            pending_blocks,
            client,
            max_depth,
            rx,
            sender,
            cache: Arc::new(Mutex::new(cache)),
            live_state: StdMutex::new(None),
            pending_frame_observer,
        }
    }

    #[cfg(feature = "edge-measurement")]
    fn record_cache_observation(
        observation: &CacheInsertObservation,
        source_generation: Option<u64>,
        wait_disposition: ProcessorBaseDispositionV1,
    ) {
        let Some(recorder) = EdgeMeasurementGlobal::installed() else {
            return;
        };
        for generation in observation
            .evicted_generations
            .iter()
            .copied()
            .filter(|generation| recorder.claim_cache_resolution(*generation))
        {
            recorder.record_generation_product(ProcessorTerminalInputV1 {
                source_generation: generation,
                base_disposition: ProcessorBaseDispositionV1::CacheEvicted,
                observer_disposition: ProcessorObserverDispositionV1::Absent,
                publish_disposition: ProcessorPublishDispositionV1::NotApplicable,
                pending_snapshot_sequence: None,
                processor_error_reason: None,
                cache_resolved_final_disposition: None,
            });
        }
        if let Some(generation) = observation
            .replaced_generation
            .filter(|generation| recorder.claim_cache_resolution(*generation))
        {
            recorder.record_generation_product(ProcessorTerminalInputV1 {
                source_generation: generation,
                base_disposition: ProcessorBaseDispositionV1::CacheReplacedOldGeneration,
                observer_disposition: ProcessorObserverDispositionV1::Absent,
                publish_disposition: ProcessorPublishDispositionV1::NotApplicable,
                pending_snapshot_sequence: None,
                processor_error_reason: None,
                cache_resolved_final_disposition: None,
            });
        }
        if let Some(generation) = source_generation {
            if observation.cached {
                recorder.observe_cache_wait(generation, wait_disposition);
            } else {
                recorder.record_generation_product(ProcessorTerminalInputV1 {
                    source_generation: generation,
                    base_disposition: ProcessorBaseDispositionV1::CacheRejectedAhead,
                    observer_disposition: ProcessorObserverDispositionV1::Absent,
                    publish_disposition: ProcessorPublishDispositionV1::NotApplicable,
                    pending_snapshot_sequence: None,
                    processor_error_reason: None,
                    cache_resolved_final_disposition: None,
                });
            }
        }
    }

    /// Processes updates from the queue until the channel closes.
    pub async fn start(&self) {
        while let Some(update) = self.rx.lock().await.recv().await {
            let prev_pending_blocks = self.pending_blocks.load_full();
            match update {
                StateUpdate::Canonical(block) => {
                    debug!(message = "processing canonical block", block_number = block.number);
                    // Move the blocking MDBX read-tx + EVM rebuild off the async
                    // worker thread so it cannot starve the runtime. Safe because
                    // the node runs on a multi_thread runtime
                    // (CliRunner::try_default_runtime -> new_multi_thread); the
                    // closure runs on the same thread so all `&self`/borrow
                    // captures stay on-thread (no Send + 'static, no clone).
                    match tokio::task::block_in_place(|| {
                        self.process_canonical_block(prev_pending_blocks, &block)
                    }) {
                        Ok(new_pending_blocks) => {
                            self.pending_blocks.swap(new_pending_blocks);

                            let mut cache = self.cache.lock().await;
                            #[cfg(feature = "edge-measurement")]
                            let evicted_generations = cache.update_canonical_observed(block.number);
                            #[cfg(feature = "edge-measurement")]
                            if let Some(recorder) = EdgeMeasurementGlobal::installed() {
                                recorder.close_payloads_through(block.number);
                            }
                            #[cfg(not(feature = "edge-measurement"))]
                            cache.update_canonical(block.number);
                            #[cfg(feature = "edge-measurement")]
                            let cached = cache.drain_observed(block.number + 1);
                            #[cfg(not(feature = "edge-measurement"))]
                            let cached = cache.drain(block.number + 1);
                            drop(cache);

                            #[cfg(feature = "edge-measurement")]
                            if let Some(recorder) = EdgeMeasurementGlobal::installed() {
                                for generation in
                                    evicted_generations.into_iter().filter(|generation| {
                                        recorder.claim_cache_resolution(*generation)
                                    })
                                {
                                    recorder.record_generation_product(ProcessorTerminalInputV1 {
                                        source_generation: generation,
                                        base_disposition: ProcessorBaseDispositionV1::CacheEvicted,
                                        observer_disposition:
                                            ProcessorObserverDispositionV1::Absent,
                                        publish_disposition:
                                            ProcessorPublishDispositionV1::NotApplicable,
                                        pending_snapshot_sequence: None,
                                        processor_error_reason: None,
                                        cache_resolved_final_disposition: None,
                                    });
                                }
                            }

                            if !cached.is_empty() {
                                debug!(
                                    message = "replaying cached flashblocks after canonical block",
                                    canonical_block = block.number,
                                    cached_count = cached.len(),
                                );
                                #[cfg(feature = "edge-measurement")]
                                for (flashblock, source_generation) in cached {
                                    let source_generation =
                                        source_generation.filter(|generation| {
                                            EdgeMeasurementGlobal::installed().is_some_and(
                                                |recorder| {
                                                    recorder.claim_cache_resolution(*generation)
                                                },
                                            )
                                        });
                                    let fb_prev = self.pending_blocks.load_full();
                                    self.apply_flashblock_observed(
                                        fb_prev,
                                        flashblock,
                                        source_generation,
                                        source_generation.is_some(),
                                    )
                                    .await;
                                }
                                #[cfg(not(feature = "edge-measurement"))]
                                for flashblock in cached {
                                    let fb_prev = self.pending_blocks.load_full();
                                    self.apply_flashblock(fb_prev, flashblock).await;
                                }
                            }
                        }
                        Err(e) => {
                            error!(message = "could not process canonical block", error = %e);
                        }
                    }
                }
                StateUpdate::Flashblock(flashblock) => {
                    debug!(
                        message = "processing flashblock",
                        block_number = flashblock.metadata.block_number,
                        flashblock_index = flashblock.index
                    );
                    self.apply_flashblock(prev_pending_blocks, flashblock).await;
                }
            }
        }
    }

    async fn apply_flashblock(
        &self,
        prev_pending_blocks: Option<Arc<PendingBlocks>>,
        flashblock: Flashblock,
    ) {
        #[cfg(feature = "edge-measurement")]
        {
            let source_generation =
                EdgeMeasurementGlobal::recorder().take_source_generation(&flashblock);
            self.apply_flashblock_observed(
                prev_pending_blocks,
                flashblock,
                source_generation,
                false,
            )
            .await;
        }
        #[cfg(not(feature = "edge-measurement"))]
        self.apply_flashblock_observed(prev_pending_blocks, flashblock, None, false).await;
    }

    async fn apply_flashblock_observed(
        &self,
        prev_pending_blocks: Option<Arc<PendingBlocks>>,
        flashblock: Flashblock,
        source_generation: Option<u64>,
        cache_resolved: bool,
    ) {
        #[cfg(not(feature = "edge-measurement"))]
        let _ = (source_generation, cache_resolved);
        let start_time = Instant::now();
        // Move the blocking MDBX read-tx + EVM rebuild off the async worker
        // thread (multi_thread runtime; borrows stay on-thread, no clone).
        match tokio::task::block_in_place(|| {
            let processed = self.process_flashblock(prev_pending_blocks, &flashblock)?;
            #[cfg(feature = "edge-measurement")]
            let mut processed = processed;
            let observer = self
                .pending_frame_observer
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            let observer_delivery = notify_processed_pending_frame_observer(observer, &processed);
            #[cfg(feature = "edge-measurement")]
            {
                processed.measurement_observer_disposition = match observer_delivery {
                    ObserverDelivery::Absent => ProcessorObserverDispositionV1::Absent,
                    ObserverDelivery::Delivered => ProcessorObserverDispositionV1::Delivered,
                    ObserverDelivery::Panicked => ProcessorObserverDispositionV1::Panicked,
                };
            }
            #[cfg(not(feature = "edge-measurement"))]
            {
                _ = observer_delivery;
            }
            #[cfg(feature = "edge-measurement")]
            if let Some(ref pending) = processed.pending_blocks
                && let Some(installed) = EdgeMeasurementGlobal::installed()
            {
                (processed.measurement_registration, processed.measurement_send_marker) = installed
                    .prepare_pending_publication(pending, processed.advanced, source_generation);
            }
            Ok(processed)
        }) {
            Ok(processed) => {
                #[cfg(feature = "edge-measurement")]
                let actual_base_disposition =
                    if processed.advanced && processed.pending_blocks.is_none() {
                        ProcessorBaseDispositionV1::AdvancedWithoutSnapshot
                    } else {
                        processed.measurement_base_disposition
                    };
                #[cfg(feature = "edge-measurement")]
                let pending_snapshot_sequence = processed
                    .measurement_registration
                    .and_then(|registration| registration.pending_snapshot_sequence);
                #[cfg(feature = "edge-measurement")]
                let mut publish_disposition = ProcessorPublishDispositionV1::NotApplicable;
                #[cfg(feature = "edge-measurement")]
                let mut generation_product_recorded = false;
                let new_pending_blocks = processed.pending_blocks;
                if let Some(ref pb) = new_pending_blocks {
                    #[cfg(feature = "edge-measurement")]
                    {
                        let send_result = self.sender.send(Arc::clone(pb));
                        if processed.advanced {
                            publish_disposition = send_result.as_ref().ok().map_or(
                                ProcessorPublishDispositionV1::NoReceivers,
                                |count| {
                                    ProcessorPublishDispositionV1::Published(
                                        u64::try_from(*count)
                                            .expect("broadcast receiver count fits u64"),
                                    )
                                },
                            );
                        }
                        if let (Some(recorder), Some(generation)) =
                            (EdgeMeasurementGlobal::installed(), source_generation)
                        {
                            recorder.record_generation_product(ProcessorTerminalInputV1 {
                                source_generation: generation,
                                base_disposition: if cache_resolved {
                                    ProcessorBaseDispositionV1::CacheResolvedToProcessor
                                } else {
                                    actual_base_disposition
                                },
                                observer_disposition: processed.measurement_observer_disposition,
                                publish_disposition,
                                pending_snapshot_sequence,
                                processor_error_reason: None,
                                cache_resolved_final_disposition: cache_resolved
                                    .then_some(actual_base_disposition),
                            });
                            generation_product_recorded = true;
                        }
                        if let Some(registration) = processed.measurement_registration {
                            let receiver_count = send_result.as_ref().ok().copied();
                            if EdgeMeasurementGlobal::installed().is_some_and(|recorder| {
                                recorder
                                    .registry()
                                    .record_send(registration, receiver_count)
                                    .is_err()
                            }) {
                                warn!(
                                    message =
                                        "edge measurement send disposition could not be recorded"
                                );
                            }
                        }
                        if let Some(marker) = processed.measurement_send_marker
                            && EdgeMeasurementGlobal::installed().is_some_and(|recorder| {
                                recorder
                                    .registry()
                                    .record_unregistered_send(
                                        marker,
                                        send_result.as_ref().ok().copied(),
                                    )
                                    .is_err()
                            })
                        {
                            warn!(
                                message = "edge measurement non-authority send disposition could not be recorded"
                            );
                        }
                        _ = send_result;
                    }
                    #[cfg(not(feature = "edge-measurement"))]
                    {
                        _ = self.sender.send(Arc::clone(pb));
                    }
                }
                #[cfg(feature = "edge-measurement")]
                if !generation_product_recorded
                    && let (Some(recorder), Some(generation)) =
                        (EdgeMeasurementGlobal::installed(), source_generation)
                {
                    recorder.record_generation_product(ProcessorTerminalInputV1 {
                        source_generation: generation,
                        base_disposition: if cache_resolved {
                            ProcessorBaseDispositionV1::CacheResolvedToProcessor
                        } else {
                            actual_base_disposition
                        },
                        observer_disposition: processed.measurement_observer_disposition,
                        publish_disposition,
                        pending_snapshot_sequence,
                        processor_error_reason: None,
                        cache_resolved_final_disposition: cache_resolved
                            .then_some(actual_base_disposition),
                    });
                }
                self.pending_blocks.swap(new_pending_blocks);
                Metrics::block_processing_duration().record(start_time.elapsed());
            }
            Err(e) => {
                #[cfg(feature = "edge-measurement")]
                {
                    match &e {
                        StateProcessorError::Provider(ProviderError::MissingCanonicalHeader {
                            ..
                        }) => {
                            let observation = self
                                .cache
                                .lock()
                                .await
                                .insert_observed(flashblock, source_generation);
                            Self::record_cache_observation(
                                &observation,
                                source_generation,
                                ProcessorBaseDispositionV1::CachedAwaitCanonical,
                            );
                            if observation.cached {
                                debug!(
                                    message = "cached flashblock pending canonical block",
                                    error = %e
                                );
                            }
                            return;
                        }
                        StateProcessorError::MissingFirstFlashblock => {
                            let mut cache = self.cache.lock().await;
                            if flashblock.index > 0
                                && cache.has_flashblock(
                                    flashblock.metadata.block_number,
                                    flashblock.index - 1,
                                )
                            {
                                let observation =
                                    cache.insert_observed(flashblock, source_generation);
                                drop(cache);
                                Self::record_cache_observation(
                                    &observation,
                                    source_generation,
                                    ProcessorBaseDispositionV1::CachedAwaitPredecessor,
                                );
                                if observation.cached {
                                    return;
                                }
                                return;
                            }
                            drop(cache);
                            if let (Some(recorder), Some(generation)) =
                                (EdgeMeasurementGlobal::installed(), source_generation)
                            {
                                recorder.record_generation_product(ProcessorTerminalInputV1 {
                                    source_generation: generation,
                                    base_disposition: if cache_resolved {
                                        ProcessorBaseDispositionV1::CacheResolvedToProcessor
                                    } else {
                                        ProcessorBaseDispositionV1::MissingFirstUncacheable
                                    },
                                    observer_disposition: ProcessorObserverDispositionV1::Absent,
                                    publish_disposition:
                                        ProcessorPublishDispositionV1::NotApplicable,
                                    pending_snapshot_sequence: None,
                                    processor_error_reason: Some("MissingFirstFlashblock"),
                                    cache_resolved_final_disposition: cache_resolved.then_some(
                                        ProcessorBaseDispositionV1::MissingFirstUncacheable,
                                    ),
                                });
                            }
                            return;
                        }
                        _ => {}
                    }

                    if let (Some(recorder), Some(generation)) =
                        (EdgeMeasurementGlobal::installed(), source_generation)
                    {
                        let (disposition, reason) = processor_error_product(&e);
                        recorder.record_generation_product(ProcessorTerminalInputV1 {
                            source_generation: generation,
                            base_disposition: if cache_resolved {
                                ProcessorBaseDispositionV1::CacheResolvedToProcessor
                            } else {
                                disposition
                            },
                            observer_disposition: ProcessorObserverDispositionV1::Absent,
                            publish_disposition: ProcessorPublishDispositionV1::NotApplicable,
                            pending_snapshot_sequence: None,
                            processor_error_reason: Some(reason),
                            cache_resolved_final_disposition: cache_resolved.then_some(disposition),
                        });
                    }
                }
                #[cfg(not(feature = "edge-measurement"))]
                match e {
                    StateProcessorError::Provider(ProviderError::MissingCanonicalHeader {
                        ..
                    }) => {
                        if self.cache.lock().await.insert(flashblock) {
                            debug!(message = "cached flashblock pending canonical block", error = %e);
                            return;
                        }
                    }
                    StateProcessorError::MissingFirstFlashblock => {
                        let mut cache = self.cache.lock().await;
                        if flashblock.index > 0
                            && cache.has_flashblock(
                                flashblock.metadata.block_number,
                                flashblock.index - 1,
                            )
                            && cache.insert(flashblock)
                        {
                            return;
                        }
                        return;
                    }
                    _ => {}
                }

                if !matches!(
                    e,
                    StateProcessorError::Provider(ProviderError::MissingCanonicalHeader { .. })
                ) {
                    error!(message = "could not process Flashblock", error = %e);
                    Metrics::block_processing_error().increment(1);
                }
            }
        }
    }

    #[instrument(level = "debug", skip_all, fields(block_number = block.number))]
    fn process_canonical_block(
        &self,
        prev_pending_blocks: Option<Arc<PendingBlocks>>,
        block: &RecoveredBlock<BaseBlock>,
    ) -> Result<Option<Arc<PendingBlocks>>> {
        let pending_blocks = match &prev_pending_blocks {
            Some(pb) => pb,
            None => {
                debug!(message = "no pending state to update with canonical block, skipping");
                self.clear_live_state();
                return Ok(None);
            }
        };

        let mut flashblocks = pending_blocks.get_flashblocks();
        let num_flashblocks_for_canon =
            flashblocks.iter().filter(|fb| fb.metadata.block_number == block.number).count();
        Metrics::flashblocks_in_block().record(num_flashblocks_for_canon as f64);
        Metrics::pending_snapshot_height().set(pending_blocks.latest_block_number() as f64);

        // Check for reorg by comparing transaction sets
        let tracked_txns = pending_blocks.get_transactions_for_block(block.number);
        let tracked_txn_hashes: Vec<_> = tracked_txns.map(|tx| tx.tx_hash()).collect();
        let block_txn_hashes: Vec<_> = block.body().transactions().map(|tx| tx.tx_hash()).collect();

        let reorg_result = ReorgDetector::detect(&tracked_txn_hashes, &block_txn_hashes);
        let reorg_detected = reorg_result.is_reorg();

        // Determine the reconciliation strategy
        let strategy = CanonicalBlockReconciler::reconcile(
            Some(pending_blocks.earliest_block_number()),
            Some(pending_blocks.latest_block_number()),
            block.number,
            self.max_depth,
            reorg_detected,
        );

        match strategy {
            ReconciliationStrategy::CatchUp => {
                debug!(
                    message = "pending snapshot cleared because canonical caught up",
                    latest_pending_block = pending_blocks.latest_block_number(),
                    canonical_block = block.number,
                );
                Metrics::pending_clear_catchup().increment(1);
                Metrics::pending_snapshot_fb_index()
                    .set(pending_blocks.latest_flashblock_index() as f64);
                self.clear_live_state();
                Ok(None)
            }
            ReconciliationStrategy::HandleReorg => {
                warn!(
                    message = "reorg detected, recomputing pending flashblocks going ahead of reorg",
                    tracked_txn_hashes = ?tracked_txn_hashes,
                    block_txn_hashes = ?block_txn_hashes,
                );
                Metrics::pending_clear_reorg().increment(1);

                // If there is a reorg, we re-process all future flashblocks without reusing the existing pending state
                flashblocks.retain(|flashblock| flashblock.metadata.block_number > block.number);
                self.build_pending_state(None, &flashblocks)
            }
            ReconciliationStrategy::DepthLimitExceeded { depth, max_depth } => {
                debug!(
                    message = "pending blocks depth exceeds max depth, resetting pending blocks",
                    pending_blocks_depth = depth,
                    max_depth = max_depth,
                );

                flashblocks.retain(|flashblock| flashblock.metadata.block_number > block.number);
                self.build_pending_state(None, &flashblocks)
            }
            ReconciliationStrategy::Continue => {
                debug!(
                    message = "canonical block behind latest pending block, continuing with existing pending state",
                    latest_pending_block = pending_blocks.latest_block_number(),
                    earliest_pending_block = pending_blocks.earliest_block_number(),
                    canonical_block = block.number,
                    pending_txns_for_block = ?tracked_txn_hashes.len(),
                    canonical_txns_for_block = ?block_txn_hashes.len(),
                );
                // If no reorg, we can continue building on top of the existing pending state
                // NOTE: We do not retain specific flashblocks here to avoid losing track of our "earliest" pending block number
                self.build_pending_state(prev_pending_blocks, &flashblocks)
            }
            ReconciliationStrategy::NoPendingState => {
                // This case is already handled above, but included for completeness
                debug!(message = "no pending state to update with canonical block, skipping");
                self.clear_live_state();
                Ok(None)
            }
        }
    }

    #[instrument(
        level = "debug",
        skip_all,
        fields(
            block_number = flashblock.metadata.block_number,
            flashblock_index = flashblock.index
        )
    )]
    fn process_flashblock(
        &self,
        prev_pending_blocks: Option<Arc<PendingBlocks>>,
        flashblock: &Flashblock,
    ) -> Result<ProcessedFlashblock> {
        let pending_blocks = match &prev_pending_blocks {
            Some(pb) => pb,
            None => {
                if flashblock.index == 0 {
                    return measured!(
                        self.build_pending_state(None, std::slice::from_ref(flashblock))
                            .map(ProcessedFlashblock::advanced),
                        ProcessorBaseDispositionV1::AdvancedInitialBase
                    );
                }

                return Err(StateProcessorError::MissingFirstFlashblock);
            }
        };

        let validation_result = FlashblockSequenceValidator::validate(
            pending_blocks.latest_block_number(),
            pending_blocks.latest_flashblock_index(),
            flashblock.metadata.block_number,
            flashblock.index,
            flashblock.metadata.prev_flashblock_id,
        );

        match validation_result {
            SequenceValidationResult::NextInSequence => measured!(
                self.build_pending_state_for_same_block(pending_blocks, flashblock)
                    .map(ProcessedFlashblock::advanced),
                ProcessorBaseDispositionV1::AdvancedNextInSequence
            ),
            SequenceValidationResult::FirstOfNextBlock => measured!(
                self.build_pending_state_for_next_block(pending_blocks, flashblock)
                    .map(ProcessedFlashblock::advanced),
                ProcessorBaseDispositionV1::AdvancedFirstOfNextBlock
            ),
            SequenceValidationResult::Duplicate => {
                // We have received a duplicate flashblock for the current block
                Metrics::unexpected_block_order().increment(1);
                warn!(
                    message = "Received duplicate Flashblock for current block, ignoring",
                    curr_block = %pending_blocks.latest_block_number(),
                    flashblock_index = %flashblock.index,
                );
                measured!(
                    Ok(ProcessedFlashblock::unchanged(prev_pending_blocks)),
                    ProcessorBaseDispositionV1::UnchangedDuplicateExact
                )
            }
            SequenceValidationResult::InvalidNewBlockIndex { block_number, index: _ } => {
                // We have received a non-zero flashblock for a new block
                Metrics::unexpected_block_order().increment(1);
                error!(
                    message = "Received non-zero index Flashblock for new block, zeroing Flashblocks until we receive a base Flashblock",
                    curr_block = %pending_blocks.latest_block_number(),
                    new_block = %block_number,
                );
                self.clear_live_state();
                measured!(
                    Ok(ProcessedFlashblock::unchanged(None)),
                    ProcessorBaseDispositionV1::UnchangedInvalidNewBlockIndex
                )
            }
            SequenceValidationResult::NonSequentialGap { expected, actual } => {
                Metrics::unexpected_block_order().increment(1);
                error!(
                    curr_block = %pending_blocks.latest_block_number(),
                    expected_flashblock_index = %expected,
                    actual_flashblock_index = %actual,
                    "received non-sequential flashblock index for current block"
                );
                self.clear_live_state();
                measured!(
                    Ok(ProcessedFlashblock::unchanged(None)),
                    ProcessorBaseDispositionV1::UnchangedSequenceGap
                )
            }
            SequenceValidationResult::NonSequentialPredecessor { expected, actual } => {
                Metrics::unexpected_block_order().increment(1);
                error!(
                    curr_block = %pending_blocks.latest_block_number(),
                    curr_flashblock_index = %pending_blocks.latest_flashblock_index(),
                    new_block = %flashblock.metadata.block_number,
                    new_flashblock_index = %flashblock.index,
                    expected_prev_block = %expected.block_number,
                    expected_prev_index = %expected.index,
                    actual_prev_block = %actual.block_number,
                    actual_prev_index = %actual.index,
                    "received flashblock with non-sequential predecessor link"
                );
                self.clear_live_state();
                measured!(
                    Ok(ProcessedFlashblock::unchanged(None)),
                    ProcessorBaseDispositionV1::UnchangedPredecessorGap
                )
            }
        }
    }

    #[instrument(
        level = "debug",
        skip_all,
        fields(
            block_number = flashblock.metadata.block_number,
            flashblock_index = flashblock.index
        )
    )]
    fn build_pending_state_for_same_block(
        &self,
        prev_pending_blocks: &Arc<PendingBlocks>,
        flashblock: &Flashblock,
    ) -> Result<Option<Arc<PendingBlocks>>> {
        let latest_block_base = prev_pending_blocks.latest_block_base().clone();
        let latest_block_l1_block_info = prev_pending_blocks.latest_block_l1_block_info().clone();
        let latest_flashblock_tx_start = prev_pending_blocks.pending_transaction_count();

        let mut live_state = self.lock_live_state();
        let Some(LivePendingState { mut db, state_overrides }) = live_state.take() else {
            drop(live_state);
            warn!(
                message = "live pending state unavailable, falling back to full rebuild",
                block_number = flashblock.metadata.block_number,
                flashblock_index = flashblock.index,
                path = "same_block"
            );
            let mut flashblocks = prev_pending_blocks.get_flashblocks();
            flashblocks.push(flashblock.clone());
            return self.build_pending_state(Some(Arc::clone(prev_pending_blocks)), &flashblocks);
        };
        drop(live_state);

        let latest_header = prev_pending_blocks.latest_header();
        let mut latest_block_flashblocks = prev_pending_blocks.latest_block_flashblocks();
        latest_block_flashblocks.push(flashblock.clone());
        let latest_block_header =
            BlockAssembler::refresh_same_block_header(&latest_header, &latest_block_flashblocks)?;

        db.block_hashes.insert(latest_block_base.block_number - 1, latest_block_base.parent_hash);

        let evm_config = BaseEvmConfig::base(self.client.chain_spec());
        let evm_env = evm_config
            .evm_env(&latest_header)
            .map_err(|e| ExecutionError::EvmEnv(e.to_string()))?;
        let evm = evm_config.evm_with_env(db, evm_env);

        let previous_block_transaction_count = prev_pending_blocks.latest_block_transaction_count();
        let pending_block = Block {
            header: Header {
                parent_hash: latest_block_base.parent_hash,
                number: latest_block_base.block_number,
                timestamp: latest_block_base.timestamp,
                gas_limit: latest_block_base.gas_limit,
                base_fee_per_gas: Some(latest_block_base.base_fee_per_gas.saturating_to()),
                ..Default::default()
            },
            body: BlockBody {
                transactions: flashblock
                    .diff
                    .transactions
                    .iter()
                    .map(|tx| BaseTxEnvelope::decode_2718_exact(tx.as_ref()))
                    .collect::<std::result::Result<_, _>>()
                    .map_err(|e| ExecutionError::BlockConversion(e.to_string()))?,
                ..Default::default()
            },
        };
        let latest_block_transaction_count = prev_pending_blocks.latest_block_transaction_count()
            + pending_block.body.transactions.len();
        let recovery_start = Instant::now();
        let txs_with_senders: Vec<(BaseTxEnvelope, Address)> = pending_block
            .body
            .transactions
            .par_iter()
            .cloned()
            .map(|tx| -> Result<(BaseTxEnvelope, Address)> {
                let sender = tx.recover_signer()?;
                Ok((tx, sender))
            })
            .collect::<Result<_>>()?;
        let sender_recovery_elapsed = recovery_start.elapsed();
        Metrics::sender_recovery_duration().record(sender_recovery_elapsed);

        let mut pending_blocks_builder = PendingBlocksBuilder::from_previous(prev_pending_blocks);
        pending_blocks_builder.with_flashblocks([flashblock.clone()]);
        pending_blocks_builder.replace_latest_header(latest_block_header);

        let mut pending_state_builder = PendingStateBuilder::new(
            self.client.chain_spec(),
            evm,
            pending_block,
            None,
            latest_block_l1_block_info.clone(),
            state_overrides,
        );
        pending_state_builder.set_execution_offsets(
            prev_pending_blocks.latest_block_cumulative_gas_used(),
            prev_pending_blocks.latest_block_next_log_index(),
        );

        for (offset, (transaction, sender)) in txs_with_senders.into_iter().enumerate() {
            let tx_hash = transaction.tx_hash();
            let idx = previous_block_transaction_count + offset;

            pending_blocks_builder.with_transaction_sender(tx_hash, sender);
            pending_blocks_builder.increment_nonce(sender);

            let recovered_transaction = Recovered::new_unchecked(transaction, sender);
            let executed_transaction =
                pending_state_builder.execute_transaction(idx, recovered_transaction)?;

            if let Some(time_us) = executed_transaction.execution_time_us {
                pending_blocks_builder.with_execution_time(tx_hash, time_us);
            }

            for (address, account) in &executed_transaction.state {
                if account.is_touched() {
                    pending_blocks_builder.with_account_balance(*address, account.info.balance);
                }
            }

            pending_blocks_builder.with_transaction(executed_transaction.rpc_transaction);
            pending_blocks_builder.with_receipt(tx_hash, executed_transaction.receipt);
            pending_blocks_builder.with_transaction_state(tx_hash, executed_transaction.state);
            pending_blocks_builder.with_transaction_result(tx_hash, executed_transaction.result);
        }

        let latest_block_cumulative_gas_used = pending_state_builder.cumulative_gas_used();
        let latest_block_next_log_index = pending_state_builder.next_log_index();
        let (db, state_overrides) = pending_state_builder.into_db_and_state_overrides();
        pending_blocks_builder.with_latest_block_context(
            latest_flashblock_tx_start,
            latest_block_base,
            latest_block_l1_block_info,
            latest_block_transaction_count,
            latest_block_cumulative_gas_used,
            latest_block_next_log_index,
        );
        self.publish_pending_blocks(pending_blocks_builder, db, state_overrides)
    }

    #[instrument(
        level = "debug",
        skip_all,
        fields(
            block_number = flashblock.metadata.block_number,
            flashblock_index = flashblock.index
        )
    )]
    fn build_pending_state_for_next_block(
        &self,
        prev_pending_blocks: &Arc<PendingBlocks>,
        flashblock: &Flashblock,
    ) -> Result<Option<Arc<PendingBlocks>>> {
        let Some(base) = flashblock.base.clone() else {
            return Err(StateProcessorError::MissingFirstFlashblock);
        };

        let mut live_state = self.lock_live_state();
        let Some(LivePendingState { mut db, state_overrides }) = live_state.take() else {
            drop(live_state);
            warn!(
                message = "live pending state unavailable, falling back to full rebuild",
                block_number = flashblock.metadata.block_number,
                flashblock_index = flashblock.index,
                path = "next_block"
            );
            let mut flashblocks = prev_pending_blocks.get_flashblocks();
            flashblocks.push(flashblock.clone());
            return self.build_pending_state(Some(Arc::clone(prev_pending_blocks)), &flashblocks);
        };
        drop(live_state);

        let previous_header = prev_pending_blocks.latest_header();
        let current_block = BlockAssembler::assemble(std::slice::from_ref(flashblock))?;
        let l1_block_info = current_block.l1_block_info()?;
        let AssembledBlock { block: assembled_block, header: assembled_header, .. } = current_block;
        let pending_block = Block {
            header: Header {
                parent_hash: base.parent_hash,
                number: base.block_number,
                timestamp: base.timestamp,
                gas_limit: base.gas_limit,
                base_fee_per_gas: Some(base.base_fee_per_gas.saturating_to()),
                ..Default::default()
            },
            body: assembled_block.body,
        };

        db.block_hashes.insert(base.block_number - 1, base.parent_hash);

        let evm_config = BaseEvmConfig::base(self.client.chain_spec());
        let block_env_attributes = BaseNextBlockEnvAttributes {
            timestamp: base.timestamp,
            suggested_fee_recipient: base.fee_recipient,
            prev_randao: base.prev_randao,
            gas_limit: base.gas_limit,
            parent_beacon_block_root: Some(base.parent_beacon_block_root),
            extra_data: base.extra_data.clone(),
        };
        let evm_env = evm_config
            .next_evm_env(&previous_header, &block_env_attributes)
            .map_err(|e| ExecutionError::EvmEnv(e.to_string()))?;
        let evm = evm_config.evm_with_env(db, evm_env);

        let recovery_start = Instant::now();
        let txs_with_senders: Vec<(BaseTxEnvelope, Address)> = pending_block
            .body
            .transactions
            .par_iter()
            .cloned()
            .map(|tx| -> Result<(BaseTxEnvelope, Address)> {
                let sender = tx.recover_signer()?;
                Ok((tx, sender))
            })
            .collect::<Result<_>>()?;
        Metrics::sender_recovery_duration().record(recovery_start.elapsed());

        let mut pending_blocks_builder = PendingBlocksBuilder::from_previous(prev_pending_blocks);
        pending_blocks_builder.with_flashblocks([flashblock.clone()]);
        pending_blocks_builder.with_header(assembled_header);

        let mut pending_state_builder = PendingStateBuilder::new(
            self.client.chain_spec(),
            evm,
            pending_block,
            None,
            l1_block_info.clone(),
            state_overrides,
        );
        pending_state_builder
            .apply_pre_execution_changes(base.parent_hash, Some(base.parent_beacon_block_root))?;

        for (idx, (transaction, sender)) in txs_with_senders.into_iter().enumerate() {
            let tx_hash = transaction.tx_hash();

            pending_blocks_builder.with_transaction_sender(tx_hash, sender);
            pending_blocks_builder.increment_nonce(sender);

            let recovered_transaction = Recovered::new_unchecked(transaction, sender);
            let executed_transaction =
                pending_state_builder.execute_transaction(idx, recovered_transaction)?;

            if let Some(time_us) = executed_transaction.execution_time_us {
                pending_blocks_builder.with_execution_time(tx_hash, time_us);
            }

            for (address, account) in &executed_transaction.state {
                if account.is_touched() {
                    pending_blocks_builder.with_account_balance(*address, account.info.balance);
                }
            }

            pending_blocks_builder.with_transaction(executed_transaction.rpc_transaction);
            pending_blocks_builder.with_receipt(tx_hash, executed_transaction.receipt);
            pending_blocks_builder.with_transaction_state(tx_hash, executed_transaction.state);
            pending_blocks_builder.with_transaction_result(tx_hash, executed_transaction.result);
        }

        let latest_block_cumulative_gas_used = pending_state_builder.cumulative_gas_used();
        let latest_block_next_log_index = pending_state_builder.next_log_index();
        let (db, state_overrides) = pending_state_builder.into_db_and_state_overrides();
        pending_blocks_builder.with_latest_block_context(
            prev_pending_blocks.pending_transaction_count(),
            base,
            l1_block_info,
            flashblock.diff.transactions.len(),
            latest_block_cumulative_gas_used,
            latest_block_next_log_index,
        );

        self.publish_pending_blocks(pending_blocks_builder, db, state_overrides)
    }

    #[instrument(level = "debug", skip_all, fields(num_flashblocks = flashblocks.len()))]
    fn build_pending_state(
        &self,
        prev_pending_blocks: Option<Arc<PendingBlocks>>,
        flashblocks: &[Flashblock],
    ) -> Result<Option<Arc<PendingBlocks>>> {
        // BTreeMap guarantees ascending order of keys while iterating
        let mut flashblocks_per_block = BTreeMap::<BlockNumber, Vec<Flashblock>>::new();
        for flashblock in flashblocks {
            flashblocks_per_block
                .entry(flashblock.metadata.block_number)
                .or_default()
                .push(flashblock.clone());
        }

        let earliest_block_number = flashblocks_per_block.keys().min().unwrap();
        let canonical_block = earliest_block_number - 1;
        let mut last_block_header = self
            .client
            .header_by_number(canonical_block)
            .map_err(|e| ProviderError::StateProvider(e.to_string()))?
            .ok_or(ProviderError::MissingCanonicalHeader { block_number: canonical_block })?;

        let evm_config = BaseEvmConfig::base(self.client.chain_spec());
        let state_provider = self
            .client
            .state_by_block_number_or_tag(BlockNumberOrTag::Number(canonical_block))
            .map_err(|e| ProviderError::StateProvider(e.to_string()))?;
        let state_provider_db = StateProviderDatabase::new(state_provider);
        let mut pending_blocks_builder = PendingBlocksBuilder::new();

        // Track state changes across flashblocks, accumulating bundle state
        // from previous pending blocks if available.
        let mut db = State::builder().with_database(state_provider_db).with_bundle_update().build();

        let mut state_overrides =
            prev_pending_blocks.as_ref().map_or_else(StateOverride::default, |pending_blocks| {
                pending_blocks.get_state_overrides().unwrap_or_default()
            });

        let mut total_transaction_count = 0usize;
        for (_block_number, flashblocks) in flashblocks_per_block {
            // Use BlockAssembler to reconstruct the block from flashblocks
            let assembled = BlockAssembler::assemble(&flashblocks)?;
            let latest_flashblock_tx_count =
                flashblocks.last().map(|latest| latest.diff.transactions.len()).unwrap_or_default();
            let latest_block_base = assembled.base.clone();

            pending_blocks_builder.with_flashblocks(assembled.flashblocks.clone());
            pending_blocks_builder.with_header(assembled.header.clone());

            // Extract L1 block info using the AssembledBlock method
            let l1_block_info = assembled.l1_block_info()?;
            let latest_block_l1_block_info = l1_block_info.clone();
            let latest_block_transaction_count = assembled.block.body.transactions.len();

            let block_env_attributes = BaseNextBlockEnvAttributes {
                timestamp: assembled.base.timestamp,
                suggested_fee_recipient: assembled.base.fee_recipient,
                prev_randao: assembled.base.prev_randao,
                gas_limit: assembled.base.gas_limit,
                parent_beacon_block_root: Some(assembled.base.parent_beacon_block_root),
                extra_data: assembled.base.extra_data.clone(),
            };

            db.block_hashes
                .insert(latest_block_base.block_number - 1, latest_block_base.parent_hash);

            let evm_env = evm_config
                .next_evm_env(&last_block_header, &block_env_attributes)
                .map_err(|e| ExecutionError::EvmEnv(e.to_string()))?;
            let evm = evm_config.evm_with_env(db, evm_env);

            // Parallel sender recovery - batch all ECDSA operations upfront
            let recovery_start = Instant::now();
            let txs_with_senders: Vec<(BaseTxEnvelope, Address)> = assembled
                .block
                .body
                .transactions
                .par_iter()
                .cloned()
                .map(|tx| -> Result<(BaseTxEnvelope, Address)> {
                    let tx_hash = tx.tx_hash();
                    let sender = match prev_pending_blocks
                        .as_ref()
                        .and_then(|p| p.get_transaction_sender(&tx_hash))
                    {
                        Some(cached) => cached,
                        None => tx.recover_signer()?,
                    };
                    Ok((tx, sender))
                })
                .collect::<Result<_>>()?;
            Metrics::sender_recovery_duration().record(recovery_start.elapsed());

            // Clone header before moving block to avoid cloning the entire block
            let block_header = assembled.block.header.clone();

            let parent_block_hash = assembled.base.parent_hash;
            let parent_beacon_block_root = Some(assembled.base.parent_beacon_block_root);

            let mut pending_state_builder = PendingStateBuilder::new(
                self.client.chain_spec(),
                evm,
                assembled.block,
                prev_pending_blocks.clone(),
                l1_block_info,
                state_overrides,
            );

            pending_state_builder
                .apply_pre_execution_changes(parent_block_hash, parent_beacon_block_root)?;

            for (idx, (transaction, sender)) in txs_with_senders.into_iter().enumerate() {
                let tx_hash = transaction.tx_hash();

                pending_blocks_builder.with_transaction_sender(tx_hash, sender);
                pending_blocks_builder.increment_nonce(sender);

                let recovered_transaction = Recovered::new_unchecked(transaction, sender);

                let executed_transaction =
                    pending_state_builder.execute_transaction(idx, recovered_transaction)?;

                if let Some(time_us) = executed_transaction.execution_time_us {
                    pending_blocks_builder.with_execution_time(tx_hash, time_us);
                }

                for (address, account) in &executed_transaction.state {
                    if account.is_touched() {
                        pending_blocks_builder.with_account_balance(*address, account.info.balance);
                    }
                }

                pending_blocks_builder.with_transaction(executed_transaction.rpc_transaction);
                pending_blocks_builder.with_receipt(tx_hash, executed_transaction.receipt);
                pending_blocks_builder.with_transaction_state(tx_hash, executed_transaction.state);
                pending_blocks_builder
                    .with_transaction_result(tx_hash, executed_transaction.result);
            }

            let latest_flashblock_tx_start = total_transaction_count
                .saturating_add(latest_block_transaction_count)
                .saturating_sub(latest_flashblock_tx_count);
            pending_blocks_builder.with_latest_block_context(
                latest_flashblock_tx_start,
                latest_block_base,
                latest_block_l1_block_info,
                latest_block_transaction_count,
                pending_state_builder.cumulative_gas_used(),
                pending_state_builder.next_log_index(),
            );
            total_transaction_count += latest_block_transaction_count;

            (db, state_overrides) = pending_state_builder.into_db_and_state_overrides();
            last_block_header = block_header;
        }

        self.publish_pending_blocks(pending_blocks_builder, db, state_overrides)
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the runtime-flavor contract that `start()` and
    //! `apply_flashblock()` rely on when they wrap the blocking MDBX read-tx +
    //! EVM rebuild in `tokio::task::block_in_place`.
    //!
    //! A full behavior-parity test would require constructing a
    //! `StateProcessor<Client>` over a mock `StateProviderFactory` plus EVM
    //! state, for which this crate has no fixtures. Instead we lock in the
    //! single safety precondition the wrap depends on as an executable
    //! assertion: `block_in_place` is sound on a `multi_thread` runtime (the
    //! node runtime, per `CliRunner::try_default_runtime` -> `new_multi_thread`)
    //! and panics on a `current_thread` runtime. This documents — and CI-pins —
    //! the
    //! invariant so a future change to the runtime flavor (or a `#[tokio::test]`
    //! default-`current_thread` harness driving these fns) fails loudly here.

    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use alloy_consensus::{Header, Sealed};
    use alloy_primitives::{Address, B256, Bloom, Bytes, U256};
    use alloy_rpc_types_engine::PayloadId;
    use base_common_flashblocks::{
        ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, Metadata,
    };

    use super::{
        ProcessedFlashblock, notify_pending_frame_observer, notify_processed_pending_frame_observer,
    };
    use crate::{PendingBlocks, PendingBlocksBuilder, PendingFrameObserver};

    #[derive(Debug)]
    struct CountingObserver {
        calls: Arc<AtomicUsize>,
    }

    impl PendingFrameObserver for CountingObserver {
        fn on_pending_frame(&self, _pending: &PendingBlocks) {
            self.calls.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[derive(Debug)]
    struct PanicObserver;

    impl PendingFrameObserver for PanicObserver {
        fn on_pending_frame(&self, _pending: &PendingBlocks) {
            panic!("observer panic should be isolated");
        }
    }

    fn test_pending_blocks() -> PendingBlocks {
        let flashblock = Flashblock {
            payload_id: PayloadId::default(),
            index: 0,
            base: Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: B256::ZERO,
                parent_hash: B256::ZERO,
                fee_recipient: Address::ZERO,
                prev_randao: B256::ZERO,
                block_number: 1,
                gas_limit: 30_000_000,
                timestamp: 1_700_000_000,
                extra_data: Bytes::default(),
                base_fee_per_gas: U256::from(1_000_000_000u64),
            }),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::default(),
                gas_used: 0,
                block_hash: B256::ZERO,
                transactions: vec![],
                withdrawals: vec![],
                withdrawals_root: B256::ZERO,
                blob_gas_used: None,
            },
            metadata: Metadata::new(1),
        };
        let mut builder = PendingBlocksBuilder::new();
        builder.with_flashblocks([flashblock]);
        builder.with_header(Sealed::new_unchecked(Header::default(), B256::ZERO));
        builder.build().expect("pending blocks should build")
    }

    #[test]
    fn pending_frame_observer_invokes_and_isolates_panic() {
        let pending_blocks = test_pending_blocks();
        let calls = Arc::new(AtomicUsize::new(0));
        notify_pending_frame_observer(
            Some(Arc::new(CountingObserver { calls: Arc::clone(&calls) })),
            &pending_blocks,
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        notify_pending_frame_observer(Some(Arc::new(PanicObserver)), &pending_blocks);
        notify_pending_frame_observer(None, &pending_blocks);
    }

    #[test]
    fn duplicate_processed_flashblock_does_not_notify_observer() {
        let pending_blocks = Arc::new(test_pending_blocks());
        let calls = Arc::new(AtomicUsize::new(0));

        notify_processed_pending_frame_observer(
            Some(Arc::new(CountingObserver { calls: Arc::clone(&calls) })),
            &ProcessedFlashblock::advanced(Some(Arc::clone(&pending_blocks))),
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        notify_processed_pending_frame_observer(
            Some(Arc::new(CountingObserver { calls: Arc::clone(&calls) })),
            &ProcessedFlashblock::unchanged(Some(pending_blocks)),
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    /// Mirrors the wrap at the two call sites: `block_in_place` returning a
    /// value, borrowing on-thread. Under `multi_thread` it runs the closure and
    /// returns its result without panicking.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn block_in_place_is_sound_on_multi_thread_runtime() {
        let owned = String::from("prev_pending_blocks");
        let borrowed = 42u64;
        let out = tokio::task::block_in_place(|| {
            // Capture an owned value by move and a borrow by ref, exactly like
            // the real wrap captures `prev_pending_blocks` (owned) and
            // `&block`/`&flashblock` (borrowed).
            format!("{owned}-{borrowed}")
        });
        assert_eq!(out, "prev_pending_blocks-42");
    }

    /// Documents the failure mode the wrap must never be exposed to: calling
    /// `block_in_place` from a `current_thread` runtime panics. This is why the
    /// node must stay on `new_multi_thread` and why any test that drives
    /// `start()`/`apply_flashblock()` directly must use
    /// `#[tokio::test(flavor = "multi_thread")]`.
    #[tokio::test]
    #[should_panic]
    async fn block_in_place_panics_on_current_thread_runtime() {
        // `#[tokio::test]` defaults to a current_thread runtime.
        tokio::task::block_in_place(|| {});
    }
}

#[cfg(all(test, feature = "edge-measurement"))]
mod measurement_tests {
    use super::*;

    #[test]
    fn every_processor_error_family_has_an_exact_named_disposition() {
        let cases = [
            (
                StateProcessorError::Protocol(ProtocolError::InvalidSequence),
                ProcessorBaseDispositionV1::ProcessErrorProtocol,
                "InvalidSequence",
            ),
            (
                StateProcessorError::Provider(ProviderError::StateProvider("fault".into())),
                ProcessorBaseDispositionV1::ProcessErrorProvider,
                "StateProvider",
            ),
            (
                StateProcessorError::Execution(ExecutionError::GasOverflow),
                ProcessorBaseDispositionV1::ProcessErrorExecution,
                "GasOverflow",
            ),
            (
                StateProcessorError::Build(BuildError::NoFlashblocks),
                ProcessorBaseDispositionV1::ProcessErrorBuild,
                "NoFlashblocks",
            ),
            (
                StateProcessorError::MissingFirstFlashblock,
                ProcessorBaseDispositionV1::MissingFirstUncacheable,
                "MissingFirstFlashblock",
            ),
        ];

        for (error, expected_disposition, expected_reason) in cases {
            assert_eq!(processor_error_product(&error), (expected_disposition, expected_reason));
        }
    }

    #[test]
    fn processor_error_reason_inventory_is_ungrouped() {
        let errors = [
            StateProcessorError::Protocol(ProtocolError::MissingBase),
            StateProcessorError::Protocol(ProtocolError::EmptyFlashblocks),
            StateProcessorError::Execution(ExecutionError::SenderRecovery("fault".into())),
            StateProcessorError::Execution(ExecutionError::DepositReceiptMismatch),
            StateProcessorError::Execution(ExecutionError::EvmEnv("fault".into())),
            StateProcessorError::Execution(ExecutionError::L1BlockInfo("fault".into())),
            StateProcessorError::Execution(ExecutionError::BlockConversion("fault".into())),
            StateProcessorError::Execution(ExecutionError::DepositAccountLoad),
            StateProcessorError::Execution(ExecutionError::RpcReceiptBuild("fault".into())),
            StateProcessorError::Execution(ExecutionError::DaFootprintEstimation("fault".into())),
            StateProcessorError::Build(BuildError::MissingHeaders),
        ];
        let reasons: Vec<_> = errors.iter().map(|error| processor_error_product(error).1).collect();
        assert_eq!(
            reasons,
            [
                "MissingBase",
                "EmptyFlashblocks",
                "SenderRecovery",
                "DepositReceiptMismatch",
                "EvmEnv",
                "L1BlockInfo",
                "BlockConversion",
                "DepositAccountLoad",
                "RpcReceiptBuild",
                "DaFootprintEstimation",
                "MissingHeaders",
            ]
        );
    }
}
