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

    /// Returns broadcast hashes with the confirmed receipt hash appended if it is missing.
    pub fn hashes_with_receipt(&self, transaction_hash: B256) -> Vec<B256> {
        let mut hashes = self.hashes();
        if !hashes.contains(&transaction_hash) {
            hashes.push(transaction_hash);
        }
        hashes
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

#[cfg(test)]
mod tests {
    use alloy_json_rpc::{Id, Request, RequestPacket};
    use alloy_primitives::{B256, keccak256};

    use super::BroadcastJournal;

    #[test]
    fn receipt_hash_is_retained_even_without_a_broadcast_record() {
        let journal = BroadcastJournal::default();
        let receipt_hash = B256::repeat_byte(1);
        assert_eq!(journal.hashes_with_receipt(receipt_hash), vec![receipt_hash]);

        let request =
            Request::new("eth_sendRawTransaction", Id::Number(1), ["0x01"]).serialize().unwrap();
        journal.record(&RequestPacket::Single(request));
        let broadcast_hash = keccak256([1]);
        assert_eq!(journal.hashes_with_receipt(receipt_hash), vec![broadcast_hash, receipt_hash]);
        assert_eq!(journal.hashes_with_receipt(broadcast_hash), vec![broadcast_hash]);
    }
}
