use std::{sync::Arc, time::Instant};

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
    AuditedWriteKey, DeltaGuard, MaterializedState, PayloadVisitor, PortError, SnapshotHandle,
    StateMaterializer, TraderSnapshotPort, VisitControl,
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
    /// Captured pending block number.
    pub block_number: u64,
    /// Flashblock index immediately preceding this frame.
    pub predecessor_index: u64,
    /// Time at which this decoded frame was received.
    pub received_at: Instant,
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
    /// Validates non-empty, single-payload, exact-latest-index snapshot coherence.
    pub fn validate(snapshot: &SnapshotHandle) -> Result<bool, PortError> {
        let mut coherence = Self { all_same_payload: true, ..Self::default() };
        let summary = snapshot.visit_latest_block_payloads(&mut coherence)?;
        Ok(summary.complete
            && summary.visited == coherence.visited
            && coherence.visited != 0
            && coherence.all_same_payload
            && coherence.last_flashblock_index == Some(snapshot.latest_flashblock_index()))
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
            || frame.predecessor_index >= snapshot.latest_flashblock_index()
            || snapshot.latest_flashblock_index().checked_sub(frame.predecessor_index)? > 1
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

    /// Executes, guards, commits exactly once, materializes, drops provider state, then rechecks authority.
    pub fn process(
        port: &dyn TraderSnapshotPort,
        snapshot: &SnapshotHandle,
        frame: &VictimFrame,
        now: Instant,
        chain_spec: Arc<BaseChainSpec>,
        audited_writes: &[AuditedWriteKey],
    ) -> Result<Option<MaterializedState>, PortError> {
        if !port.is_current_authoritative(snapshot) || !SnapshotCoherence::validate(snapshot)? {
            return Ok(None);
        }

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
        if !matches!(output.result, ExecutionResult::Success { .. })
            || !DeltaGuard::permits(&output.state, audited_writes)
        {
            return Ok(None);
        }

        evm.db_mut().commit(output.state);
        let materialized = StateMaterializer::materialize(evm.db_mut(), audited_writes)?;
        drop(evm);

        if !port.is_current_authoritative(snapshot) {
            return Ok(None);
        }
        Ok(Some(materialized))
    }
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, sync::Arc, time::Duration};

    use alloy_consensus::{Header, Sealed};
    use revm_bytecode::Bytecode;
    use revm_database::BundleAccount;

    use super::*;
    use crate::{
        BundleVisitor, PendingSnapshotView, SnapshotHandleFactory, TransactionVisitor, VisitSummary,
    };

    const LEGACY_TX: &str = "f86c098504a817c800825208943535353535353535353535353535353535353535880de0b6b3a76400008025a028ef61340bd939bc2195fe537567866003e1a15d3c71ff63e1590620aa636276a067cbe9d8997f761aecb703304b3800ccf555c9f3dc64214b297fb1966a3b6d83";
    const ACCESS_LIST_TX: &str = "01f85f808080809401234567890123456789012345678901234567898080c080a0840cfc572845f5786e702984c2a582528cad4b49b2a10b9db1be7fca90058565a025e7109ceb98168d95b09b18bbf6b685130e0562f233877d492b94eee0c5b6d1";
    const DYNAMIC_FEE_TX: &str = "02f86c0d010183072335825208940000000000000000000000000000000000000000872386f26fc1000080c001a0cdb9e4f2f1ba53f9429077e7055e078cf599786e29059cd80c5e0e923bb2c114a01c90e29201e031baf1da66296c3a5c15c200bcb5e6c34da2f05f7d1778f8be07";

    #[derive(Debug)]
    struct FrameView {
        payloads: Vec<(PayloadId, u64)>,
        contained_hash: Option<B256>,
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
            1
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
        let view: Arc<dyn PendingSnapshotView + Send + Sync> =
            Arc::new(FrameView { payloads, contained_hash });
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
            predecessor_index: 0,
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
    fn decode_rejects_generation_hash_presence_and_age_mismatches() {
        let now = Instant::now();
        let mut frame = frame_for(DYNAMIC_FEE_TX, now);
        let snapshot =
            snapshot_with(vec![(PayloadId::default(), 1)], Some(frame.transaction_hash), now);
        assert!(FrameProcessor::decode(&snapshot, &frame, now).is_none());

        let snapshot = snapshot_with(vec![(PayloadId::default(), 1)], None, now);
        frame.predecessor_index = 1;
        assert!(FrameProcessor::decode(&snapshot, &frame, now).is_none());
        frame.predecessor_index = 0;
        frame.block_number = 101;
        assert!(FrameProcessor::decode(&snapshot, &frame, now).is_none());
        frame.block_number = 100;
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
