//! Request routing and buffered canonical inputs for shadow reconciliation.

use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};

use alloy_primitives::B256;
use base_common_genesis::{DenimTimestampSchedule, RollupConfig};
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use base_consensus_engine::ConsolidateInput;
use base_protocol::L2BlockInfo;
use tracing::debug;

use crate::{EngineClientError, NodeOperatingMode, SequencerConfig};

/// The kind of sequencer a node runs as, fixed for the life of the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequencerKind {
    /// Sequences on the canonical chain.
    Regular,
    /// Stops following canonical inputs after startup catch-up and produces a private chain.
    Isolated,
    /// Produces private blocks and reconciles them with the canonical chain.
    Shadow,
}

impl From<NodeOperatingMode> for SequencerKind {
    fn from(mode: NodeOperatingMode) -> Self {
        match mode {
            NodeOperatingMode::Validator | NodeOperatingMode::Sequencer => Self::Regular,
            NodeOperatingMode::IsolatedSequencer => Self::Isolated,
            NodeOperatingMode::ShadowSequencer { .. } => Self::Shadow,
        }
    }
}

impl SequencerKind {
    /// Returns the engine state entered once catch-up completes at the canonical `head`.
    pub fn completed_state(self, head: L2BlockInfo) -> SequencerEngineState {
        match self {
            Self::Regular => SequencerEngineState::Regular,
            Self::Isolated => SequencerEngineState::IsolatedActive,
            Self::Shadow => {
                SequencerEngineState::ShadowActive(Box::new(ShadowReconciliationGate::new(head)))
            }
        }
    }
}

/// Sequencer engine state while following canonical blocks or producing shadow blocks.
#[derive(Debug)]
pub enum SequencerEngineState {
    /// No canonical catch-up or shadow reconciliation routing is active.
    Regular,
    /// The sequencer is following canonical safe derivation and unsafe gossip.
    CatchingUp {
        /// Rolling canonical unsafe payload buffer shared by follower sequencers.
        catchup: CanonicalUnsafeCatchup,
    },
    /// Private block production is active and canonical inputs are buffered for reconciliation.
    ShadowActive(Box<ShadowReconciliationGate>),
    /// An isolated sequencer finished startup catch-up and now drops all canonical inputs so
    /// they cannot reorg its private chain.
    IsolatedActive,
}

/// Rolling canonical unsafe payloads retained while safe derivation catches up.
#[derive(Debug)]
pub struct CanonicalUnsafeCatchup {
    max_payloads: usize,
    payloads: BTreeMap<u64, BaseExecutionPayloadEnvelope>,
    observations: BTreeMap<u64, B256>,
    highest_observed: Option<(u64, B256)>,
    faulted: bool,
}

impl Default for CanonicalUnsafeCatchup {
    fn default() -> Self {
        Self::new(Self::MAX_PAYLOADS)
    }
}

impl CanonicalUnsafeCatchup {
    /// Wall-clock span of recent canonical gossip retained during catch-up.
    ///
    /// A restarted follower only connects to the buffer once derived safe blocks reach the oldest
    /// retained payload, so the window must exceed the unsafe-to-safe derivation latency (batch
    /// submission, L1 inclusion, and derivation) with ample margin, independent of block time.
    pub const CATCHUP_WINDOW: Duration = Duration::from_secs(600);

    /// Payloads retained during catch-up at the fastest supported block cadence.
    pub const MAX_PAYLOADS: usize = (Self::CATCHUP_WINDOW.as_millis()
        / DenimTimestampSchedule::DENIM_BLOCK_INTERVAL_MILLIS as u128)
        as usize;

    /// Most buffered payloads applied to the engine before it serves other requests.
    pub const MAX_APPLY_BATCH: usize = SequencerConfig::MAX_SHADOW_BLOCKS_PER_CYCLE as usize;

    /// Creates a catch-up buffer retaining at most `max_payloads` recent payloads.
    pub const fn new(max_payloads: usize) -> Self {
        Self {
            max_payloads,
            payloads: BTreeMap::new(),
            observations: BTreeMap::new(),
            highest_observed: None,
            faulted: false,
        }
    }

    /// Returns the number of payloads that span [`Self::CATCHUP_WINDOW`] at `rollup`'s block time.
    ///
    /// A chain with Denim scheduled is sized for the Denim cadence so the window also holds
    /// through the transition from whole-second blocks.
    pub fn max_payloads_for(rollup: &RollupConfig) -> usize {
        let legacy_interval_millis = rollup.block_time.saturating_mul(1_000);
        let interval_millis =
            if legacy_interval_millis == 0 || rollup.denim_timestamp_schedule().is_some() {
                DenimTimestampSchedule::DENIM_BLOCK_INTERVAL_MILLIS
            } else {
                legacy_interval_millis
            };
        ((Self::CATCHUP_WINDOW.as_millis() / u128::from(interval_millis)) as usize).max(1)
    }

    /// Returns the maximum number of payloads retained.
    pub const fn max_payloads(&self) -> usize {
        self.max_payloads
    }

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
        while self.payloads.len() > self.max_payloads {
            self.payloads.pop_first();
        }
        while self.observations.len() > self.max_payloads {
            self.observations.pop_first();
        }
    }

    /// Returns at most `limit` payloads of the contiguous canonical suffix that can extend
    /// `anchor` now.
    ///
    /// Bounding the batch keeps the engine loop responsive and the cloned working set small while
    /// a long run is applied; the remainder is returned by later calls as the anchor advances.
    pub fn contiguous_payloads(
        &mut self,
        anchor: L2BlockInfo,
        limit: usize,
    ) -> Vec<BaseExecutionPayloadEnvelope> {
        self.payloads.retain(|number, _| *number > anchor.block_info.number);

        let mut number = anchor.block_info.number.saturating_add(1);
        let mut parent = anchor.block_info.hash;
        let mut payloads = Vec::new();
        while payloads.len() < limit {
            let Some(payload) = self.payloads.get(&number) else { break };
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

    /// Returns whether any canonical unsafe payload has ever been observed.
    pub const fn has_observations(&self) -> bool {
        self.highest_observed.is_some()
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
    use base_common_genesis::{BaseUpgradeConfig, RollupConfig, UpgradeConfig};
    use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
    use base_consensus_engine::{ConsolidateInput, test_utils::TestAttributesBuilder};
    use base_protocol::{BlockInfo, L2BlockInfo};
    use rstest::rstest;

    use super::{CanonicalUnsafeCatchup, ShadowReconciliationGate};
    use crate::{EngineClientError, SequencerConfig};

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

        let payloads = catchup.contiguous_payloads(safe, usize::MAX);
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
        assert_eq!(
            catchup.contiguous_payloads(anchor, usize::MAX).len(),
            CanonicalUnsafeCatchup::MAX_PAYLOADS
        );
    }

    #[test]
    fn catchup_connects_when_derivation_lags_beyond_the_shadow_cycle_limit() {
        // Derivation leaves the safe head more than the shadow cycle limit behind the latest
        // gossip, which is what happens to a restarted follower when blocks are fast.
        let lag = SequencerConfig::MAX_SHADOW_BLOCKS_PER_CYCLE + 14;
        let anchor_number = 50;
        let tip = anchor_number + lag;
        let mut catchup = CanonicalUnsafeCatchup::default();
        let mut parent = B256::from(U256::from(anchor_number));
        for number in anchor_number + 1..=tip {
            let hash = B256::from(U256::from(number));
            catchup.buffer_payload(payload(number, parent, hash));
            parent = hash;
        }
        let anchor = head(anchor_number, B256::from(U256::from(anchor_number)));
        assert!(!catchup.is_complete(anchor, anchor));

        let suffix = catchup.contiguous_payloads(anchor, usize::MAX);

        assert_eq!(suffix.len() as u64, lag);
        let applied = head(tip, B256::from(U256::from(tip)));
        catchup.commit(applied);
        assert!(catchup.is_complete(applied, applied));
    }

    #[rstest]
    #[case::denim_default(RollupConfig::default(), 3_000)]
    #[case::legacy_block_time(rollup_config(2, None), 300)]
    #[case::denim_scheduled(rollup_config(2, Some(1_000)), 3_000)]
    #[case::block_time_longer_than_window(rollup_config(601, None), 1)]
    fn catchup_window_spans_the_same_time_at_any_block_time(
        #[case] rollup: RollupConfig,
        #[case] expected: usize,
    ) {
        assert_eq!(CanonicalUnsafeCatchup::max_payloads_for(&rollup), expected);
    }

    #[test]
    fn full_window_is_applied_in_bounded_batches() {
        let total = CanonicalUnsafeCatchup::MAX_PAYLOADS as u64;
        let mut catchup = CanonicalUnsafeCatchup::default();
        let mut parent = B256::ZERO;
        for number in 1..=total {
            let hash = B256::from(U256::from(number));
            catchup.buffer_payload(payload(number, parent, hash));
            parent = hash;
        }

        let mut anchor = head(0, B256::ZERO);
        let mut applied = 0;
        while !catchup.is_complete(anchor, anchor) {
            let batch =
                catchup.contiguous_payloads(anchor, CanonicalUnsafeCatchup::MAX_APPLY_BATCH);
            assert!(!batch.is_empty() && batch.len() <= CanonicalUnsafeCatchup::MAX_APPLY_BATCH);
            applied += batch.len() as u64;
            let last = batch.last().unwrap().execution_payload.block_number();
            anchor = head(last, B256::from(U256::from(last)));
            catchup.commit(anchor);
        }

        assert_eq!(applied, total);
    }

    #[test]
    fn failed_batch_resumes_after_applied_prefix() {
        let mut catchup = CanonicalUnsafeCatchup::new(10);
        let mut parent = B256::ZERO;
        for number in 1..=10 {
            let hash = B256::from(U256::from(number));
            catchup.buffer_payload(payload(number, parent, hash));
            parent = hash;
        }

        // The engine applied block 4 before failing without a commit; the retry starts after it.
        let anchor = head(4, B256::from(U256::from(4)));
        let retry = catchup.contiguous_payloads(anchor, 3);

        let numbers: Vec<_> =
            retry.iter().map(|payload| payload.execution_payload.block_number()).collect();
        assert_eq!(numbers, vec![5, 6, 7]);
    }

    #[test]
    fn catchup_retains_only_its_configured_capacity() {
        let mut catchup = CanonicalUnsafeCatchup::new(4);
        let mut parent = B256::ZERO;
        for number in 1..=6 {
            let hash = B256::from(U256::from(number));
            catchup.buffer_payload(payload(number, parent, hash));
            parent = hash;
        }

        assert_eq!(catchup.max_payloads(), 4);
        assert_eq!(catchup.payloads.keys().copied().collect::<Vec<_>>(), vec![3, 4, 5, 6]);
        assert_eq!(catchup.observations.len(), 4);
    }

    fn rollup_config(block_time: u64, denim: Option<u64>) -> RollupConfig {
        RollupConfig {
            block_time,
            upgrades: UpgradeConfig {
                base: BaseUpgradeConfig { denim, ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        }
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
        assert!(catchup.contiguous_payloads(safe, usize::MAX).is_empty());

        assert!(catchup.is_complete(safe, safe));
    }

    #[test]
    fn private_head_beyond_last_observed_payload_does_not_complete_catchup() {
        let mut catchup = CanonicalUnsafeCatchup::default();
        catchup.buffer_payload(payload(11, B256::with_last_byte(10), B256::with_last_byte(11)));
        let private = head(12, B256::with_last_byte(99));
        assert!(catchup.contiguous_payloads(private, usize::MAX).is_empty());

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
        assert!(!gate.should_defer_finalized(10, 10));
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
