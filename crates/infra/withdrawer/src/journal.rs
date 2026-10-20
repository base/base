use std::sync::{Arc, Mutex};

use alloy_json_rpc::{RequestPacket, SerializedRequest};
use alloy_primitives::{B256, Bytes, keccak256};

/// Shared broadcast hashes retained across transport failures and send cancellation.
#[derive(Debug, Clone, Default)]
pub struct BroadcastJournal {
    hashes: Arc<Mutex<Vec<B256>>>,
}

impl BroadcastJournal {
    /// Returns the distinct hashes in broadcast order.
    pub fn hashes(&self) -> Vec<B256> {
        self.hashes.lock().expect("broadcast journal lock poisoned").clone()
    }

    /// Starts a new serialized submission.
    pub fn clear(&self) {
        self.hashes.lock().expect("broadcast journal lock poisoned").clear();
    }

    /// Records raw transaction hashes before forwarding a single request or batch.
    pub fn record(&self, packet: &RequestPacket) {
        let requests = match packet {
            RequestPacket::Single(request) => std::slice::from_ref(request),
            RequestPacket::Batch(requests) => requests.as_slice(),
        };
        for request in requests {
            self.record_request(request);
        }
    }

    /// Ignores malformed parameters without changing the outgoing request.
    pub fn record_request(&self, request: &SerializedRequest) {
        if request.method() != "eth_sendRawTransaction" {
            return;
        }
        let Some(params) = request.params() else { return };
        let Ok([raw]) = serde_json::from_str::<[Bytes; 1]>(params.get()) else { return };
        let hash = keccak256(raw);
        let mut hashes = self.hashes.lock().expect("broadcast journal lock poisoned");
        if !hashes.contains(&hash) {
            hashes.push(hash);
        }
    }
}
