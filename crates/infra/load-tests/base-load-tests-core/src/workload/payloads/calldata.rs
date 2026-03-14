use alloy_primitives::{Address, Bytes, U256};

use super::Payload;
use crate::{rpc::TransactionRequest, workload::SeededRng};

const GAS_PER_NONZERO_BYTE: u64 = 16;

/// Generates ETH transfer transactions with random calldata.
#[derive(Debug, Clone)]
pub struct CalldataPayload {
    /// Maximum calldata size in bytes.
    pub max_size: usize,
    /// Minimum calldata size in bytes.
    pub min_size: usize,
}

impl CalldataPayload {
    /// Creates a new calldata payload with the given maximum size.
    pub const fn new(max_size: usize) -> Self {
        Self { max_size, min_size: 0 }
    }

    /// Sets the minimum calldata size.
    pub const fn with_min_size(mut self, min_size: usize) -> Self {
        self.min_size = min_size;
        self
    }
}

impl Default for CalldataPayload {
    fn default() -> Self {
        Self { max_size: 128, min_size: 0 }
    }
}

impl Payload for CalldataPayload {
    fn name(&self) -> &'static str {
        "calldata"
    }

    fn generate(&self, rng: &mut SeededRng, _from: Address, to: Address) -> TransactionRequest {
        let size = if self.min_size == self.max_size {
            self.max_size
        } else {
            rng.gen_range(self.min_size..=self.max_size)
        };

        let data: Vec<u8> = (0..size).map(|_| rng.gen_range(0..=255)).collect();
        let gas_limit = 21_000 + (size as u64 * GAS_PER_NONZERO_BYTE);

        TransactionRequest {
            to,
            value: U256::ZERO,
            data: Bytes::from(data),
            gas_limit: Some(gas_limit),
            nonce: None,
        }
    }
}
