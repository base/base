//! Request routing and buffered canonical inputs for shadow reconciliation.

use std::collections::{BTreeMap, VecDeque};

use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use base_consensus_engine::ConsolidateInput;
use base_protocol::L2BlockInfo;

use crate::{EngineClientError, SequencerConfig};

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

    /// Buffers a deferred safe signal.
    pub fn buffer_safe_signal(&mut self, signal: ConsolidateInput) {
        if self.safe_signals.len() >= Self::MAX_SAFE_SIGNALS {
            self.faulted = true;
        } else {
            self.safe_signals.push_back(signal);
        }
    }

    /// Records the latest finalized block.
    pub const fn buffer_finalized(&mut self, number: u64) {
        self.latest_finalized = Some(match self.latest_finalized {
            Some(previous) if previous > number => previous,
            _ => number,
        });
    }

    /// Selects a complete reconciliation range without consuming it.
    pub fn prepare(
        &self,
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
        let mut parent = self.anchor.block_info.hash;
        let mut payloads = Vec::with_capacity(length as usize);
        for number in self.anchor.block_info.number + 1..=shadow_head.block_info.number {
            let Some(payload) = self.payloads.get(&number) else { return Ok(None) };
            if payload.execution_payload.parent_hash() != parent {
                return Err(EngineClientError::InvalidShadowReconciliation(
                    "payload parent continuity mismatch".into(),
                ));
            }
            parent = payload.execution_payload.block_hash();
            payloads.push(payload.clone());
        }
        Ok(Some(CanonicalReconciliationInputs {
            shadow_head,
            payloads,
            safe_signals: self.safe_signals.clone(),
            finalized_block_number: self.latest_finalized,
        }))
    }

    /// Commits a successful reconciliation and advances the anchor.
    pub fn commit(&mut self, head: L2BlockInfo) {
        self.payloads.retain(|number, _| *number > head.block_info.number);
        self.anchor = head;
        self.safe_signals.clear();
        self.latest_finalized = None;
    }

    /// Clears buffered inputs and faults.
    pub fn clear(&mut self) {
        self.payloads.clear();
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
    use base_consensus_engine::ConsolidateInput;
    use base_protocol::{BlockInfo, L2BlockInfo};

    use super::ShadowReconciliationGate;
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
    fn selects_out_of_order_contiguous_range_without_consuming_it() {
        let anchor = head(10, B256::with_last_byte(10));
        let mut gate = ShadowReconciliationGate::new(anchor);
        gate.buffer_payload(payload(12, B256::with_last_byte(11), B256::with_last_byte(12)));
        gate.buffer_payload(payload(11, anchor.block_info.hash, B256::with_last_byte(11)));

        let prepared = gate.prepare(head(12, B256::with_last_byte(99))).unwrap().unwrap();
        assert_eq!(prepared.payloads.len(), 2);
        assert_eq!(gate.payloads.len(), 2);
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
