use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes};
use alloy_rpc_types::TransactionRequest;
use alloy_sol_types::{SolCall, sol};
use async_trait::async_trait;

use super::Payload;
use crate::workload::SeededRng;

/// Conservative gas limit for a counter increment that updates one storage slot.
pub const DOUBLE_COUNTER_GAS_LIMIT: u64 = 100_000;

/// Generates deterministic calls to a `DoubleCounter` contract's `increment2()` function.
#[derive(Debug, Clone)]
pub struct DoubleCounterPayload {
    /// `DoubleCounter` contract address.
    pub contract: Address,
}

impl DoubleCounterPayload {
    /// Creates a payload targeting `contract`.
    pub const fn new(contract: Address) -> Self {
        Self { contract }
    }

    /// ABI-encodes `increment2()`.
    pub fn encode_increment2() -> Bytes {
        sol! {
            function increment2() external;
        }
        Bytes::from(increment2Call {}.abi_encode())
    }
}

#[async_trait]
impl Payload for DoubleCounterPayload {
    fn name(&self) -> &'static str {
        "double_counter"
    }

    fn uses_runner_recipient(&self) -> bool {
        false
    }

    fn generate(&self, _rng: &mut SeededRng, _from: Address, _to: Address) -> TransactionRequest {
        TransactionRequest::default()
            .with_to(self.contract)
            .with_input(Self::encode_increment2())
            .with_gas_limit(DOUBLE_COUNTER_GAS_LIMIT)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    #[test]
    fn generates_increment2_call_for_configured_contract() {
        let contract = address!("1111111111111111111111111111111111111111");
        let request = DoubleCounterPayload::new(contract).generate(
            &mut SeededRng::new(1),
            Address::ZERO,
            Address::ZERO,
        );

        assert_eq!(request.to, Some(contract.into()));
        let expected = DoubleCounterPayload::encode_increment2();
        assert_eq!(request.input.input(), Some(&expected));
        assert_eq!(request.gas, Some(DOUBLE_COUNTER_GAS_LIMIT));
    }
}
