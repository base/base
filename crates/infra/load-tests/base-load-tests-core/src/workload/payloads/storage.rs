use alloy_primitives::{Address, Bytes, U256};

use super::Payload;
use crate::{rpc::TransactionRequest, workload::SeededRng};

/// Generates storage-heavy transactions that write to contract slots.
#[derive(Debug, Clone)]
pub struct StoragePayload {
    /// Contract address to call.
    pub contract: Address,
    /// Number of storage slots to write.
    pub slots_per_tx: u32,
}

impl StoragePayload {
    /// Creates a new storage payload.
    pub const fn new(contract: Address, slots_per_tx: u32) -> Self {
        Self { contract, slots_per_tx }
    }

    fn encode_fill_storage(slot_count: u32, seed: u64) -> Bytes {
        let mut data = Vec::with_capacity(4 + 32 + 32);
        data.extend_from_slice(&[0x12, 0x34, 0x56, 0x78]);
        data.extend_from_slice(&U256::from(slot_count).to_be_bytes::<32>());
        data.extend_from_slice(&U256::from(seed).to_be_bytes::<32>());
        Bytes::from(data)
    }
}

impl Payload for StoragePayload {
    fn name(&self) -> &'static str {
        "storage"
    }

    fn generate(&self, rng: &mut SeededRng, _from: Address, _to: Address) -> TransactionRequest {
        let seed: u64 = rng.random();
        let data = Self::encode_fill_storage(self.slots_per_tx, seed);

        TransactionRequest::contract_call(self.contract, data)
            .with_gas_limit(u64::from(self.slots_per_tx) * 22_000 + 21_000)
    }
}
