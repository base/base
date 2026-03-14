use alloy_primitives::{Address, U256};

use super::Payload;
use crate::{rpc::TransactionRequest, workload::SeededRng};

/// Generates simple ETH transfer transactions.
#[derive(Debug, Clone)]
pub struct TransferPayload {
    /// Minimum value to transfer.
    pub min_value: U256,
    /// Maximum value to transfer.
    pub max_value: U256,
}

impl TransferPayload {
    /// Creates a new transfer payload with min and max values.
    pub const fn new(min_value: U256, max_value: U256) -> Self {
        Self { min_value, max_value }
    }

    /// Creates a transfer payload with a fixed value.
    pub const fn fixed(value: U256) -> Self {
        Self { min_value: value, max_value: value }
    }
}

impl Default for TransferPayload {
    fn default() -> Self {
        Self {
            min_value: U256::from(1_000_000_000_000_000u64),
            max_value: U256::from(10_000_000_000_000_000u64),
        }
    }
}

impl Payload for TransferPayload {
    fn name(&self) -> &'static str {
        "transfer"
    }

    fn generate(&self, rng: &mut SeededRng, _from: Address, to: Address) -> TransactionRequest {
        let value = if self.min_value == self.max_value {
            self.min_value
        } else {
            let min: u128 = self.min_value.try_into().unwrap_or(u128::MAX);
            let max: u128 = self.max_value.try_into().unwrap_or(u128::MAX);
            U256::from(rng.gen_range(min..=max))
        };

        TransactionRequest::transfer(to, value)
    }
}
