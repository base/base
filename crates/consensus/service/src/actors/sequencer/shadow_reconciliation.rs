//! Request routing and buffered canonical inputs for shadow reconciliation.

use std::collections::{BTreeMap, VecDeque};

use alloy_primitives::B256;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use base_consensus_engine::ConsolidateInput;
use base_protocol::L2BlockInfo;
use tracing::debug;

use crate::{EngineClientError, SequencerConfig};

/// Sequencer engine state while following canonical blocks or producing shadow blocks.
#[derive(Debug)]
pub enum SequencerEngineState {
    /// No canonical catch-up or shadow reconciliation routing is active.
    Regular,
    /// The sequencer is following canonical safe derivation and unsafe gossip.
    CatchingUp {
        /// Whether catch-up completion should activate private shadow production.
        shadow: bool,
        /// Rolling canonical unsafe payload buffer shared by follower sequencers.
        catchup: CanonicalUnsafeCatchup,
    },
    /// Private block production is active and canonical inputs are buffered for reconciliation.
    ShadowActive(Box<ShadowReconciliationGate>),
}

/// Rolling canonical unsafe payloads retained while safe derivation catches up.
#[derive(Debug, Default)]
pub struct CanonicalUnsafeCatchup {
    payloads: BTreeMap<u64, BaseExecutionPayloadEnvelope>,
    observations: BTreeMap<u64, B256>,
    highest_observed: Option<(u64, B256)>,
    faulted: bool,
}

impl CanonicalUnsafeCatchup {
    /// Maximum recent canonical unsafe payloads retained during catch-up.
    pub const MAX_PAYLOADS: usize = SequencerConfig::MAX_SHADOW_BLOCKS_PER_CYCLE as usize;

    /// Retains an authenticated canonical payload in the rolling recent window.
    pub fn buffer_payload(&mut self, envelope: BaseExecutionPayloadEnvelope) {
        let number = envelope.execution_payload.block_number();
        let hash = envelope.execution_payload.block_hash();
        if self.faulted {
            return;
        }
        if let Some(existing_hash) = self.observations.get(&number) {
            if *existing_hash != hash {
                self.faulted = true;
            }
            return;
        }
        self.observations.insert(number, hash);
        if self.highest_observed.is_none_or(|(highest, _)| number > highest) {
            self.highest_observed = Some((number, hash));
        }
        self.payloads.insert(number, envelope);
        while self.payloads.len() > Self::MAX_PAYLOADS {
            self.payloads.pop_first();
        }
        while self.observations.len() > Self::MAX_PAYLOADS {
            self.observations.pop_first();
        }
    }

    /// Returns the contiguous canonical suffix that can extend `anchor` now.
    pub fn contiguous_payloads(
        &mut self,
        anchor: L2BlockInfo,
    ) -> Vec<BaseExecutionPayloadEnvelope> {
        self.payloads.retain(|number, _| *number > anchor.block_info.number);

        let mut number = anchor.block_info.number.saturating_add(1);
        let mut parent = anchor.block_info.hash;
        let mut payloads = Vec::new();
        while let Some(payload) = self.payloads.get(&number) {
            if payload.execution_payload.parent_hash() != parent {
                break;
            }
            parent = payload.execution_payload.block_hash();
            payloads.push(payload.clone());
            number = number.saturating_add(1);
        }
        payloads
    }

    /// Removes payloads acknowledged by the execution engine.
    ///
    /// Recent observations remain bounded separately so conflicting gossip for an acknowledged
    /// height still faults catch-up.
    pub fn commit(&mut self, head: L2BlockInfo) {
        self.payloads.retain(|number, _| *number > head.block_info.number);
    }

    /// Returns whether conflicting canonical observations have faulted catch-up.
    pub const fn is_faulted(&self) -> bool {
        self.faulted
    }

    /// Returns whether the engine has reached or safely overtaken the latest observed payload.
    /// Buffered payloads across a gap intentionally prevent completion until every observation is
    /// either applied in order or overtaken by a fully safe-derived head.
    pub fn is_complete(&self, unsafe_head: L2BlockInfo, safe_head: L2BlockInfo) -> bool {
        self.highest_observed.is_some_and(|(number, hash)| {
            !self.faulted
                && self.payloads.is_empty()
                && ((unsafe_head.block_info.number == number
                    && unsafe_head.block_info.hash == hash)
                    || (unsafe_head == safe_head && safe_head.block_info.number > number))
        })
    }
}

/// Canonical inputs selected by the shadow gate for one authoritative engine update.
#[derive(Debug)]
pub struct CanonicalReconciliationInputs {
    /// Private unsafe head that must still be current before applying the inputs.
    pub shadow_head: L2BlockInfo,
    /// Contiguous authoritative payloads replacing the private branch.
    pub payloads: Vec<BaseExecutionPayloadEnvelope>,
    /// Safe-head signals deferred while the private branch was active.
    pub safe_signals: VecDeque<ConsolidateInput>,
    /// Latest finalized block number deferred during the cycle.
    pub finalized_block_number: Option<u64>,
}

/// Buffered canonical inputs and reconciliation progress for a shadow sequencer.
#[derive(Debug)]
pub struct ShadowReconciliationGate {
    payloads: BTreeMap<u64, BaseExecutionPayloadEnvelope>,
    local_payloads: BTreeMap<u64, BaseExecutionPayloadEnvelope>,
    faulted: bool,
    anchor: L2BlockInfo,
    safe_signals: VecDeque<ConsolidateInput>,
    latest_finalized: Option<u64>,
}

impl ShadowReconciliationGate {
    /// Maximum number of inputs retained for one reconciliation cycle.
    pub const MAX_PAYLOADS: usize = SequencerConfig::MAX_SHADOW_BLOCKS_PER_CYCLE as usize;
    /// Maximum deferred safe signals, independent of the payload-cycle limit because derivation
    /// catch-up can produce safe signals faster than canonical payload gossip arrives.
    pub const MAX_SAFE_SIGNALS: usize = 1024;

    /// Creates an empty gate anchored at the canonical unsafe head.
    pub const fn new(anchor: L2BlockInfo) -> Self {
        Self {
            payloads: BTreeMap::new(),
            local_payloads: BTreeMap::new(),
            faulted: false,
            anchor,
            safe_signals: VecDeque::new(),
            latest_finalized: None,
        }
    }

    /// Buffers an authenticated canonical payload.
    pub fn buffer_payload(&mut self, envelope: BaseExecutionPayloadEnvelope) {
        let number = envelope.execution_payload.block_number();
        if number <= self.anchor.block_info.number || self.faulted {
            return;
        }
        if let Some(existing) = self.payloads.get(&number) {
            if existing.execution_payload.block_hash() != envelope.execution_payload.block_hash() {
                self.faulted = true;
            }
            return;
        }
        if self.payloads.len() >= Self::MAX_PAYLOADS {
            self.faulted = true;
            return;
        }
        self.payloads.insert(number, envelope);
    }

    /// Retains a locally-built payload that may be identical to a suppressed canonical payload.
    pub fn buffer_local_payload(&mut self, envelope: &BaseExecutionPayloadEnvelope) {
        let number = envelope.execution_payload.block_number();
        if number <= self.anchor.block_info.number || self.faulted {
            return;
        }
        if self.local_payloads.len() >= Self::MAX_PAYLOADS {
            self.faulted = true;
            return;
        }
        self.local_payloads.entry(number).or_insert_with(|| envelope.clone());
    }

    /// Buffers a deferred safe signal.
    pub fn buffer_safe_signal(&mut self, signal: ConsolidateInput) {
        if self.safe_signals.len() >= Self::MAX_SAFE_SIGNALS {
            self.faulted = true;
        } else {
            self.safe_signals.push_back(signal);
        }
    }

    /// Returns whether a safe signal must wait for canonical reconciliation.
    pub const fn should_defer_safe_signal(&self, signal: &ConsolidateInput) -> bool {
        let block_number = match signal {
            ConsolidateInput::Attributes(attributes) => attributes.block_number(),
            ConsolidateInput::BlockInfo(block_info) => block_info.block_info.number,
        };
        block_number > self.anchor.block_info.number
    }

    /// Records the latest finalized block.
    pub const fn buffer_finalized(&mut self, number: u64) {
        self.latest_finalized = Some(match self.latest_finalized {
            Some(previous) if previous > number => previous,
            _ => number,
        });
    }

    /// Returns whether a finalized update must wait for the safe head or canonical anchor.
    pub const fn should_defer_finalized(&self, number: u64, safe_head_number: u64) -> bool {
        number > safe_head_number || number > self.anchor.block_info.number
    }

    /// Selects a complete reconciliation range, removing the matched payloads from the buffer.
    ///
    /// Validates the whole range before mutating anything: an incomplete range (`Ok(None)`) or
    /// an invalid one (`Err`) leaves the buffer untouched. Only once every payload in the range
    /// is confirmed present and parent-hash-chained does it remove them from the buffer, moving
    /// ownership into the returned inputs instead of cloning.
    pub fn prepare(
        &mut self,
        shadow_head: L2BlockInfo,
    ) -> Result<Option<CanonicalReconciliationInputs>, EngineClientError> {
        if self.faulted {
            return Err(EngineClientError::ShadowBufferFaulted);
        }
        let length = shadow_head
            .block_info
            .number
            .checked_sub(self.anchor.block_info.number)
            .ok_or_else(|| {
                EngineClientError::InvalidShadowReconciliation("shadow head precedes anchor".into())
            })?;
        if length == 0 || length > Self::MAX_PAYLOADS as u64 {
            return Err(EngineClientError::InvalidShadowReconciliation(
                "invalid reconciliation range".into(),
            ));
        }
        let range = self.anchor.block_info.number + 1..=shadow_head.block_info.number;
        let mut parent = self.anchor.block_info.hash;
        let mut local_payload_needs_canonical_witness = false;
        for number in range.clone() {
            let payload = self.payloads.get(&number).or_else(|| self.local_payloads.get(&number));
            let Some(payload) = payload else {
                debug!(
                    target: "engine",
                    missing_payload = number,
                    anchor = self.anchor.block_info.number,
                    shadow_head = shadow_head.block_info.number,
                    buffered_payloads = self.payloads.len(),
                    "Shadow reconciliation is waiting for canonical payload"
                );
                return Ok(None);
            };
            if payload.execution_payload.parent_hash() != parent {
                if local_payload_needs_canonical_witness || !self.payloads.contains_key(&number) {
                    return Ok(None);
                }
                return Err(EngineClientError::InvalidShadowReconciliation(
                    "payload parent continuity mismatch".into(),
                ));
            }
            local_payload_needs_canonical_witness = !self.payloads.contains_key(&number);
            parent = payload.execution_payload.block_hash();
        }
        if local_payload_needs_canonical_witness
            && self
                .payloads
                .get(&shadow_head.block_info.number.saturating_add(1))
                .is_none_or(|payload| payload.execution_payload.parent_hash() != parent)
        {
            return Ok(None);
        }
        let mut payloads = Vec::with_capacity(length as usize);
        for number in range {
            let payload = self
                .payloads
                .remove(&number)
                .or_else(|| self.local_payloads.remove(&number))
                .expect("presence validated above");
            payloads.push(payload);
        }
        Ok(Some(CanonicalReconciliationInputs {
            shadow_head,
            payloads,
            safe_signals: std::mem::take(&mut self.safe_signals),
            finalized_block_number: self.latest_finalized,
        }))
    }

    /// Commits a successful reconciliation and advances the anchor.
    pub fn commit(&mut self, head: L2BlockInfo) {
        self.payloads.retain(|number, _| *number > head.block_info.number);
        self.local_payloads.retain(|number, _| *number > head.block_info.number);
        self.anchor = head;
        self.safe_signals.clear();
        self.latest_finalized = None;
    }

    /// Clears buffered inputs and faults.
    pub fn clear(&mut self) {
        self.payloads.clear();
        self.local_payloads.clear();
        self.safe_signals.clear();
        self.latest_finalized = None;
        self.faulted = false;
    }

    /// Clears the gate and moves its canonical anchor.
    pub fn reanchor(&mut self, anchor: L2BlockInfo) {
        self.clear();
        self.anchor = anchor;
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bloom, U256};
    use alloy_rpc_types_engine::ExecutionPayloadV1;
    use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
    use base_consensus_engine::{ConsolidateInput, test_utils::TestAttributesBuilder};
    use base_protocol::{BlockInfo, L2BlockInfo};

    use super::{CanonicalUnsafeCatchup, ShadowReconciliationGate};
    use crate::EngineClientError;

    fn head(number: u64, hash: B256) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo { number, hash, ..Default::default() },
            ..Default::default()
        }
    }

    fn payload(number: u64, parent_hash: B256, block_hash: B256) -> BaseExecutionPayloadEnvelope {
        BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: BaseExecutionPayload::V1(ExecutionPayloadV1 {
                parent_hash,
                fee_recipient: Address::ZERO,
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::ZERO,
                prev_randao: B256::ZERO,
                block_number: number,
                gas_limit: 30_000_000,
                gas_used: 0,
                timestamp: number,
                extra_data: Default::default(),
                base_fee_per_gas: U256::ZERO,
                block_hash,
                transactions: vec![],
            }),
        }
    }

    #[test]
    fn catchup_waits_for_parent_then_returns_contiguous_suffix() {
        let safe = head(10, B256::with_last_byte(10));
        let mut catchup = CanonicalUnsafeCatchup::default();
        catchup.buffer_payload(payload(12, B256::with_last_byte(11), B256::with_last_byte(12)));
        catchup.buffer_payload(payload(11, safe.block_info.hash, B256::with_last_byte(11)));

        let payloads = catchup.contiguous_payloads(safe);
        assert_eq!(payloads.len(), 2);
        assert_eq!(payloads[0].execution_payload.block_number(), 11);
        assert_eq!(payloads[1].execution_payload.block_number(), 12);
        assert!(!catchup.is_complete(safe, safe));

        let canonical_head = head(12, B256::with_last_byte(12));
        catchup.commit(canonical_head);
        assert!(catchup.is_complete(canonical_head, safe));
    }

    #[test]
    fn catchup_retains_rolling_latest_payload_window() {
        let mut catchup = CanonicalUnsafeCatchup::default();
        let mut parent = B256::ZERO;
        for number in 1..=CanonicalUnsafeCatchup::MAX_PAYLOADS as u64 + 2 {
            let hash = B256::from(U256::from(number));
            catchup.buffer_payload(payload(number, parent, hash));
            parent = hash;
        }

        assert_eq!(catchup.payloads.len(), CanonicalUnsafeCatchup::MAX_PAYLOADS);
        assert_eq!(catchup.payloads.first_key_value().map(|(number, _)| *number), Some(3));
        assert_eq!(catchup.observations.len(), CanonicalUnsafeCatchup::MAX_PAYLOADS);
        assert_eq!(catchup.observations.first_key_value().map(|(number, _)| *number), Some(3));
        let anchor = head(2, B256::from(U256::from(2)));
        assert_eq!(catchup.contiguous_payloads(anchor).len(), CanonicalUnsafeCatchup::MAX_PAYLOADS);
    }

    #[test]
    fn conflict_at_oldest_retained_observation_faults_catchup() {
        let mut catchup = CanonicalUnsafeCatchup::default();
        for number in 1..=CanonicalUnsafeCatchup::MAX_PAYLOADS as u64 {
            catchup.buffer_payload(payload(number, B256::ZERO, B256::from(U256::from(number))));
        }
        catchup.commit(head(
            CanonicalUnsafeCatchup::MAX_PAYLOADS as u64,
            B256::from(U256::from(CanonicalUnsafeCatchup::MAX_PAYLOADS as u64)),
        ));

        catchup.buffer_payload(payload(1, B256::ZERO, B256::with_last_byte(0xff)));

        assert!(catchup.is_faulted());
    }

    #[test]
    fn safe_head_beyond_last_observed_payload_completes_catchup() {
        let mut catchup = CanonicalUnsafeCatchup::default();
        catchup.buffer_payload(payload(11, B256::with_last_byte(10), B256::with_last_byte(11)));
        let safe = head(12, B256::with_last_byte(12));
        assert!(catchup.contiguous_payloads(safe).is_empty());

        assert!(catchup.is_complete(safe, safe));
    }

    #[test]
    fn private_head_beyond_last_observed_payload_does_not_complete_catchup() {
        let mut catchup = CanonicalUnsafeCatchup::default();
        catchup.buffer_payload(payload(11, B256::with_last_byte(10), B256::with_last_byte(11)));
        let private = head(12, B256::with_last_byte(99));
        assert!(catchup.contiguous_payloads(private).is_empty());

        assert!(!catchup.is_complete(private, head(10, B256::with_last_byte(10))));
    }

    #[test]
    fn conflict_after_original_payload_was_applied_prevents_completion() {
        let mut catchup = CanonicalUnsafeCatchup::default();
        let original = head(11, B256::with_last_byte(11));
        catchup.buffer_payload(payload(11, B256::with_last_byte(10), original.block_info.hash));
        catchup.commit(original);
        assert!(catchup.is_complete(original, L2BlockInfo::default()));

        catchup.buffer_payload(payload(11, B256::with_last_byte(10), B256::with_last_byte(99)));

        assert!(catchup.is_faulted());
        assert!(!catchup.is_complete(original, L2BlockInfo::default()));
    }

    #[test]
    fn selects_out_of_order_contiguous_range_and_removes_it_from_the_buffer() {
        let anchor = head(10, B256::with_last_byte(10));
        let mut gate = ShadowReconciliationGate::new(anchor);
        gate.buffer_payload(payload(12, B256::with_last_byte(11), B256::with_last_byte(12)));
        gate.buffer_payload(payload(11, anchor.block_info.hash, B256::with_last_byte(11)));

        let prepared = gate.prepare(head(12, B256::with_last_byte(99))).unwrap().unwrap();
        assert_eq!(prepared.payloads.len(), 2);
        assert!(gate.payloads.is_empty());
    }

    #[test]
    fn incomplete_range_leaves_buffer_untouched() {
        let anchor = head(10, B256::with_last_byte(10));
        let mut gate = ShadowReconciliationGate::new(anchor);
        gate.buffer_payload(payload(11, anchor.block_info.hash, B256::with_last_byte(11)));

        let prepared = gate.prepare(head(12, B256::with_last_byte(99))).unwrap();
        assert!(prepared.is_none());
        assert_eq!(gate.payloads.len(), 1);
    }

    #[test]
    fn canonical_child_proves_suppressed_identical_local_parent() {
        let anchor = head(10, B256::with_last_byte(10));
        let mut gate = ShadowReconciliationGate::new(anchor);
        gate.buffer_local_payload(&payload(11, anchor.block_info.hash, B256::with_last_byte(11)));
        gate.buffer_payload(payload(12, B256::with_last_byte(11), B256::with_last_byte(12)));

        let prepared = gate.prepare(head(12, B256::with_last_byte(99))).unwrap().unwrap();
        assert_eq!(prepared.payloads.len(), 2);
        assert_eq!(prepared.payloads[0].execution_payload.block_hash(), B256::with_last_byte(11));
    }

    #[test]
    fn unwitnessed_local_tail_cannot_be_used_for_reconciliation() {
        let anchor = head(10, B256::with_last_byte(10));
        let mut gate = ShadowReconciliationGate::new(anchor);
        gate.buffer_local_payload(&payload(11, anchor.block_info.hash, B256::with_last_byte(11)));

        assert!(gate.prepare(head(11, B256::with_last_byte(11))).unwrap().is_none());
        assert_eq!(gate.local_payloads.len(), 1);
    }

    #[test]
    fn divergent_local_payload_waits_for_canonical_replacement() {
        let anchor = head(10, B256::with_last_byte(10));
        let mut gate = ShadowReconciliationGate::new(anchor);
        gate.buffer_local_payload(&payload(11, anchor.block_info.hash, B256::with_last_byte(99)));
        gate.buffer_payload(payload(12, B256::with_last_byte(11), B256::with_last_byte(12)));

        assert!(gate.prepare(head(12, B256::with_last_byte(12))).unwrap().is_none());
    }

    #[test]
    fn conflict_faults_gate_and_preserves_first_payload() {
        let mut gate = ShadowReconciliationGate::new(L2BlockInfo::default());
        gate.buffer_payload(payload(1, B256::ZERO, B256::with_last_byte(1)));
        gate.buffer_payload(payload(1, B256::ZERO, B256::with_last_byte(2)));

        assert!(matches!(
            gate.prepare(head(1, B256::ZERO)),
            Err(EngineClientError::ShadowBufferFaulted)
        ));
        assert_eq!(gate.payloads[&1].execution_payload.block_hash(), B256::with_last_byte(1));
    }

    #[test]
    fn overflow_faults_gate_without_eviction() {
        let mut gate = ShadowReconciliationGate::new(L2BlockInfo::default());
        for number in 1..=ShadowReconciliationGate::MAX_PAYLOADS as u64 {
            gate.buffer_payload(payload(number, B256::ZERO, B256::from(U256::from(number))));
        }
        gate.buffer_payload(payload(
            ShadowReconciliationGate::MAX_PAYLOADS as u64 + 1,
            B256::ZERO,
            B256::with_last_byte(1),
        ));

        assert!(matches!(
            gate.prepare(head(1, B256::ZERO)),
            Err(EngineClientError::ShadowBufferFaulted)
        ));
        assert_eq!(gate.payloads.len(), ShadowReconciliationGate::MAX_PAYLOADS);
    }

    #[test]
    fn finalized_block_number_does_not_regress() {
        let mut gate = ShadowReconciliationGate::new(L2BlockInfo::default());
        gate.buffer_finalized(12);
        gate.buffer_finalized(10);

        assert_eq!(gate.latest_finalized, Some(12));
    }

    #[test]
    fn only_defers_safe_signals_above_canonical_anchor() {
        let gate = ShadowReconciliationGate::new(head(10, B256::ZERO));

        assert!(
            !gate.should_defer_safe_signal(&ConsolidateInput::BlockInfo(head(10, B256::ZERO,)))
        );
        assert!(gate.should_defer_safe_signal(&ConsolidateInput::BlockInfo(head(11, B256::ZERO,))));
        assert!(!gate.should_defer_safe_signal(&ConsolidateInput::Attributes(Box::new(
            TestAttributesBuilder::new().with_parent(head(9, B256::ZERO)).build(),
        ))));
        assert!(gate.should_defer_safe_signal(&ConsolidateInput::Attributes(Box::new(
            TestAttributesBuilder::new().with_parent(head(10, B256::ZERO)).build(),
        ))));
    }

    #[test]
    fn only_defers_finalized_updates_above_safe_head_or_canonical_anchor() {
        let gate = ShadowReconciliationGate::new(head(10, B256::ZERO));

        assert!(!gate.should_defer_finalized(9, 9));
        assert!(gate.should_defer_finalized(10, 9));
        assert!(gate.should_defer_finalized(11, 11));
    }

    #[test]
    fn safe_signal_capacity_is_independent_of_payload_capacity() {
        let mut gate = ShadowReconciliationGate::new(L2BlockInfo::default());
        for number in 1..=ShadowReconciliationGate::MAX_PAYLOADS as u64 + 1 {
            gate.buffer_safe_signal(ConsolidateInput::BlockInfo(head(number, B256::ZERO)));
        }

        assert!(!gate.faulted);
        assert_eq!(gate.safe_signals.len(), ShadowReconciliationGate::MAX_PAYLOADS + 1);
    }

    #[test]
    fn reanchor_clears_inputs_and_fault() {
        let mut gate = ShadowReconciliationGate::new(L2BlockInfo::default());
        gate.buffer_payload(payload(1, B256::ZERO, B256::with_last_byte(1)));
        gate.buffer_payload(payload(1, B256::ZERO, B256::with_last_byte(2)));
        let anchor = head(20, B256::with_last_byte(20));
        gate.reanchor(anchor);

        assert!(gate.payloads.is_empty());
        assert_eq!(gate.anchor, anchor);
        assert!(!gate.faulted);
    }
}
