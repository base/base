use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes, U256};
use alloy_rpc_types::TransactionRequest;
use alloy_sol_types::{SolCall, sol};

use super::Payload;
use crate::workload::SeededRng;

/// Gas budgeted per fresh storage slot. A cold zero -> non-zero SSTORE is 22,100
/// gas; the extra ~900 covers the per-iteration loop, keccak mixing, and warm
/// SLOAD overhead so the tx does not run out of gas on the last slots.
const STORAGE_GAS_PER_SLOT: u64 = 23_000;

/// Fixed per-tx gas overhead: 21,000 intrinsic + calldata + selector dispatch +
/// the single keccak256 over the slot-key preimage.
const STORAGE_GAS_BASE: u64 = 30_000;

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
        sol! {
            function fillStorage(uint256 slotCount, uint256 seed) external;
        }
        Bytes::from(
            fillStorageCall { slotCount: U256::from(slot_count), seed: U256::from(seed) }
                .abi_encode(),
        )
    }
}

impl Payload for StoragePayload {
    fn name(&self) -> &'static str {
        "storage"
    }

    fn uses_runner_recipient(&self) -> bool {
        false
    }

    fn generate(&self, rng: &mut SeededRng, _from: Address, _to: Address) -> TransactionRequest {
        let seed: u64 = rng.random();
        let data = Self::encode_fill_storage(self.slots_per_tx, seed);

        TransactionRequest::default()
            .with_to(self.contract)
            .with_input(data)
            .with_gas_limit(u64::from(self.slots_per_tx) * STORAGE_GAS_PER_SLOT + STORAGE_GAS_BASE)
    }
}
