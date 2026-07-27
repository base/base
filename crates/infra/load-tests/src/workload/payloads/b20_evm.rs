//! B-20 EVM contract token transfer payload for load testing.

use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes, U256};
use alloy_rpc_types::TransactionRequest;
use alloy_sol_types::SolCall;
use base_common_precompiles::IB20;

use super::Payload;
use crate::workload::SeededRng;

/// Generates transfers against a fixed pre-deployed B-20 EVM contract.
#[derive(Debug, Clone)]
pub struct B20EvmTransferPayload {
    /// Pre-deployed EVM token contract address.
    pub contract: Address,
    /// Minimum transfer amount.
    pub min_amount: U256,
    /// Maximum transfer amount.
    pub max_amount: U256,
}

impl B20EvmTransferPayload {
    /// Creates a payload bound to a pre-deployed EVM token contract.
    pub const fn new(contract: Address, min_amount: U256, max_amount: U256) -> Self {
        Self { contract, min_amount, max_amount }
    }
}

impl Payload for B20EvmTransferPayload {
    fn name(&self) -> &'static str {
        "b20_evm"
    }

    fn uses_runner_recipient(&self) -> bool {
        true
    }

    fn generate(&self, rng: &mut SeededRng, _from: Address, to: Address) -> TransactionRequest {
        let amount = if self.min_amount == self.max_amount {
            self.min_amount
        } else {
            let min: u128 =
                self.min_amount.try_into().expect("b20_evm min_amount must fit in u128");
            let max: u128 =
                self.max_amount.try_into().expect("b20_evm max_amount must fit in u128");
            U256::from(rng.gen_range(min..=max))
        };
        let call = IB20::transferCall { to, amount };

        TransactionRequest::default()
            .with_to(self.contract)
            .with_input(Bytes::from(call.abi_encode()))
            .with_gas_limit(100_000)
    }
}
