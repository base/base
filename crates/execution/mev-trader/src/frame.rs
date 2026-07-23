use std::{collections::BTreeSet, sync::Arc, time::Instant};

use alloy_consensus::{
    Transaction,
    transaction::{Recovered, SignerRecoverable},
};
use alloy_eips::{Decodable2718, Typed2718};
use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_rpc_types_engine::PayloadId;
use base_common_consensus::BaseTxEnvelope;
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::BaseEvmConfig;
use reth_evm::{ConfigureEvm, Evm};
use reth_revm::{State, database::StateProviderDatabase};
use revm::{DatabaseCommit, context_interface::result::ExecutionResult};

use crate::{
    CancellationProbe, DeltaGuard, FrameAuditPlan, MaterializedState, MeasurementContext,
    PayloadVisitor, PortError, SnapshotHandle, StateMaterializer, TraderSnapshotPort, VisitControl,
};

/// Maximum age accepted for an ingress victim frame.
pub const MAX_FRAME_AGE_MILLIS: u64 = 250;
/// Maximum raw frame size accepted before decoding.
pub const MAX_RAW_FRAME_BYTES: usize = 128 * 1024;

/// Decoded-frame ingress record bound to one pending generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VictimFrame {
    /// Transaction chain identifier.
    pub chain_id: u64,
    /// EIP-2718 transaction type.
    pub transaction_type: u8,
    /// Expected transaction hash.
    pub transaction_hash: B256,
    /// Recovered transaction sender.
    pub from: Address,
    /// Exact received transaction bytes.
    pub raw_tx: Bytes,
    /// Captured canonical parent hash.
    pub parent_hash: B256,
    /// Feed-observed pending block number.
    pub block_number: u64,
    /// Feed-observed flashblock index containing this victim.
    pub victim_flashblock_index: u64,
    /// Time at which this decoded frame was received.
    pub received_at: Instant,
}

/// Canonical strict-ascending set of pools whose quote state changed in the victim delta.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DirtyPoolSet(Vec<Address>);

impl DirtyPoolSet {
    /// Returns the canonical pool-address slice.
    pub fn as_slice(&self) -> &[Address] {
        &self.0
    }

    /// Returns the number of dirty pools.
    pub const fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns whether no quote-bearing pool changed.
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Typed victim-delta validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaError {
    /// The audit plan or actual changed key set failed the existing delta guard.
    DeltaNotAudited,
    /// A classified quote slot referred to a pool outside the immutable universe.
    DirtyPoolOutsideUniverse,
}

/// Validated classification of the actual victim state delta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedFrameDelta {
    dirty_pools: DirtyPoolSet,
}

impl ValidatedFrameDelta {
    /// Validates all actual changes and derives only quote-slot-intersecting dirty pools.
    pub fn validate_and_classify(
        state: &revm::state::EvmState,
        audit: &FrameAuditPlan,
    ) -> Result<Self, DeltaError> {
        if !DeltaGuard::permits(state, audit.audited_writes()) {
            return Err(DeltaError::DeltaNotAudited);
        }

        let mut dirty = BTreeSet::new();
        for (address, account) in state {
            for (slot, _) in account.changed_storage_slots() {
                if let Some(owner) = audit.owner_for_storage(*address, *slot) {
                    if !audit.contains_pool(owner) {
                        return Err(DeltaError::DirtyPoolOutsideUniverse);
                    }
                    dirty.insert(owner);
                }
            }
        }
        Ok(Self { dirty_pools: DirtyPoolSet(dirty.into_iter().collect()) })
    }

    /// Returns the canonical dirty-pool set.
    pub const fn dirty_pools(&self) -> &DirtyPoolSet {
        &self.dirty_pools
    }
}

impl DeltaGuard {
    /// Preserves delta authorization parity and classifies actual quote-slot changes.
    pub fn validate_and_classify(
        state: &revm::state::EvmState,
        audit: &FrameAuditPlan,
    ) -> Result<ValidatedFrameDelta, DeltaError> {
        ValidatedFrameDelta::validate_and_classify(state, audit)
    }
}

/// One-shot guard that makes the sole post-validation database commit explicit.
#[derive(Debug, Default)]
pub struct FrameCommitGuard {
    commits: u8,
}

impl FrameCommitGuard {
    /// Commits the validated state only when no prior commit has occurred.
    pub fn commit<Database>(
        &mut self,
        database: &mut Database,
        state: revm::state::EvmState,
    ) -> Result<(), PortError>
    where
        Database: DatabaseCommit,
    {
        if self.commits != 0 {
            return Err(PortError::Incoherent);
        }
        database.commit(state);
        self.commits = 1;
        Ok(())
    }

    /// Returns whether exactly one commit completed.
    pub const fn completed_exactly_once(&self) -> bool {
        self.commits == 1
    }

    /// Rejects a processing path that did not complete exactly one commit.
    pub const fn finish(self) -> Result<(), PortError> {
        if self.completed_exactly_once() { Ok(()) } else { Err(PortError::Incoherent) }
    }
}

/// Opaque evidence that one frame completed the authoritative processing path.
#[derive(Debug)]
pub struct ProcessedFrame {
    materialized_state: MaterializedState,
    measurement_context: MeasurementContext,
    dirty_pools: DirtyPoolSet,
}

impl ProcessedFrame {
    /// Returns immutable post-victim state materialized from audited writes.
    pub const fn materialized_state(&self) -> &MaterializedState {
        &self.materialized_state
    }

    /// Returns immutable frame identity bound to the successful processing result.
    pub const fn measurement_context(&self) -> &MeasurementContext {
        &self.measurement_context
    }

    /// Returns the canonical pools whose quote slots changed in the actual victim delta.
    pub const fn dirty_pools(&self) -> &DirtyPoolSet {
        &self.dirty_pools
    }
}

/// Payload traversal state used for generation coherence checks.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotCoherence {
    first_payload_id: Option<PayloadId>,
    last_flashblock_index: Option<u64>,
    visited: u32,
    all_same_payload: bool,
}

impl SnapshotCoherence {
    fn validated_payload_id(snapshot: &SnapshotHandle) -> Result<Option<PayloadId>, PortError> {
        let mut coherence = Self { all_same_payload: true, ..Self::default() };
        let summary = snapshot.visit_latest_block_payloads(&mut coherence)?;
        Ok((summary.complete
            && summary.visited == coherence.visited
            && coherence.visited != 0
            && coherence.all_same_payload
            && coherence.last_flashblock_index == Some(snapshot.latest_flashblock_index()))
        .then_some(coherence.first_payload_id)
        .flatten())
    }

    /// Validates non-empty, single-payload, exact-latest-index snapshot coherence.
    pub fn validate(snapshot: &SnapshotHandle) -> Result<bool, PortError> {
        Ok(Self::validated_payload_id(snapshot)?.is_some())
    }
}

impl PayloadVisitor for SnapshotCoherence {
    fn visit(
        &mut self,
        payload_id: PayloadId,
        flashblock_index: u64,
    ) -> Result<VisitControl, PortError> {
        match self.first_payload_id {
            Some(first) if first != payload_id => self.all_same_payload = false,
            None => self.first_payload_id = Some(payload_id),
            Some(_) => {}
        }
        self.last_flashblock_index = Some(flashblock_index);
        self.visited = self.visited.checked_add(1).ok_or(PortError::LimitExceeded)?;
        Ok(VisitControl::Continue)
    }
}

/// Validates and executes one victim frame against an authoritative captured snapshot.
#[derive(Debug, Default, Clone, Copy)]
pub struct FrameProcessor;

impl FrameProcessor {
    /// Decodes a frame only when all hash, sender, chain, type, generation, and age checks pass.
    pub fn decode(
        snapshot: &SnapshotHandle,
        frame: &VictimFrame,
        now: Instant,
    ) -> Option<BaseTxEnvelope> {
        if frame.raw_tx.len() > MAX_RAW_FRAME_BYTES
            || frame.parent_hash != snapshot.parent_hash()
            || frame.block_number != snapshot.latest_block_number()
            || snapshot.latest_flashblock_index() >= frame.victim_flashblock_index
            || frame.victim_flashblock_index.checked_sub(snapshot.latest_flashblock_index())
                > Some(1)
            || snapshot.has_transaction_hash(frame.transaction_hash)
            || now.checked_duration_since(frame.received_at)?.as_millis()
                > u128::from(MAX_FRAME_AGE_MILLIS)
            || keccak256(&frame.raw_tx) != frame.transaction_hash
        {
            return None;
        }

        let transaction = BaseTxEnvelope::decode_2718_exact(frame.raw_tx.as_ref()).ok()?;
        if *transaction.tx_hash() != frame.transaction_hash
            || transaction.ty() != frame.transaction_type
            || transaction.chain_id() != Some(frame.chain_id)
        {
            return None;
        }

        match &transaction {
            BaseTxEnvelope::Legacy(_) if transaction.chain_id().is_none() => return None,
            BaseTxEnvelope::Legacy(_) | BaseTxEnvelope::Eip2930(_) | BaseTxEnvelope::Eip1559(_) => {
            }
            BaseTxEnvelope::Eip7702(_)
            | BaseTxEnvelope::Deposit(_)
            | BaseTxEnvelope::Eip8130(_) => return None,
        }
        if transaction.recover_signer().ok()? != frame.from {
            return None;
        }
        Some(transaction)
    }

    /// Executes, validates and classifies, commits exactly once, materializes, then rechecks authority.
    pub fn process(
        port: &dyn TraderSnapshotPort,
        snapshot: &SnapshotHandle,
        frame: &VictimFrame,
        now: Instant,
        chain_spec: Arc<BaseChainSpec>,
        audit: &FrameAuditPlan,
        cancellation: &CancellationProbe,
    ) -> Result<Option<ProcessedFrame>, PortError> {
        if !port.is_current_authoritative(snapshot) {
            return Ok(None);
        }
        let Some(payload_id) = SnapshotCoherence::validated_payload_id(snapshot)? else {
            return Ok(None);
        };

        let latest_header = snapshot.latest_header();
        if latest_header.number != snapshot.latest_block_number()
            || latest_header.parent_hash != snapshot.parent_hash()
            || snapshot.canonical_block_number().checked_add(1)
                != Some(snapshot.latest_block_number())
        {
            return Ok(None);
        }

        let parent_header = port.sealed_header_at_hash(snapshot.parent_hash())?;
        if parent_header.hash() != snapshot.parent_hash()
            || parent_header.number != snapshot.canonical_block_number()
        {
            return Ok(None);
        }
        let Some(transaction) = Self::decode(snapshot, frame, now) else { return Ok(None) };

        let provider = port.state_at_hash(snapshot.parent_hash())?;
        let database = StateProviderDatabase::new(provider);
        let state = State::builder().with_database(database).with_bundle_update().build();
        let evm_config = BaseEvmConfig::base(chain_spec);
        let evm_env = evm_config.evm_env(&latest_header).map_err(|_| PortError::Incoherent)?;
        let mut evm = evm_config.evm_with_env(state, evm_env);
        let recovered = Recovered::new_unchecked(transaction, frame.from);
        let output = match evm.transact(evm_config.tx_env(&recovered)) {
            Ok(output) => output,
            Err(_) => return Ok(None),
        };
        if !matches!(output.result, ExecutionResult::Success { .. }) {
            return Ok(None);
        }
        let Ok(validated_delta) = DeltaGuard::validate_and_classify(&output.state, audit) else {
            return Ok(None);
        };

        let mut commit = FrameCommitGuard::default();
        commit.commit(evm.db_mut(), output.state)?;
        let materialized =
            StateMaterializer::materialize(evm.db_mut(), audit.audited_writes(), cancellation)?;
        drop(evm);
        commit.finish()?;

        let current_authority = port.is_current_authoritative(snapshot);
        if !cancellation.checkpoint(Instant::now(), current_authority) {
            cancellation.token().request_cancel();
            cancellation.acknowledge_drop();
            return Ok(None);
        }
        Ok(Some(ProcessedFrame {
            materialized_state: materialized,
            measurement_context: MeasurementContext {
                parent_hash: snapshot.parent_hash(),
                block_number: snapshot.latest_block_number(),
                predecessor_index: snapshot.latest_flashblock_index(),
                payload_id,
                victim: frame.transaction_hash,
            },
            dirty_pools: validated_delta.dirty_pools,
        }))
    }
}

/// Production-path processed-frame support for sibling crate-unit tests.
#[cfg(test)]
pub(crate) mod test_utils {
    use std::{
        collections::BTreeMap,
        str::FromStr,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use alloy_consensus::{Header, Sealed, Transaction, transaction::SignerRecoverable};
    use alloy_eips::{Decodable2718, Typed2718};
    use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
    use alloy_rpc_types_engine::PayloadId;
    use base_common_consensus::BaseTxEnvelope;
    use base_execution_chainspec::BaseChainSpec;
    use reth_provider::StateProviderBox;

    use super::{FrameProcessor, ProcessedFrame, VictimFrame};
    use crate::{
        BundleVisitor, CancellationProbe, CancellationToken, GlobalLifecycle, PayloadVisitor,
        PendingSnapshotView, PortError, SnapshotCaptureCoordinator, SnapshotHandle,
        SnapshotHandleFactory, TraderSnapshotPort, TransactionVisitor, VisitControl, VisitSummary,
        registry,
    };

    const RAW_VICTIM: &str = "f8628080830186a0940000000000000000000000000000000000000000808082422da0840cfc572845f5786e702984c2a582528cad4b49b2a10b9db1be7fca90058565a025e7109ceb98168d95b09b18bbf6b685130e0562f233877d492b94eee0c5b6d1";
    const BLOCK_NUMBER: u64 = 100;
    const PREDECESSOR_INDEX: u64 = 1;

    #[derive(Debug)]
    struct ProofView {
        parent_hash: B256,
        latest_header: Mutex<Sealed<Header>>,
    }

    impl PendingSnapshotView for ProofView {
        fn parent_hash(&self) -> B256 {
            self.parent_hash
        }

        fn latest_block_number(&self) -> u64 {
            BLOCK_NUMBER
        }

        fn canonical_block_number(&self) -> u64 {
            BLOCK_NUMBER - 1
        }

        fn latest_flashblock_index(&self) -> u64 {
            PREDECESSOR_INDEX
        }

        fn latest_header(&self) -> Sealed<Header> {
            self.latest_header.lock().expect("fixture header lock").clone()
        }

        fn pending_account_nonce(
            &self,
            _address: Address,
        ) -> Result<Option<crate::PendingAccountNonce>, PortError> {
            Ok(None)
        }

        fn latest_block_transaction_count(&self) -> usize {
            0
        }

        fn has_transaction_hash(&self, _transaction_hash: B256) -> bool {
            false
        }

        fn transaction_position(
            &self,
            _block_number: u64,
            _transaction_hash: B256,
        ) -> Option<usize> {
            None
        }

        fn visit_latest_block_payloads(
            &self,
            visitor: &mut dyn PayloadVisitor,
        ) -> Result<VisitSummary, PortError> {
            let control = visitor.visit(PayloadId::new([2; 8]), PREDECESSOR_INDEX)?;
            Ok(VisitSummary { visited: 1, complete: control == VisitControl::Continue })
        }

        fn visit_transactions_for_block(
            &self,
            _block_number: u64,
            _start: usize,
            _limit: usize,
            _visitor: &mut dyn TransactionVisitor,
        ) -> Result<VisitSummary, PortError> {
            Ok(VisitSummary { visited: 0, complete: true })
        }

        fn visit_bundle(
            &self,
            _visitor: &mut dyn BundleVisitor,
        ) -> Result<VisitSummary, PortError> {
            Ok(VisitSummary { visited: 0, complete: true })
        }
    }

    #[derive(Debug)]
    struct ProofPort {
        view: Arc<dyn PendingSnapshotView + Send + Sync>,
        received_at: Instant,
        parent_header: Sealed<Header>,
        provider_available: AtomicBool,
        authority_checks: AtomicUsize,
        authoritative_until: AtomicUsize,
    }

    impl TraderSnapshotPort for ProofPort {
        fn capture_latest(
            &self,
            factory: &SnapshotHandleFactory,
        ) -> Result<Option<SnapshotHandle>, PortError> {
            factory.issue(Arc::clone(&self.view), self.received_at).map(Some)
        }

        fn is_current_authoritative(&self, handle: &SnapshotHandle) -> bool {
            let check = self.authority_checks.fetch_add(1, Ordering::SeqCst);
            check < self.authoritative_until.load(Ordering::SeqCst)
                && handle.matches_capture(&self.view, self.received_at)
        }

        fn state_at_hash(&self, block_hash: B256) -> Result<StateProviderBox, PortError> {
            if !self.provider_available.load(Ordering::SeqCst) {
                return Err(PortError::ProviderUnavailable);
            }
            if block_hash != self.parent_header.hash() {
                return Err(PortError::Incoherent);
            }
            Ok(Box::new(reth_provider::noop::NoopProvider::default()))
        }

        fn sealed_header_at_hash(&self, block_hash: B256) -> Result<Sealed<Header>, PortError> {
            if block_hash != self.parent_header.hash() {
                return Err(PortError::Incoherent);
            }
            Ok(self.parent_header.clone())
        }
    }

    /// Opaque test-only owner of one captured deterministic processing fixture.
    #[derive(Debug)]
    pub(crate) struct TestFrameHarness {
        port: ProofPort,
        view: Arc<ProofView>,
        snapshot: SnapshotHandle,
        frame: VictimFrame,
        chain_spec: Arc<BaseChainSpec>,
        audited_writes: [crate::AuditedWriteKey; 1],
        processing_probe: CancellationProbe,
    }

    impl TestFrameHarness {
        /// Captures the exact authoritative snapshot used by the fixture.
        pub(crate) fn capture() -> Self {
            Self::capture_timed().0
        }

        /// Prepares the fixture, then sets receive t0 around only authoritative capture.
        pub(crate) fn capture_timed() -> (Self, Instant) {
            let raw_tx = Bytes::from_str(&format!("0x{RAW_VICTIM}")).expect("fixture bytes");
            let transaction =
                BaseTxEnvelope::decode_2718_exact(raw_tx.as_ref()).expect("fixture transaction");
            let transaction_hash = keccak256(&raw_tx);
            let from = transaction.recover_signer().expect("fixture sender");

            assert_eq!(*transaction.tx_hash(), transaction_hash);
            assert_eq!(transaction.chain_id(), Some(8453));
            assert_eq!(transaction.ty(), 0);
            assert_eq!(transaction.nonce(), 0);
            assert_eq!(transaction.gas_price(), Some(0));
            assert_eq!(transaction.gas_limit(), 100_000);
            assert_eq!(transaction.to(), Some(Address::ZERO));
            assert_eq!(transaction.value(), U256::ZERO);
            assert!(transaction.input().is_empty());

            let parent_header = Sealed::new(Header {
                number: BLOCK_NUMBER - 1,
                gas_limit: 30_000_000,
                base_fee_per_gas: Some(0),
                ..Default::default()
            });
            let parent_hash = parent_header.hash();
            let latest_header = Sealed::new(Header {
                parent_hash,
                number: BLOCK_NUMBER,
                gas_limit: 30_000_000,
                base_fee_per_gas: Some(0),
                ..Default::default()
            });
            let view =
                Arc::new(ProofView { parent_hash, latest_header: Mutex::new(latest_header) });
            let captured_view: Arc<dyn PendingSnapshotView + Send + Sync> =
                Arc::<ProofView>::clone(&view);
            let fixture_time = Instant::now();
            let mut frame = VictimFrame {
                chain_id: 8453,
                transaction_type: 0,
                transaction_hash,
                from,
                raw_tx,
                parent_hash,
                block_number: BLOCK_NUMBER,
                victim_flashblock_index: PREDECESSOR_INDEX + 1,
                received_at: fixture_time,
            };
            let port = ProofPort {
                view: captured_view,
                received_at: fixture_time,
                parent_header,
                provider_available: AtomicBool::new(true),
                authority_checks: AtomicUsize::new(0),
                authoritative_until: AtomicUsize::new(usize::MAX),
            };
            let chain_spec = Arc::new(BaseChainSpec::mainnet());
            let audited_writes = registry::test_utils::audited_sender_nonce(from);
            let processing_probe = CancellationProbe::new(
                Arc::new(CancellationToken::with_approved_deadline(Instant::now())),
                Arc::new(GlobalLifecycle::default()),
            );

            frame.received_at = Instant::now();
            let snapshot = SnapshotCaptureCoordinator
                .capture(&port)
                .expect("snapshot capture")
                .expect("authoritative snapshot");
            let harness =
                Self { port, view, snapshot, frame, chain_spec, audited_writes, processing_probe };
            let discover_end = Instant::now();
            (harness, discover_end)
        }

        /// Returns the captured victim frame, including its receive t0.
        pub(crate) const fn frame(&self) -> &VictimFrame {
            &self.frame
        }

        /// Returns the exact audited sender-nonce key used by the positive fixture.
        pub(crate) const fn audited_writes(&self) -> [crate::AuditedWriteKey; 1] {
            self.audited_writes
        }

        /// Makes the hash-pinned provider unavailable for the next processing attempt.
        pub(crate) fn make_provider_unavailable(&self) {
            self.port.provider_available.store(false, Ordering::SeqCst);
        }

        /// Makes the captured latest header fail block-number coherence.
        pub(crate) fn make_latest_header_incoherent(&self) {
            let latest_header = Sealed::new(Header {
                parent_hash: self.frame.parent_hash,
                number: BLOCK_NUMBER + 1,
                gas_limit: 30_000_000,
                base_fee_per_gas: Some(0),
                ..Default::default()
            });
            *self.view.latest_header.lock().expect("fixture header lock") = latest_header;
        }

        /// Allows exactly `successful` authority checks during the next processing attempt.
        pub(crate) fn allow_authority_checks(&self, successful: usize) {
            self.port.authority_checks.store(0, Ordering::SeqCst);
            self.port.authoritative_until.store(successful, Ordering::SeqCst);
        }

        /// Executes the production processing path and preserves its fail-closed result.
        pub(crate) fn process_result(
            &self,
            audited_writes: &[crate::AuditedWriteKey],
            cancellation: &CancellationProbe,
        ) -> Result<Option<ProcessedFrame>, PortError> {
            let audit = crate::FrameAuditPlan::new(audited_writes.to_vec(), BTreeMap::new())
                .map_err(|_| PortError::Incoherent)?;
            FrameProcessor::process(
                &self.port,
                &self.snapshot,
                &self.frame,
                Instant::now(),
                Arc::clone(&self.chain_spec),
                &audit,
                cancellation,
            )
        }

        /// Executes the production processing call with the prepared positive inputs.
        pub(crate) fn process_prepared(&self) -> Result<Option<ProcessedFrame>, PortError> {
            self.process_result(&self.audited_writes, &self.processing_probe)
        }

        /// Checks the audited write and context of a successful processing result.
        pub(crate) fn assert_processed(&self, processed: &ProcessedFrame) {
            assert_eq!(self.audited_writes[0].address(), self.frame.from);
            assert!(!self.audited_writes[0].evidence_digest().is_zero());
            assert_eq!(processed.materialized_state().writes.len(), 1);
            assert_eq!(processed.materialized_state().writes[0].key, self.audited_writes[0]);
            assert_eq!(processed.materialized_state().writes[0].value, U256::from(1));
            assert_eq!(
                processed.measurement_context(),
                &crate::MeasurementContext {
                    parent_hash: self.frame.parent_hash,
                    block_number: BLOCK_NUMBER,
                    predecessor_index: PREDECESSOR_INDEX,
                    payload_id: PayloadId::new([2; 8]),
                    victim: self.frame.transaction_hash,
                }
            );
        }

        /// Obtains a proof only by executing the production processing path.
        pub(crate) fn process(&self, cancellation: &CancellationProbe) -> ProcessedFrame {
            let processed = self
                .process_result(&self.audited_writes, cancellation)
                .expect("frame processing")
                .expect("successful frame proof");
            self.assert_processed(&processed);
            processed
        }
    }

    /// Obtains a proof through capture and the production frame-processing path.
    pub(crate) fn processed_frame() -> ProcessedFrame {
        let cancellation = CancellationProbe::new(
            Arc::new(CancellationToken::new(Instant::now() + Duration::from_secs(5))),
            Arc::new(GlobalLifecycle::default()),
        );
        TestFrameHarness::capture().process(&cancellation)
    }
}
#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, str::FromStr, sync::Arc, time::Duration};

    use alloy_consensus::{Header, Sealed};
    use alloy_primitives::U256;
    use revm::state::{Account, EvmState, EvmStorageSlot, TransactionId};
    use revm_bytecode::Bytecode;
    use revm_database::{BundleAccount, InMemoryDB};

    use super::*;
    use crate::{
        BundleVisitor, CancellationToken, GlobalLifecycle, PendingSnapshotView,
        SnapshotHandleFactory, TaskState, TransactionVisitor, VisitSummary,
    };

    #[test]
    fn t4a_delta_classifies_canonical_dirty_pool_subset_from_actual_changes() {
        let first = Address::with_last_byte(1);
        let second = Address::with_last_byte(2);
        let third = Address::with_last_byte(3);
        let slot = U256::from(7);
        let nonce_address = Address::with_last_byte(9);
        let mut keys = vec![
            crate::AuditedWriteKey::Storage {
                address: second,
                slot,
                evidence_digest: B256::with_last_byte(2),
            },
            crate::AuditedWriteKey::Storage {
                address: third,
                slot,
                evidence_digest: B256::with_last_byte(4),
            },
            crate::AuditedWriteKey::AccountNonce {
                address: nonce_address,
                evidence_digest: B256::with_last_byte(3),
            },
            crate::AuditedWriteKey::Storage {
                address: first,
                slot,
                evidence_digest: B256::with_last_byte(1),
            },
        ];
        keys.sort();
        let owners = [((third, slot), third), ((second, slot), second), ((first, slot), first)]
            .into_iter()
            .collect();
        let audit = FrameAuditPlan::new(keys, owners).expect("audit");

        let mut first_account = Account::default();
        first_account.storage.insert(
            slot,
            EvmStorageSlot::new_changed(U256::ZERO, U256::from(1), TransactionId::ZERO),
        );
        first_account.mark_touch();
        let mut third_account = Account::default();
        third_account.storage.insert(
            slot,
            EvmStorageSlot::new_changed(U256::ZERO, U256::from(3), TransactionId::ZERO),
        );
        third_account.mark_touch();
        let mut nonce_account = Account::default();
        nonce_account.set_current_info_as_original();
        nonce_account.info.nonce = 1;
        nonce_account.mark_touch();
        let state: EvmState =
            [(third, third_account), (nonce_address, nonce_account), (first, first_account)]
                .into_iter()
                .collect();

        let validated = DeltaGuard::validate_and_classify(&state, &audit).expect("allowed delta");
        assert_eq!(validated.dirty_pools().as_slice(), &[first, third]);
        let empty = FrameAuditPlan::new(Vec::new(), BTreeMap::new()).expect("empty audit");
        assert_eq!(
            DeltaGuard::validate_and_classify(&state, &empty),
            Err(DeltaError::DeltaNotAudited)
        );
    }

    #[test]
    fn t4a_frame_process_preserves_commit_once_and_delta_rejection_parity() {
        let accepted = test_utils::TestFrameHarness::capture();
        let (_, accepted_probe) = cancellation(Instant::now() + Duration::from_secs(5));
        let processed = accepted
            .process_result(&accepted.audited_writes(), &accepted_probe)
            .expect("processing")
            .expect("accepted");
        accepted.assert_processed(&processed);
        assert!(processed.dirty_pools().is_empty());
        let mut commit = FrameCommitGuard::default();
        assert!(!commit.completed_exactly_once());
        commit.commit(&mut InMemoryDB::default(), EvmState::default()).expect("first commit");
        assert!(commit.completed_exactly_once());
        assert_eq!(
            commit.commit(&mut InMemoryDB::default(), EvmState::default()),
            Err(PortError::Incoherent)
        );
        commit.finish().expect("exactly one commit");

        let rejected = test_utils::TestFrameHarness::capture();
        let (_, probe) = cancellation(Instant::now() + Duration::from_secs(5));
        assert!(matches!(rejected.process_result(&[], &probe), Ok(None)));

        let address = Address::with_last_byte(44);
        let allowed = crate::AuditedWriteKey::AccountBalance {
            address,
            evidence_digest: B256::with_last_byte(45),
        };
        let mut changed = Account::default();
        changed.set_current_info_as_original();
        changed.info.balance = U256::from(2);
        changed.mark_touch();
        let changed_state: EvmState = [(address, changed)].into_iter().collect();
        assert!(DeltaGuard::permits(&changed_state, &[allowed]));
        assert!(!DeltaGuard::permits(&changed_state, &[allowed, allowed]));
        assert!(!DeltaGuard::permits(&changed_state, &[]));

        let mut code_changed = Account::default();
        code_changed.set_current_info_as_original();
        code_changed.info.balance = U256::from(2);
        code_changed.info.code_hash = B256::with_last_byte(46);
        code_changed.mark_touch();
        let code_changed_state: EvmState = [(address, code_changed)].into_iter().collect();
        assert!(!DeltaGuard::permits(&code_changed_state, &[allowed]));
    }

    const LEGACY_TX: &str = "f86c098504a817c800825208943535353535353535353535353535353535353535880de0b6b3a76400008025a028ef61340bd939bc2195fe537567866003e1a15d3c71ff63e1590620aa636276a067cbe9d8997f761aecb703304b3800ccf555c9f3dc64214b297fb1966a3b6d83";
    const ACCESS_LIST_TX: &str = "01f85f808080809401234567890123456789012345678901234567898080c080a0840cfc572845f5786e702984c2a582528cad4b49b2a10b9db1be7fca90058565a025e7109ceb98168d95b09b18bbf6b685130e0562f233877d492b94eee0c5b6d1";
    const DYNAMIC_FEE_TX: &str = "02f86c0d010183072335825208940000000000000000000000000000000000000000872386f26fc1000080c001a0cdb9e4f2f1ba53f9429077e7055e078cf599786e29059cd80c5e0e923bb2c114a01c90e29201e031baf1da66296c3a5c15c200bcb5e6c34da2f05f7d1778f8be07";

    #[derive(Debug)]
    struct FrameView {
        payloads: Vec<(PayloadId, u64)>,
        contained_hash: Option<B256>,
        latest_flashblock_index: u64,
    }

    impl PendingSnapshotView for FrameView {
        fn parent_hash(&self) -> B256 {
            B256::with_last_byte(1)
        }

        fn latest_block_number(&self) -> u64 {
            100
        }

        fn canonical_block_number(&self) -> u64 {
            99
        }

        fn latest_flashblock_index(&self) -> u64 {
            self.latest_flashblock_index
        }

        fn latest_header(&self) -> Sealed<Header> {
            Sealed::new_unchecked(
                Header {
                    parent_hash: self.parent_hash(),
                    number: self.latest_block_number(),
                    ..Default::default()
                },
                B256::with_last_byte(2),
            )
        }

        fn pending_account_nonce(
            &self,
            _address: Address,
        ) -> Result<Option<crate::PendingAccountNonce>, PortError> {
            Ok(None)
        }

        fn latest_block_transaction_count(&self) -> usize {
            0
        }

        fn has_transaction_hash(&self, transaction_hash: B256) -> bool {
            self.contained_hash == Some(transaction_hash)
        }

        fn transaction_position(
            &self,
            _block_number: u64,
            _transaction_hash: B256,
        ) -> Option<usize> {
            None
        }

        fn visit_latest_block_payloads(
            &self,
            visitor: &mut dyn PayloadVisitor,
        ) -> Result<VisitSummary, PortError> {
            let mut visited = 0;
            for (payload_id, index) in &self.payloads {
                visited += 1;
                if visitor.visit(*payload_id, *index)? == VisitControl::Stop {
                    return Ok(VisitSummary { visited, complete: false });
                }
            }
            Ok(VisitSummary { visited, complete: true })
        }

        fn visit_transactions_for_block(
            &self,
            _block_number: u64,
            _start: usize,
            _limit: usize,
            _visitor: &mut dyn TransactionVisitor,
        ) -> Result<VisitSummary, PortError> {
            Ok(VisitSummary { visited: 0, complete: true })
        }

        fn visit_bundle(
            &self,
            _visitor: &mut dyn BundleVisitor,
        ) -> Result<VisitSummary, PortError> {
            Ok(VisitSummary { visited: 0, complete: true })
        }
    }

    fn snapshot_with(
        payloads: Vec<(PayloadId, u64)>,
        contained_hash: Option<B256>,
        received_at: Instant,
    ) -> SnapshotHandle {
        snapshot_with_latest(payloads, contained_hash, received_at, 1)
    }

    fn snapshot_with_latest(
        payloads: Vec<(PayloadId, u64)>,
        contained_hash: Option<B256>,
        received_at: Instant,
        latest_flashblock_index: u64,
    ) -> SnapshotHandle {
        let view: Arc<dyn PendingSnapshotView + Send + Sync> =
            Arc::new(FrameView { payloads, contained_hash, latest_flashblock_index });
        SnapshotHandleFactory::new().issue(view, received_at).expect("fresh factory")
    }

    fn frame_for(raw: &str, now: Instant) -> VictimFrame {
        let raw_tx = Bytes::from_str(&format!("0x{raw}")).expect("fixture bytes");
        let transaction =
            BaseTxEnvelope::decode_2718_exact(raw_tx.as_ref()).expect("fixture transaction");
        VictimFrame {
            chain_id: transaction.chain_id().expect("protected fixture"),
            transaction_type: transaction.ty(),
            transaction_hash: B256::from(*transaction.tx_hash()),
            from: transaction.recover_signer().expect("fixture sender"),
            raw_tx,
            parent_hash: B256::with_last_byte(1),
            block_number: 100,
            victim_flashblock_index: 2,
            received_at: now,
        }
    }

    #[test]
    fn decode_accepts_protected_types_zero_one_and_two() {
        let now = Instant::now();
        let snapshot = snapshot_with(vec![(PayloadId::default(), 1)], None, now);

        for raw in [LEGACY_TX, ACCESS_LIST_TX, DYNAMIC_FEE_TX] {
            assert!(FrameProcessor::decode(&snapshot, &frame_for(raw, now), now).is_some());
        }
    }

    #[test]
    fn decode_enforces_victim_flashblock_and_feed_block_relation() {
        let now = Instant::now();
        let mut frame = frame_for(DYNAMIC_FEE_TX, now);

        let zero = snapshot_with_latest(vec![(PayloadId::default(), 0)], None, now, 0);
        frame.victim_flashblock_index = 1;
        assert!(FrameProcessor::decode(&zero, &frame, now).is_some());

        let four = snapshot_with_latest(vec![(PayloadId::default(), 4)], None, now, 4);
        frame.victim_flashblock_index = 5;
        assert!(FrameProcessor::decode(&four, &frame, now).is_some());

        frame.victim_flashblock_index = 4;
        assert!(FrameProcessor::decode(&four, &frame, now).is_none());
        frame.victim_flashblock_index = 0;
        assert!(FrameProcessor::decode(&four, &frame, now).is_none());
        frame.victim_flashblock_index = 3;
        assert!(FrameProcessor::decode(&four, &frame, now).is_none());
        frame.victim_flashblock_index = 6;
        assert!(FrameProcessor::decode(&four, &frame, now).is_none());

        frame.victim_flashblock_index = 5;
        frame.block_number = 101;
        assert!(FrameProcessor::decode(&four, &frame, now).is_none());
    }

    #[test]
    fn decode_rejects_hash_presence_and_age_mismatches() {
        let now = Instant::now();
        let mut frame = frame_for(DYNAMIC_FEE_TX, now);
        let snapshot =
            snapshot_with(vec![(PayloadId::default(), 1)], Some(frame.transaction_hash), now);
        assert!(FrameProcessor::decode(&snapshot, &frame, now).is_none());

        let snapshot = snapshot_with(vec![(PayloadId::default(), 1)], None, now);
        frame.received_at = now - Duration::from_millis(MAX_FRAME_AGE_MILLIS + 1);
        assert!(FrameProcessor::decode(&snapshot, &frame, now).is_none());
    }

    #[test]
    fn coherence_requires_nonempty_single_payload_and_exact_latest_index() {
        let now = Instant::now();
        let first = PayloadId::default();
        let second = PayloadId::new([1; 8]);

        assert!(
            SnapshotCoherence::validate(&snapshot_with(vec![(first, 0), (first, 1)], None, now))
                .expect("coherence")
        );
        assert!(
            !SnapshotCoherence::validate(&snapshot_with(Vec::new(), None, now)).expect("coherence")
        );
        assert!(
            !SnapshotCoherence::validate(&snapshot_with(vec![(first, 0), (second, 1)], None, now,))
                .expect("coherence")
        );
        assert!(
            !SnapshotCoherence::validate(&snapshot_with(vec![(first, 0)], None, now))
                .expect("coherence")
        );
    }

    #[test]
    fn production_process_mints_the_pinned_measurement_proof() {
        let processed = test_utils::processed_frame();
        assert_eq!(processed.materialized_state().writes.len(), 1);
    }

    fn cancellation(deadline: Instant) -> (Arc<CancellationToken>, CancellationProbe) {
        let token = Arc::new(CancellationToken::new(deadline));
        let probe =
            CancellationProbe::new(Arc::clone(&token), Arc::new(GlobalLifecycle::default()));
        (token, probe)
    }

    #[test]
    fn production_process_fails_closed_for_provider_header_and_delta_faults() {
        let deadline = Instant::now() + Duration::from_secs(5);

        let provider = test_utils::TestFrameHarness::capture();
        provider.make_provider_unavailable();
        let (_, probe) = cancellation(deadline);
        assert!(matches!(
            provider.process_result(&provider.audited_writes(), &probe),
            Err(PortError::ProviderUnavailable)
        ));

        let header = test_utils::TestFrameHarness::capture();
        header.make_latest_header_incoherent();
        let (_, probe) = cancellation(deadline);
        assert!(matches!(header.process_result(&header.audited_writes(), &probe), Ok(None)));

        let delta = test_utils::TestFrameHarness::capture();
        let (_, probe) = cancellation(deadline);
        assert!(matches!(delta.process_result(&[], &probe), Ok(None)));
    }

    #[test]
    fn production_process_fails_closed_for_precancel_and_deadline() {
        let harness = test_utils::TestFrameHarness::capture();
        let (token, probe) = cancellation(Instant::now() + Duration::from_secs(5));
        assert!(token.request_cancel());
        assert!(matches!(
            harness.process_result(&harness.audited_writes(), &probe),
            Err(PortError::Incoherent)
        ));
        assert_eq!(token.state(), TaskState::DroppedAcked);

        let harness = test_utils::TestFrameHarness::capture();
        let (token, probe) = cancellation(Instant::now());
        assert!(matches!(
            harness.process_result(&harness.audited_writes(), &probe),
            Err(PortError::Incoherent)
        ));
        assert_eq!(token.state(), TaskState::DroppedAcked);
    }

    #[test]
    fn production_process_fails_closed_for_initial_and_final_authority_loss() {
        let initial = test_utils::TestFrameHarness::capture();
        initial.allow_authority_checks(0);
        let (token, probe) = cancellation(Instant::now() + Duration::from_secs(5));
        assert!(matches!(initial.process_result(&initial.audited_writes(), &probe), Ok(None)));
        assert_eq!(token.state(), TaskState::Active);

        let final_loss = test_utils::TestFrameHarness::capture();
        final_loss.allow_authority_checks(1);
        let (token, probe) = cancellation(Instant::now() + Duration::from_secs(5));
        assert!(matches!(
            final_loss.process_result(&final_loss.audited_writes(), &probe),
            Ok(None)
        ));
        assert_eq!(token.state(), TaskState::DroppedAcked);
    }
    #[test]
    fn bundle_types_remain_borrowed_at_public_boundary() {
        fn visitor_shape(
            _account: &BundleAccount,
            _bytecode: &Bytecode,
            _visitor: &mut dyn BundleVisitor,
        ) {
        }

        let _ = visitor_shape;
    }
}
