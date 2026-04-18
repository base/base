use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes};
use alloy_rpc_types::TransactionRequest;

use super::Payload;
use crate::workload::SeededRng;

const GAS_PER_ZERO_BYTE: u64 = 4;
const GAS_PER_NONZERO_BYTE: u64 = 16;
const TOKENS_PER_NONZERO_BYTE: u64 = GAS_PER_NONZERO_BYTE / GAS_PER_ZERO_BYTE;

/// EIP-7623 floor cost per calldata token (active on Prague / Base Isthmus+).
const EIP7623_FLOOR_COST_PER_TOKEN: u64 = 10;

/// Generates ETH transfer transactions with random calldata.
#[derive(Debug, Clone)]
pub struct CalldataPayload {
    /// Maximum calldata size in bytes.
    pub max_size: usize,
    /// Minimum calldata size in bytes.
    pub min_size: usize,
    /// Number of times to repeat the random sequence (1 = no repetition).
    /// Higher values produce more compressible data.
    pub repeat_count: usize,
}

impl CalldataPayload {
    /// Creates a new calldata payload with the given maximum size.
    pub const fn new(max_size: usize) -> Self {
        Self { max_size, min_size: 0, repeat_count: 1 }
    }

    /// Sets the minimum calldata size.
    pub const fn with_min_size(mut self, min_size: usize) -> Self {
        self.min_size = min_size;
        self
    }

    /// Sets the repeat count for compressibility (1 = no repetition).
    pub const fn with_repeat_count(mut self, repeat_count: usize) -> Self {
        self.repeat_count = if repeat_count == 0 { 1 } else { repeat_count };
        self
    }
}

impl Default for CalldataPayload {
    fn default() -> Self {
        Self { max_size: 128, min_size: 0, repeat_count: 1 }
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

        let data: Vec<u8> = if self.repeat_count <= 1 {
            (0..size).map(|_| rng.gen_range(0..=255)).collect()
        } else {
            let chunk_size = size.div_ceil(self.repeat_count);
            let chunk: Vec<u8> = (0..chunk_size).map(|_| rng.gen_range(0..=255)).collect();
            chunk.iter().cycle().take(size).copied().collect()
        };
        let zero_bytes = data.iter().filter(|&&b| b == 0).count() as u64;
        let nonzero_bytes = data.len() as u64 - zero_bytes;

        let intrinsic_gas =
            21_000 + zero_bytes * GAS_PER_ZERO_BYTE + nonzero_bytes * GAS_PER_NONZERO_BYTE;

        let tokens = zero_bytes + nonzero_bytes * TOKENS_PER_NONZERO_BYTE;
        let floor_gas = 21_000 + tokens * EIP7623_FLOOR_COST_PER_TOKEN;

        let gas_limit = intrinsic_gas.max(floor_gas);

        TransactionRequest::default()
            .with_to(to)
            .with_input(Bytes::from(data))
            .with_gas_limit(gas_limit)
    }
}
