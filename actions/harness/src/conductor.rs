//! Controllable conductor for action tests.
//!
//! Provides [`TestConductor`] and [`TestConductorHandle`] for testing sequencer
//! behavior under conductor-gated leadership. Multiple sequencers can share a
//! single [`TestConductorHandle`] to coordinate leadership transitions.

use std::sync::{Arc, Mutex};

use alloy_primitives::B256;
use async_trait::async_trait;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use base_consensus_node::{Conductor, ConductorError};

/// Shared mutable state backing a [`TestConductorHandle`].
#[derive(Debug)]
pub struct ConductorState {
    leader_id: Option<u8>,
    committed_payloads: Vec<(u8, B256)>,
}

impl ConductorState {
    const fn new(leader_id: Option<u8>) -> Self {
        Self { leader_id, committed_payloads: Vec::new() }
    }
}

/// Shared handle to a simulated conductor for use in action tests.
///
/// Clone the handle to share leadership state between multiple [`TestConductor`]
/// instances. Use [`conductor`] to mint a per-sequencer [`TestConductor`] that
/// is identified by a sequencer ID.
///
/// [`conductor`]: TestConductorHandle::conductor
#[derive(Debug, Clone)]
pub struct TestConductorHandle(Arc<Mutex<ConductorState>>);

impl TestConductorHandle {
    /// Create a handle with the given initial leader.
    ///
    /// Pass `Some(id)` to make sequencer `id` the initial leader.
    /// Pass `None` to start with no leader.
    pub fn new(leader_id: Option<u8>) -> Self {
        Self(Arc::new(Mutex::new(ConductorState::new(leader_id))))
    }

    /// Convenience constructor: sequencer `0` is the initial leader.
    pub fn with_leader_zero() -> Self {
        Self::new(Some(0))
    }

    /// Transfer leadership to the given sequencer id.
    pub fn set_leader(&self, id: u8) {
        self.0.lock().expect("conductor state lock poisoned").leader_id = Some(id);
    }

    /// Clear leadership (no sequencer is the leader).
    pub fn clear_leader(&self) {
        self.0.lock().expect("conductor state lock poisoned").leader_id = None;
    }

    /// Return the total number of payloads committed across all sequencers.
    pub fn committed_count(&self) -> usize {
        self.0.lock().expect("conductor state lock poisoned").committed_payloads.len()
    }

    /// Return a snapshot of all committed payloads as `(sequencer_id, block_hash)` pairs.
    pub fn committed_payloads(&self) -> Vec<(u8, B256)> {
        self.0.lock().expect("conductor state lock poisoned").committed_payloads.clone()
    }

    /// Return the committed payloads for a specific sequencer id.
    pub fn committed_by(&self, id: u8) -> Vec<B256> {
        self.0
            .lock()
            .expect("conductor state lock poisoned")
            .committed_payloads
            .iter()
            .filter(|(s, _)| *s == id)
            .map(|(_, h)| *h)
            .collect()
    }

    /// Mint a [`TestConductor`] identified by `id`.
    ///
    /// The conductor shares this handle's state. Pass it to
    /// [`L2Sequencer::set_conductor`] to gate block building on leadership.
    ///
    /// [`L2Sequencer::set_conductor`]: crate::L2Sequencer::set_conductor
    pub fn conductor(&self, id: u8) -> TestConductor {
        TestConductor { handle: self.clone(), id }
    }
}

impl Default for TestConductorHandle {
    fn default() -> Self {
        Self::new(Some(0))
    }
}

/// Per-sequencer [`Conductor`] implementation backed by a shared [`TestConductorHandle`].
///
/// Mint one per sequencer via [`TestConductorHandle::conductor`]. Each
/// `TestConductor` has an `id` that identifies which sequencer it represents.
/// Leadership is determined by comparing `id` against the handle's current
/// `leader_id`.
#[derive(Debug, Clone)]
pub struct TestConductor {
    handle: TestConductorHandle,
    id: u8,
}

impl TestConductor {
    /// Return the sequencer id this conductor represents.
    pub const fn id(&self) -> u8 {
        self.id
    }
}

#[async_trait]
impl Conductor for TestConductor {
    async fn leader(&self) -> Result<bool, ConductorError> {
        let state = self.handle.0.lock().expect("conductor state lock poisoned");
        Ok(state.leader_id == Some(self.id))
    }

    async fn active(&self) -> Result<bool, ConductorError> {
        Ok(true)
    }

    async fn commit_unsafe_payload(
        &self,
        payload: &BaseExecutionPayloadEnvelope,
    ) -> Result<(), ConductorError> {
        let mut state = self.handle.0.lock().expect("conductor state lock poisoned");
        if state.leader_id != Some(self.id) {
            return Err(ConductorError::NotLeader);
        }
        let block_hash = payload.execution_payload.as_v1().block_hash;
        state.committed_payloads.push((self.id, block_hash));
        Ok(())
    }

    async fn override_leader(&self) -> Result<(), ConductorError> {
        self.handle.set_leader(self.id);
        Ok(())
    }
}
