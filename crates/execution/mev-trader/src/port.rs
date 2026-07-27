use std::{cell::Cell, fmt::Debug, marker::PhantomData, rc::Rc, sync::Arc, time::Instant};

use alloy_consensus::{Header, Sealed};
use alloy_primitives::{Address, B256};
use alloy_rpc_types_engine::PayloadId;
use base_common_consensus::BaseTxEnvelope;
use reth_provider::StateProviderBox;
use revm_bytecode::Bytecode;
use revm_database::BundleAccount;
use thiserror::Error;

/// Controls whether a borrowed snapshot traversal continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisitControl {
    /// Continue visiting source entries.
    Continue,
    /// Stop before source exhaustion.
    Stop,
}

/// Describes how much of a borrowed snapshot source was visited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisitSummary {
    /// Number of entries presented to the visitor.
    pub visited: u32,
    /// Whether the underlying source was exhausted.
    pub complete: bool,
}

/// Errors exposed by the read-only snapshot port.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum PortError {
    /// The pending snapshot is unavailable.
    #[error("pending snapshot unavailable")]
    SnapshotUnavailable,
    /// The hash-pinned state provider is unavailable.
    #[error("hash-pinned state provider unavailable")]
    ProviderUnavailable,
    /// The hash-pinned header is unavailable.
    #[error("hash-pinned header unavailable")]
    HeaderUnavailable,
    /// A required borrowed traversal stopped early.
    #[error("required snapshot traversal stopped early")]
    VisitorStopped,
    /// A configured bounded traversal limit was exceeded.
    #[error("snapshot traversal limit exceeded")]
    LimitExceeded,
    /// Snapshot or frame coherence validation failed.
    #[error("snapshot is incoherent")]
    Incoherent,
    /// The single-use snapshot handle factory was already consumed.
    #[error("snapshot handle factory already used")]
    FactoryAlreadyUsed,
    /// A runtime captured the snapshot before required edge evidence was attached.
    #[error("MissingRequiredEvidence")]
    MissingRequiredEvidence,
}

/// Visits pending payload identifiers without exposing their backing snapshot.
pub trait PayloadVisitor: Debug {
    /// Visits one payload identifier and its flashblock index.
    fn visit(
        &mut self,
        payload_id: PayloadId,
        flashblock_index: u64,
    ) -> Result<VisitControl, PortError>;
}

/// Visits decoded transactions without transferring snapshot ownership.
pub trait TransactionVisitor: Debug {
    /// Visits one transaction at its block-local position.
    fn visit(
        &mut self,
        position: usize,
        transaction: &BaseTxEnvelope,
    ) -> Result<VisitControl, PortError>;
}

/// Visits pending bundle accounts and bytecode.
pub trait BundleVisitor: Debug {
    /// Visits one pending bundle account.
    fn visit_account(
        &mut self,
        address: Address,
        account: &BundleAccount,
    ) -> Result<VisitControl, PortError>;

    /// Visits one pending contract keyed by its code hash.
    fn visit_contract(
        &mut self,
        code_hash: B256,
        bytecode: &Bytecode,
    ) -> Result<VisitControl, PortError>;
}

/// Checked account nonce values captured from one pending-state overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingAccountNonce {
    original_nonce: u64,
    current_nonce: u64,
}

impl PendingAccountNonce {
    /// Constructs a coherent overlay nonce whose current value has not regressed.
    pub const fn checked(original_nonce: u64, current_nonce: u64) -> Result<Self, PortError> {
        if current_nonce < original_nonce {
            return Err(PortError::Incoherent);
        }
        Ok(Self { original_nonce, current_nonce })
    }

    /// Returns the account nonce before pending-state changes.
    pub const fn original_nonce(&self) -> u64 {
        self.original_nonce
    }

    /// Returns the account nonce after pending-state changes.
    pub const fn current_nonce(&self) -> u64 {
        self.current_nonce
    }
}

/// Opaque, borrowed view of one immutable pending snapshot.
pub trait PendingSnapshotView: Debug + Send + Sync {
    /// Returns the canonical parent hash used by pending execution.
    fn parent_hash(&self) -> B256;

    /// Returns the latest pending block number.
    fn latest_block_number(&self) -> u64;

    /// Returns the canonical block number immediately before pending execution.
    fn canonical_block_number(&self) -> u64;

    /// Returns the latest flashblock index.
    fn latest_flashblock_index(&self) -> u64;

    /// Returns the latest sealed pending header.
    fn latest_header(&self) -> Sealed<Header>;

    /// Returns checked account nonce values from the pending overlay, when present.
    fn pending_account_nonce(
        &self,
        address: Address,
    ) -> Result<Option<PendingAccountNonce>, PortError>;

    /// Returns the transaction count in the latest pending block.
    fn latest_block_transaction_count(&self) -> usize;

    /// Returns whether the snapshot contains a transaction hash.
    fn has_transaction_hash(&self, transaction_hash: B256) -> bool;

    /// Returns a transaction's block-local position when present.
    fn transaction_position(&self, block_number: u64, transaction_hash: B256) -> Option<usize>;

    /// Visits all payloads belonging to the latest pending block.
    fn visit_latest_block_payloads(
        &self,
        visitor: &mut dyn PayloadVisitor,
    ) -> Result<VisitSummary, PortError>;

    /// Visits a bounded transaction range for one block.
    fn visit_transactions_for_block(
        &self,
        block_number: u64,
        start: usize,
        limit: usize,
        visitor: &mut dyn TransactionVisitor,
    ) -> Result<VisitSummary, PortError>;

    /// Visits the snapshot's pending bundle.
    fn visit_bundle(&self, visitor: &mut dyn BundleVisitor) -> Result<VisitSummary, PortError>;
}
/// Exact source facts attached only after the matching registry terminal exists.
#[cfg(feature = "edge-measurement")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeSnapshotEvidenceV1 {
    /// Source generation bound by the flash producer.
    pub source_generation: u64,
    /// Pending publication sequence.
    pub pending_snapshot_sequence: u64,
    /// Independent coverage-queue acceptance sequence.
    pub coverage_sequence: u64,
    /// Payload-first ledger sequence.
    pub payload_first_record_sequence: u64,
    /// Payload-first authority hash.
    pub payload_first_record_hash: B256,
    /// Structural processor terminal hash.
    pub structural_terminal_hash: B256,
    /// Connection record sequence current at CLI receipt.
    pub connection_sequence: u64,
    /// Connection authority hash current at CLI receipt.
    pub connection_record_hash: B256,
    /// Canonical SHA-256 of the complete pending terminal record.
    pub registry_terminal_record_hash: B256,
}

/// Single-use issuer for an opaque snapshot handle.
#[derive(Debug)]
pub struct SnapshotHandleFactory {
    issued: Cell<bool>,
    not_thread_safe: PhantomData<Rc<()>>,
}

impl SnapshotHandleFactory {
    pub(crate) const fn new() -> Self {
        Self { issued: Cell::new(false), not_thread_safe: PhantomData }
    }

    /// Issues the sole handle permitted for this capture call.
    pub fn issue(
        &self,
        view: Arc<dyn PendingSnapshotView + Send + Sync>,
        received_at: Instant,
    ) -> Result<SnapshotHandle, PortError> {
        if self.issued.replace(true) {
            return Err(PortError::FactoryAlreadyUsed);
        }
        Ok(SnapshotHandle {
            view,
            received_at,
            #[cfg(feature = "edge-measurement")]
            edge_evidence: None,
        })
    }
    /// Issues the sole handle with already-attached exact edge evidence.
    #[cfg(feature = "edge-measurement")]
    pub fn issue_with_edge_evidence(
        &self,
        view: Arc<dyn PendingSnapshotView + Send + Sync>,
        received_at: Instant,
        evidence: EdgeSnapshotEvidenceV1,
    ) -> Result<SnapshotHandle, PortError> {
        if self.issued.replace(true) {
            return Err(PortError::FactoryAlreadyUsed);
        }
        Ok(SnapshotHandle { view, received_at, edge_evidence: Some(evidence) })
    }
}

/// Opaque authority-bearing handle for one captured pending snapshot.
#[derive(Debug)]
pub struct SnapshotHandle {
    view: Arc<dyn PendingSnapshotView + Send + Sync>,
    received_at: Instant,
    #[cfg(feature = "edge-measurement")]
    edge_evidence: Option<EdgeSnapshotEvidenceV1>,
}

impl SnapshotHandle {
    /// Returns when the captured pending record was received.
    pub const fn received_at(&self) -> Instant {
        self.received_at
    }
    /// Returns the exact evidence captured at handle issuance, never a later backfill.
    #[cfg(feature = "edge-measurement")]
    pub fn edge_evidence(&self) -> Result<EdgeSnapshotEvidenceV1, PortError> {
        self.edge_evidence.ok_or(PortError::MissingRequiredEvidence)
    }

    /// Returns the captured canonical parent hash.
    pub fn parent_hash(&self) -> B256 {
        self.view.parent_hash()
    }

    /// Returns the captured latest pending block number.
    pub fn latest_block_number(&self) -> u64 {
        self.view.latest_block_number()
    }

    /// Returns the captured canonical block number.
    pub fn canonical_block_number(&self) -> u64 {
        self.view.canonical_block_number()
    }

    /// Returns the captured latest flashblock index.
    pub fn latest_flashblock_index(&self) -> u64 {
        self.view.latest_flashblock_index()
    }

    /// Returns the captured latest pending header.
    pub fn latest_header(&self) -> Sealed<Header> {
        self.view.latest_header()
    }

    /// Returns checked account nonce values from the captured pending overlay, when present.
    pub fn pending_account_nonce(
        &self,
        address: Address,
    ) -> Result<Option<PendingAccountNonce>, PortError> {
        self.view.pending_account_nonce(address)
    }

    /// Returns the captured latest-block transaction count.
    pub fn latest_block_transaction_count(&self) -> usize {
        self.view.latest_block_transaction_count()
    }

    /// Returns whether the captured snapshot contains a transaction hash.
    pub fn has_transaction_hash(&self, transaction_hash: B256) -> bool {
        self.view.has_transaction_hash(transaction_hash)
    }

    /// Returns the captured block-local transaction position.
    pub fn transaction_position(&self, block_number: u64, transaction_hash: B256) -> Option<usize> {
        self.view.transaction_position(block_number, transaction_hash)
    }

    /// Visits all latest-block payloads through the opaque view.
    pub fn visit_latest_block_payloads(
        &self,
        visitor: &mut dyn PayloadVisitor,
    ) -> Result<VisitSummary, PortError> {
        self.view.visit_latest_block_payloads(visitor)
    }

    /// Visits a bounded block transaction range through the opaque view.
    pub fn visit_transactions_for_block(
        &self,
        block_number: u64,
        start: usize,
        limit: usize,
        visitor: &mut dyn TransactionVisitor,
    ) -> Result<VisitSummary, PortError> {
        self.view.visit_transactions_for_block(block_number, start, limit, visitor)
    }

    /// Visits the pending bundle through the opaque view.
    pub fn visit_bundle(&self, visitor: &mut dyn BundleVisitor) -> Result<VisitSummary, PortError> {
        self.view.visit_bundle(visitor)
    }

    /// Returns only whether this handle matches a captured typed view and receive time.
    pub fn matches_capture(
        &self,
        view: &Arc<dyn PendingSnapshotView + Send + Sync>,
        received_at: Instant,
    ) -> bool {
        Arc::ptr_eq(&self.view, view) && self.received_at == received_at
    }
}

/// Read-only node adapter used to capture and validate pending snapshot authority.
pub trait TraderSnapshotPort: Debug + Send + Sync {
    /// Captures the latest pending snapshot using the borrowed single-use factory.
    fn capture_latest(
        &self,
        factory: &SnapshotHandleFactory,
    ) -> Result<Option<SnapshotHandle>, PortError>;

    /// Revalidates that a handle still represents the current authoritative snapshot.
    fn is_current_authoritative(&self, handle: &SnapshotHandle) -> bool;

    /// Returns state pinned to an exact block hash.
    fn state_at_hash(&self, block_hash: B256) -> Result<StateProviderBox, PortError>;

    /// Returns a sealed header pinned to an exact block hash.
    fn sealed_header_at_hash(&self, block_hash: B256) -> Result<Sealed<Header>, PortError>;
}

/// Coordinates one stack-scoped factory capture.
#[derive(Debug, Default, Clone, Copy)]
pub struct SnapshotCaptureCoordinator;

impl SnapshotCaptureCoordinator {
    /// Captures a handle and rejects authority that is already stale.
    pub fn capture(
        &self,
        port: &dyn TraderSnapshotPort,
    ) -> Result<Option<SnapshotHandle>, PortError> {
        let factory = SnapshotHandleFactory::new();
        let handle = port.capture_latest(&factory)?;
        Ok(handle.filter(|handle| port.is_current_authoritative(handle)))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Debug)]
    struct EmptyView;

    impl PendingSnapshotView for EmptyView {
        fn parent_hash(&self) -> B256 {
            B256::ZERO
        }

        fn latest_block_number(&self) -> u64 {
            1
        }

        fn canonical_block_number(&self) -> u64 {
            0
        }

        fn latest_flashblock_index(&self) -> u64 {
            1
        }

        fn latest_header(&self) -> Sealed<Header> {
            Sealed::new_unchecked(Header { number: 1, ..Default::default() }, B256::ZERO)
        }

        fn pending_account_nonce(
            &self,
            _address: Address,
        ) -> Result<Option<PendingAccountNonce>, PortError> {
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
            _visitor: &mut dyn PayloadVisitor,
        ) -> Result<VisitSummary, PortError> {
            Ok(VisitSummary { visited: 0, complete: true })
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
    struct TestPort {
        view: Arc<dyn PendingSnapshotView + Send + Sync>,
        received_at: Instant,
        issued_twice: bool,
        current: Mutex<bool>,
    }

    impl TraderSnapshotPort for TestPort {
        fn capture_latest(
            &self,
            factory: &SnapshotHandleFactory,
        ) -> Result<Option<SnapshotHandle>, PortError> {
            let handle = factory.issue(Arc::clone(&self.view), self.received_at)?;
            if self.issued_twice {
                let _ = factory.issue(Arc::clone(&self.view), self.received_at)?;
            }
            Ok(Some(handle))
        }

        fn is_current_authoritative(&self, handle: &SnapshotHandle) -> bool {
            *self.current.lock().expect("current mutex")
                && handle.matches_capture(&self.view, self.received_at)
        }

        fn state_at_hash(&self, _block_hash: B256) -> Result<StateProviderBox, PortError> {
            Err(PortError::ProviderUnavailable)
        }

        fn sealed_header_at_hash(&self, _block_hash: B256) -> Result<Sealed<Header>, PortError> {
            Err(PortError::HeaderUnavailable)
        }
    }

    #[test]
    fn factory_is_single_use() {
        let port = TestPort {
            view: Arc::new(EmptyView),
            received_at: Instant::now(),
            issued_twice: true,
            current: Mutex::new(true),
        };
        assert!(matches!(
            SnapshotCaptureCoordinator.capture(&port),
            Err(PortError::FactoryAlreadyUsed)
        ));
    }

    #[test]
    fn coordinator_rejects_stale_authority() {
        let port = TestPort {
            view: Arc::new(EmptyView),
            received_at: Instant::now(),
            issued_twice: false,
            current: Mutex::new(false),
        };
        assert!(SnapshotCaptureCoordinator.capture(&port).expect("capture").is_none());
    }
    #[cfg(feature = "edge-measurement")]
    #[test]
    fn evidence_is_frozen_at_capture_and_missing_is_named() {
        let view: Arc<dyn PendingSnapshotView + Send + Sync> = Arc::new(EmptyView);
        let missing =
            SnapshotHandleFactory::new().issue(Arc::clone(&view), Instant::now()).expect("handle");
        assert_eq!(missing.edge_evidence(), Err(PortError::MissingRequiredEvidence));

        let present = SnapshotHandleFactory::new()
            .issue_with_edge_evidence(
                view,
                Instant::now(),
                EdgeSnapshotEvidenceV1 {
                    source_generation: 1,
                    pending_snapshot_sequence: 2,
                    coverage_sequence: 7,
                    payload_first_record_sequence: 3,
                    payload_first_record_hash: B256::with_last_byte(4),
                    structural_terminal_hash: B256::with_last_byte(5),
                    connection_sequence: 6,
                    connection_record_hash: B256::with_last_byte(7),
                    registry_terminal_record_hash: B256::with_last_byte(8),
                },
            )
            .expect("evidence handle")
            .edge_evidence()
            .expect("evidence");
        assert_eq!(present.source_generation, 1);
        assert_eq!(present.pending_snapshot_sequence, 2);
        assert_eq!(present.payload_first_record_sequence, 3);
        assert_eq!(present.connection_sequence, 6);
        assert_eq!(present.coverage_sequence, 7);
        assert_eq!(present.registry_terminal_record_hash, B256::with_last_byte(8));
    }
}
