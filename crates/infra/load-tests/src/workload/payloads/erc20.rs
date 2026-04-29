use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes, U256};
use alloy_rpc_types::TransactionRequest;

use super::Payload;
use crate::workload::SeededRng;

/// Generates ERC20 transfer transactions.
#[derive(Debug, Clone)]
pub struct Erc20Payload {
    /// Token contract address.
    pub token_address: Address,
    /// Minimum amount to transfer.
    pub min_amount: U256,
    /// Maximum amount to transfer.
    pub max_amount: U256,
}

impl Erc20Payload {
    /// Creates a new ERC20 payload.
    pub const fn new(token_address: Address, min_amount: U256, max_amount: U256) -> Self {
        Self { token_address, min_amount, max_amount }
    }

    fn encode_transfer(to: Address, amount: U256) -> Bytes {
        let mut data = Vec::with_capacity(68);
        data.extend_from_slice(&[0xa9, 0x05, 0x9c, 0xbb]);
        data.extend_from_slice(&[0u8; 12]);
        data.extend_from_slice(to.as_slice());
        data.extend_from_slice(&amount.to_be_bytes::<32>());
        Bytes::from(data)
    }
}

impl Payload for Erc20Payload {
    fn name(&self) -> &'static str {
        "erc20"
    }

    fn generate(&self, rng: &mut SeededRng, _from: Address, to: Address) -> TransactionRequest {
        let amount = if self.min_amount == self.max_amount {
            self.min_amount
        } else {
            let min: u128 = self.min_amount.try_into().expect("erc20 min_amount must fit in u128");
            let max: u128 = self.max_amount.try_into().expect("erc20 max_amount must fit in u128");
            U256::from(rng.gen_range(min..=max))
        };

        let data = Self::encode_transfer(to, amount);

        TransactionRequest::default()
            .with_to(self.token_address)
            .with_input(data)
            .with_gas_limit(65_000)
    }
}
