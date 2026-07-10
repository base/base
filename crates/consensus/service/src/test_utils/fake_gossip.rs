//! Deterministic in-memory gossip transport fakes.
//!
//! This module deliberately avoids libp2p/discv5: messages stay in-process via Tokio channels.
//! It supports drop and reorder injection for deterministic failure simulation.

use std::{collections::VecDeque, sync::Arc};

use alloy_primitives::{Address, B256, Signature, U256};
use async_trait::async_trait;
use base_common_rpc_types_engine::{
    BaseExecutionPayloadEnvelope, NetworkPayloadEnvelope, PayloadHash,
};
use base_consensus_gossip::P2pRpcRequest;
use tokio::sync::{Mutex, mpsc};

use crate::{GossipTransport, UnsafePayloadGossipClient, UnsafePayloadGossipClientError};

/// Fake gossip error type.
#[derive(Clone, Debug)]
pub enum FakeGossipError {
    /// Inbound/outbound channel closed.
    Closed,
}

#[derive(Debug, Default)]
struct FakeGossipState {
    signer: Address,
    drop_next: usize,
    reorder_pattern: Vec<usize>,
    outbound: VecDeque<NetworkPayloadEnvelope>,
}

/// Shared handle to configure and inspect fake gossip behavior.
#[derive(Clone, Debug)]
pub struct FakeGossipHandle {
    state: Arc<Mutex<FakeGossipState>>,
}

impl FakeGossipHandle {
    /// Drops the next `count` published messages.
    pub async fn drop_next(&self, count: usize) {
        self.state.lock().await.drop_next = count;
    }

    /// Reorders outbound deliveries according to `pattern` indexes.
    pub async fn reorder(&self, pattern: Vec<usize>) {
        self.state.lock().await.reorder_pattern = pattern;
    }
}

/// In-memory implementation of [`GossipTransport`] and [`UnsafePayloadGossipClient`].
#[derive(Debug)]
pub struct FakeGossipTransport {
    state: Arc<Mutex<FakeGossipState>>,
    inbound_rx: mpsc::Receiver<NetworkPayloadEnvelope>,
    inbound_tx: mpsc::Sender<NetworkPayloadEnvelope>,
}

impl FakeGossipTransport {
    /// Creates a new in-memory gossip transport.
    pub fn new(buffer: usize) -> Self {
        let (inbound_tx, inbound_rx) = mpsc::channel(buffer);
        Self {
            state: Arc::new(Mutex::new(FakeGossipState::default())),
            inbound_rx,
            inbound_tx,
        }
    }

    /// Returns a shared control handle.
    pub fn handle(&self) -> FakeGossipHandle {
        FakeGossipHandle { state: Arc::clone(&self.state) }
    }

    async fn enqueue_network_payload(&self, payload: NetworkPayloadEnvelope) -> Result<(), FakeGossipError> {
        let mut state = self.state.lock().await;
        if state.drop_next > 0 {
            state.drop_next -= 1;
            return Ok(());
        }
        state.outbound.push_back(payload);

        if !state.reorder_pattern.is_empty() {
            let source = state.outbound.iter().cloned().collect::<Vec<_>>();
            let mut reordered = VecDeque::new();
            for idx in &state.reorder_pattern {
                if let Some(item) = source.get(*idx).cloned() {
                    reordered.push_back(item);
                }
            }
            state.outbound = reordered;
            state.reorder_pattern.clear();
        }

        while let Some(next) = state.outbound.pop_front() {
            self.inbound_tx.send(next).await.map_err(|_| FakeGossipError::Closed)?;
        }
        Ok(())
    }
}

#[async_trait]
impl GossipTransport for FakeGossipTransport {
    type Error = FakeGossipError;

    async fn publish(&mut self, payload: BaseExecutionPayloadEnvelope) -> Result<(), Self::Error> {
        let network_payload = NetworkPayloadEnvelope {
            payload: payload.execution_payload,
            signature: Signature::new(U256::ZERO, U256::ZERO, false),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: payload.parent_beacon_block_root,
        };
        self.enqueue_network_payload(network_payload).await
    }

    async fn next_unsafe_block(&mut self) -> Option<NetworkPayloadEnvelope> {
        self.inbound_rx.recv().await
    }

    fn set_block_signer(&mut self, address: Address) {
        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            state.lock().await.signer = address;
        });
    }

    fn clear_pending_connections(&mut self) -> usize {
        0
    }

    fn handle_p2p_rpc(&mut self, _request: P2pRpcRequest) {}
}

#[async_trait]
impl UnsafePayloadGossipClient for FakeGossipTransport {
    async fn schedule_execution_payload_gossip(
        &self,
        payload: BaseExecutionPayloadEnvelope,
    ) -> Result<(), UnsafePayloadGossipClientError> {
        let network_payload = NetworkPayloadEnvelope {
            payload: payload.execution_payload,
            signature: Signature::new(U256::ZERO, U256::ZERO, false),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: payload.parent_beacon_block_root,
        };
        self.enqueue_network_payload(network_payload)
            .await
            .map_err(|_| UnsafePayloadGossipClientError::RequestError("gossip queue closed".to_string()))
    }
}
