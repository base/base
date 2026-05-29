//! B-20 precompile token transfer payload for load testing.

use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes, U256};
use alloy_rpc_types::TransactionRequest;
use alloy_sol_types::SolCall;
use base_common_precompiles::IB20;

use super::Payload;
use crate::workload::SeededRng;

/// Generates B-20 precompile token transfer transactions.
///
/// During the load phase, each generated transaction calls `transfer(address,uint256)` on the
/// configured B-20 token precompile. The B-20 `transfer` selector is ERC-20 compatible, so this
/// exercises the precompile's state-mutation code path (balance updates, event emission) under
/// sustained load.
#[derive(Debug, Clone)]
pub struct B20TransferPayload {
    /// B-20 token precompile address.
    pub token_address: Address,
    /// Minimum transfer amount.
    pub min_amount: U256,
    /// Maximum transfer amount.
    pub max_amount: U256,
}

impl B20TransferPayload {
    /// Creates a new B-20 transfer payload.
    pub const fn new(token_address: Address, min_amount: U256, max_amount: U256) -> Self {
        Self { token_address, min_amount, max_amount }
    }
}

impl Payload for B20TransferPayload {
    fn name(&self) -> &'static str {
        "b20"
    }

    fn generate(&self, rng: &mut SeededRng, _from: Address, to: Address) -> TransactionRequest {
        let amount = if self.min_amount == self.max_amount {
            self.min_amount
        } else {
            let min: u128 = self.min_amount.try_into().expect("b20 min_amount must fit in u128");
            let max: u128 = self.max_amount.try_into().expect("b20 max_amount must fit in u128");
            U256::from(rng.gen_range(min..=max))
        };

        let call = IB20::transferCall { to, amount };

        TransactionRequest::default()
            .with_to(self.token_address)
            .with_input(Bytes::from(call.abi_encode()))
            .with_gas_limit(100_000)
    }
}
