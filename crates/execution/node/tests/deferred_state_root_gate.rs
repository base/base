//! Publication feasibility tests against the pinned, unmodified reth validator.
//!
//! The scripted provider is used only to isolate publication from MPT correctness. These tests
//! do not certify deferred commitments. The wrapper is a test prototype, not node wiring.

use std::{
    io,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use alloy_consensus::{BlockBody, Header, EMPTY_OMMER_ROOT_HASH, EMPTY_ROOT_HASH};
use alloy_primitives::{B256, Bytes};
use base_common_consensus::{BaseBlock, BasePrimitives, BaseTxEnvelope};
use base_common_rpc_types_engine::ExecutionData;
use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
use base_execution_consensus::BaseBeaconConsensus;
use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::{BasePayloadBuilderAttributes, BasePayloadTypes};
use reth_chainspec::EthChainSpec;
use reth_chain_state::{CanonicalInMemoryState, ExecutedBlock};
use reth_engine_tree::{
    chain::FromOrchestrator,
    engine::{EngineApiKind, EngineApiRequest, FromEngine},
    persistence::PersistenceHandle,
    tree::{
        BasicEngineValidator, CacheWaitDurations, EngineApiTreeHandler, EngineApiTreeState,
        EngineValidator, TreeConfig, ValidationOutcome, ValidationOutput, WaitForCaches,
        error::InsertBlockError,
        payload_validator::TreeCtx,
        state_root_strategy::{
            LazyHashedPostState, PreparedStateRootJob, StateRootJob, StateRootJobContext,
            StateRootJobOutcome, StateRootStrategy,
        },
    },
};
use reth_network_p2p::full_block::SealedBlockWithAccessList;
use reth_node_api::{BeaconEngineMessage, NoopInvalidBlockHook};
use reth_payload_builder::{PayloadBuilderHandle, PayloadBuilderResources};
use reth_payload_primitives::{
    BuiltPayloadExecutedBlock, InvalidPayloadAttributesError, NewPayloadError, PayloadTypes,
};
use reth_primitives_traits::{Block, RecoveredBlock, SealedBlock, SealedHeader};
use reth_provider::{
    BlockExecutionOutput, ProviderError, StateRootProvider, test_utils::MockEthProvider,
};
use reth_storage_api::errors::ProviderResult;
use reth_storage_overlay::OverlayManager;
use reth_tasks::Runtime;
use reth_trie_common::{HashedPostState, LazyTrieData, SortedTrieData, updates::TrieUpdates};
use tokio::sync::{mpsc::unbounded_channel, oneshot};

use base_node_core::engine::BaseEngineValidator;

/// Provider used by the publication-only fixture.
pub type Provider = MockEthProvider<BasePrimitives, BaseChainSpec>;
/// The real validator wrapped by the publication test.
pub type Inner = BasicEngineValidator<Provider, BaseEvmConfig, BaseEngineValidator<BaseTxEnvelope, BaseChainSpec>>;
/// Finalized inputs staged privately until all inner validation checks succeed.
pub type Staged = Arc<Mutex<Option<(B256, Arc<BlockExecutionOutput<<BasePrimitives as reth_primitives_traits::NodePrimitives>::Receipt>>, SortedTrieData)>>>;

/// A same-block serial job that stages actual finalized trie inputs for the wrapper.
#[derive(Debug)]
pub struct GateStrategy {
    /// Scripted state-root provider for the publication-only fixture.
    pub provider: Provider,
    /// Private finalized inputs awaiting all inner checks.
    pub staged: Staged,
}

impl StateRootStrategy<BasePrimitives, Provider, BaseEvmConfig> for GateStrategy {
    /// Prepares the test's serial root job without sparse sharing.
    fn prepare(&self, _ctx: StateRootJobContext<'_, BasePrimitives, Provider, BaseEvmConfig>) -> ProviderResult<PreparedStateRootJob<BasePrimitives>> {
        Ok(PreparedStateRootJob::new(Box::new(Self {
            provider: self.provider.clone(), staged: self.staged.clone(),
        }), None))
    }
}

impl StateRootJob<BasePrimitives> for GateStrategy {
    /// Names the test job distinctly from the deferred-commitment implementation.
    fn name(&self) -> &'static str { "base-publication-gate" }

    /// Computes a same-block root and stages fully sorted data from finalized execution.
    fn finish(&mut self, block: &RecoveredBlock<BaseBlock>, output: Arc<BlockExecutionOutput<<BasePrimitives as reth_primitives_traits::NodePrimitives>::Receipt>>, hashed: &LazyHashedPostState) -> ProviderResult<StateRootJobOutcome> {
        let hashed = hashed.get();
        let (root, updates) = self.provider.state_root_with_updates((**hashed).clone())?;
        let sorted = SortedTrieData::new(Arc::new(hashed.clone_into_sorted()), Arc::new(updates.clone_into_sorted()));
        *self.staged.lock().unwrap() = Some((block.hash(), output, sorted));
        Ok(StateRootJobOutcome::new(root, Arc::new(updates)))
    }
}

/// Publication failures injected after the real inner validator succeeds.
#[derive(Clone, Copy, Debug)]
pub enum PublicationFault {
    /// Publish the fully sorted result.
    None,
    /// Abandon the sender before publishing.
    Drop,
    /// Panic while owning the sender.
    Panic,
    /// Hold the sender until the bounded wait expires.
    Timeout,
}

/// Test wrapper exercising the public replacement API, with an abandoned inner producer.
#[derive(Debug)]
pub struct GateValidator {
    /// Failure injected into the owned publication producer.
    pub fault: PublicationFault,
    /// Unmodified BasicEngineValidator executing the block.
    pub inner: Inner,
    /// Private finalized inputs awaiting all inner checks.
    pub staged: Staged,
    /// Successful outputs observed before admission to the tree.
    pub observed: mpsc::Sender<ValidationOutput<BasePrimitives>>,
}

impl GateValidator {
    /// Replaces the inner handle without observing it, retaining original Arc identities.
    pub fn publish(&self, outcome: ValidationOutcome<BasePrimitives>) -> ValidationOutcome<BasePrimitives> {
        let mut output = match outcome {
            Ok(output) => output,
            Err(error) => {
                self.staged.lock().unwrap().take();
                return Err(error);
            }
        };
        let (hash, execution, sorted) = self.staged.lock().unwrap().take().ok_or_else(|| {
            InsertBlockError::new(output.executed_block.sealed_block().clone(), ProviderError::other(io::Error::other("missing staged trie data")).into())
        })?;
        assert_eq!(hash, output.executed_block.recovered_block.hash(), "staged trie data must belong to this sealed hash");
        assert!(Arc::ptr_eq(&execution, &output.executed_block.execution_output), "the job and validator must share finalized execution output");
        let (tx, rx) = mpsc::channel();
        let (cancel, cancelled) = mpsc::channel();
        let fault = self.fault;
        let worker = std::thread::spawn(move || {
            match fault {
                PublicationFault::None => { let _ = tx.send(sorted); }
                PublicationFault::Drop => drop(tx),
                PublicationFault::Panic => panic!("injected publication producer panic"),
                PublicationFault::Timeout => {
                    let _ = cancelled.recv_timeout(Duration::from_secs(1));
                    drop(tx);
                }
            }
        });
        let completion = rx.recv_timeout(Duration::from_millis(100));
        let _ = cancel.send(());
        let joined = worker.join();
        let sorted = completion.map_err(|error| {
            assert!(!matches!(fault, PublicationFault::None), "ordinary publication must not fail: {error}");
            if matches!(fault, PublicationFault::Panic) {
                assert!(joined.is_err(), "panic injection must really panic in the producer");
            } else {
                assert!(joined.is_ok(), "drop and timeout producers must drain without panic");
            }
            InsertBlockError::new(output.executed_block.sealed_block().clone(), ProviderError::other(io::Error::other(error)).into())
        })?;
        let recovered = output.executed_block.recovered_block.clone();
        let execution = output.executed_block.execution_output.clone();
        let bal = output.executed_block.bal.clone().expect("fixture must exercise BAL preservation");
        let timing = output.execution_timing_stats.as_deref().expect("fixture must collect timing stats") as *const _;

        // Deliberately abandon a pending handle. Reading it would never complete. This does not
        // cancel the redundant stock sorting task; the test runtime owns and drains that task.
        let (pending, producer) = LazyTrieData::pending(Arc::new(HashedPostState::default()), Arc::new(TrieUpdates::default()));
        drop(producer);
        output.executed_block.trie_data = pending;
        output.executed_block = ExecutedBlock::new(recovered.clone(), execution.clone(), sorted.clone()).with_bal(Some(bal.clone()));
        assert!(Arc::ptr_eq(&recovered, &output.executed_block.recovered_block), "replacement must retain recovered-block Arc");
        assert!(Arc::ptr_eq(&execution, &output.executed_block.execution_output), "replacement must retain output Arc");
        assert!(Arc::ptr_eq(&bal, output.executed_block.bal().unwrap()), "replacement must retain BAL Arc");
        assert_eq!(timing, output.execution_timing_stats.as_deref().unwrap() as *const _, "replacement must retain timing allocation");
        assert_eq!(output.executed_block.trie_data(), sorted, "publication must contain exact staged sorted inputs");
        self.observed.send(output.clone()).expect("test observer remains alive");
        Ok(output)
    }
}

impl EngineValidator<BasePayloadTypes> for GateValidator {
    /// Delegates attribute validation unchanged.
    fn validate_payload_attributes_against_header(&self, attributes: &BasePayloadBuilderAttributes<BaseTxEnvelope>, header: &Header) -> Result<(), InvalidPayloadAttributesError> {
        <Inner as EngineValidator<BasePayloadTypes>>::validate_payload_attributes_against_header(&self.inner, attributes, header)
    }
    /// Delegates layout and hash validation unchanged.
    fn convert_payload_to_block(&self, payload: ExecutionData) -> Result<SealedBlock<BaseBlock>, NewPayloadError> {
        <Inner as EngineValidator<BasePayloadTypes>>::convert_payload_to_block(&self.inner, payload)
    }
    /// Publishes only a successfully validated output.
    fn validate_payload(&mut self, payload: ExecutionData, ctx: TreeCtx<'_, BasePrimitives>) -> ValidationOutcome<BasePrimitives> {
        let result = <Inner as EngineValidator<BasePayloadTypes>>::validate_payload(&mut self.inner, payload, ctx);
        self.publish(result)
    }
    /// Publishes downloaded blocks through the same gate.
    fn validate_block(&mut self, block: SealedBlockWithAccessList<BaseBlock>, ctx: TreeCtx<'_, BasePrimitives>) -> ValidationOutcome<BasePrimitives> {
        let result = <Inner as EngineValidator<BasePayloadTypes>>::validate_block(&mut self.inner, block, ctx);
        self.publish(result)
    }
    /// Refuses the trusted insertion bypass for this test cohort.
    fn on_inserted_executed_block(&self, _block: BuiltPayloadExecutedBlock<BasePrimitives>) -> ProviderResult<ExecutedBlock<BasePrimitives>> {
        Err(ProviderError::other(io::Error::other("trusted insertion disabled in publication gate")))
    }
    /// Preserves canonical cache notifications.
    fn on_canonical_head_changed(&self, hash: B256, state: &EngineApiTreeState<BasePrimitives>) {
        <Inner as EngineValidator<BasePayloadTypes>>::on_canonical_head_changed(&self.inner, hash, state);
    }
    /// Preserves payload-builder resource behavior.
    fn payload_builder_resources(&self, parent: B256, header: &Header, timestamp: u64, state: &mut EngineApiTreeState<BasePrimitives>) -> PayloadBuilderResources {
        <Inner as EngineValidator<BasePayloadTypes>>::payload_builder_resources(&self.inner, parent, header, timestamp, state)
    }
}

impl WaitForCaches for GateValidator {
    /// Preserves both cache wait durations.
    fn wait_for_caches(&self) -> CacheWaitDurations { self.inner.wait_for_caches() }
}

/// Runs the real engine with a scripted publication failure or a ready result.
pub fn run_publication_gate(fault: PublicationFault) {
    let runtime = Runtime::test();
    let chain = Arc::new(BaseChainSpecBuilder::default()
        .chain(BaseChainSpec::mainnet().chain)
        .genesis(serde_json::from_str(include_str!("assets/genesis.json")).unwrap())
        .bedrock_activated().build());
    let provider = MockEthProvider::<BasePrimitives>::new().with_chain_spec((*chain).clone());
    provider.enable_database_provider();
    let parent = SealedHeader::seal_slow(chain.genesis_header().clone());
    let base_fee = chain.next_block_base_fee(parent.header(), 2).expect("fixture has London base fee");
    provider.add_header(parent.hash(), parent.header().clone());
    let overlay = OverlayManager::new(runtime.state_trie_overlay_worker_pool());
    let staged = Staged::default();
    let (observed, result) = mpsc::channel();
    let config = TreeConfig::default().with_slow_block_threshold(Some(Duration::ZERO));
    let consensus = Arc::new(BaseBeaconConsensus::new(chain.clone()));
    let evm = BaseEvmConfig::new(chain.clone());
    let inner = BasicEngineValidator::new(provider.clone(), consensus.clone(), evm.clone(),
        BaseEngineValidator::<BaseTxEnvelope, _>::new::<reth_trie_common::KeccakKeyHasher>(chain),
        config.clone(), Box::new(NoopInvalidBlockHook::default()), overlay.clone(), runtime.clone())
        .with_state_root_strategy(Arc::new(GateStrategy { provider: provider.clone(), staged: staged.clone() }));
    let wrapper = GateValidator { inner, staged, observed, fault };
    let (persistence, _persistence_rx) = mpsc::channel();
    let (payloads, _payload_rx) = unbounded_channel();
    let (sender, _events) = EngineApiTreeHandler::spawn_new(provider, consensus, wrapper,
        PersistenceHandle::new(persistence), PayloadBuilderHandle::<BasePayloadTypes>::new(payloads),
        CanonicalInMemoryState::with_head(parent.clone(), None, None), overlay, config,
        EngineApiKind::OpStack, evm, runtime.clone());
    let block = BaseBlock::new(Header {
        parent_hash: parent.hash(), number: 1, timestamp: 2, gas_limit: 30_000_000,
        base_fee_per_gas: Some(base_fee), state_root: B256::ZERO,
        ommers_hash: EMPTY_OMMER_ROOT_HASH, transactions_root: EMPTY_ROOT_HASH,
        receipts_root: EMPTY_ROOT_HASH,
        ..Default::default()
    }, BlockBody::default()).seal_slow();
    let payload = BasePayloadTypes::<BasePrimitives>::block_to_payload(block.clone(), Some(Bytes::from_static(&[0xc0])));
    let (tx, rx) = oneshot::channel();
    sender.send(FromEngine::Request(EngineApiRequest::Beacon(BeaconEngineMessage::NewPayload {
        cause: tracing::Span::none(), payload, tx,
    }))).unwrap();
    let status = runtime.handle().block_on(async {
        tokio::time::timeout(Duration::from_secs(10), rx).await.expect("engine gate must be bounded").unwrap()
    });
    if matches!(fault, PublicationFault::None) {
        let status = status.expect("same-block plumbing fixture must pass real engine validation");
        assert_eq!(status.status, alloy_rpc_types_engine::PayloadStatusEnum::Valid, "engine must accept the ready fixture");
        assert_eq!(status.latest_valid_hash, Some(block.hash()), "engine must identify the exact accepted hash");
        let output = result.recv_timeout(Duration::from_secs(1)).expect("wrapper must publish a ready output");
        assert_eq!(output.executed_block.recovered_block.hash(), block.hash(), "wrapper must not change block identity");
        assert!(output.executed_block.bal().is_some(), "BAL must survive real engine execution");
        drop(output);
    } else {
        assert!(status.is_err(), "local publication failure must be an operational error, never INVALID or VALID: {status:?}");
        assert!(result.try_recv().is_err(), "failed publication must expose zero successful outputs");
    }
    // Explicit termination owns the engine thread in both success and operational-failure tests.
    // An operational response alone is not evidence of the future node's unhealthy/stop policy.
    let (tx, rx) = oneshot::channel();
    sender.send(FromEngine::Event(FromOrchestrator::Terminate { tx })).unwrap();
    runtime.handle().block_on(async {
        tokio::time::timeout(Duration::from_secs(10), rx).await.expect("engine must terminate within the deadline").unwrap();
    });
    drop(sender);
    runtime.shutdown_timeout(Duration::from_secs(10));
}

/// Ready replacement preserves execution, recovered-block and BAL Arcs plus timing allocation.
#[test]
fn basic_validator_ready_publication_gate() { run_publication_gate(PublicationFault::None); }

/// An abandoned producer cannot publish an executed block or leave validation blocked.
#[test]
fn basic_validator_publication_drop_is_bounded() { run_publication_gate(PublicationFault::Drop); }

/// A producer panic cannot become a successful validation result.
#[test]
fn basic_validator_publication_panic_is_bounded() { run_publication_gate(PublicationFault::Panic); }

/// A stalled producer is cancelled and drained after the publication deadline.
#[test]
fn basic_validator_publication_timeout_is_bounded() { run_publication_gate(PublicationFault::Timeout); }
