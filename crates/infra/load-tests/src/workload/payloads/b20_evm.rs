//! B-20 EVM contract token transfer payload for load testing.

use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes, U256};
use alloy_rpc_types::TransactionRequest;
use alloy_sol_types::SolCall;
use base_common_precompiles::IB20;

use super::Payload;
use crate::workload::SeededRng;

/// Generates B-20 EVM contract token transfer transactions against a fixed pre-deployed contract.
///
/// Unlike the precompile variant, every sender transfers from the same shared token contract that
/// was deployed and funded at the DB level before the benchmark run. No per-run setup is needed.
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
    /// Creates a new payload bound to a pre-deployed EVM token contract.
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
            let min: u128 = self.min_amount.try_into().expect("b20_evm min_amount must fit in u128");
            let max: u128 = self.max_amount.try_into().expect("b20_evm max_amount must fit in u128");
            U256::from(rng.gen_range(min..=max))
        };

        let call = IB20::transferCall { to, amount };

        TransactionRequest::default()
            .with_to(self.contract)
            .with_input(Bytes::from(call.abi_encode()))
            .with_gas_limit(100_000)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, U256};

    use super::*;
    use crate::workload::SeededRng;

    #[test]
    fn generate_targets_fixed_contract() {
        let contract = address!("b200000000000000000000000000000000000ee2");
        let sender = address!("00000000000000000000000000000000000000a1");
        let recipient = address!("00000000000000000000000000000000000000b2");
        let payload =
            B20EvmTransferPayload::new(contract, U256::from(1000), U256::from(1000));
        let mut rng = SeededRng::new(7);

        let tx = payload.generate(&mut rng, sender, recipient);

        assert_eq!(
            tx.to,
            Some(alloy_primitives::TxKind::Call(contract)),
            "transfer must target the fixed EVM contract, not the sender"
        );

        let expected = IB20::transferCall { to: recipient, amount: U256::from(1000) };
        assert_eq!(
            tx.input.input().expect("input set").as_ref(),
            expected.abi_encode().as_slice(),
        );
    }
}
