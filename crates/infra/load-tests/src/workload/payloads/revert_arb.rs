use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes};
use alloy_rpc_types::TransactionRequest;
use async_trait::async_trait;

use super::Payload;
use crate::workload::SeededRng;

/// Default declared gas limit for a revert arb: sized like a searcher's execution path even
/// though the call reverts after the intrinsic cost plus a selector dispatch.
pub const DEFAULT_REVERT_ARB_GAS_LIMIT: u64 = 2_000_000;

/// Function selector no deployed test contract implements, so the call reverts at dispatch.
pub const REVERT_ARB_SELECTOR: [u8; 4] = [0xde, 0xad, 0xbe, 0xef];

/// Generates calls that revert at the target contract's selector dispatch.
///
/// Models the gas-limit-pricing adversary: a high-tip transaction that reserves a large
/// `gas_limit`, passes its validity predicates, and reverts after ~21-25k gas. Against a
/// contract without a fallback (such as `DoubleCounter`) the unknown selector reverts
/// immediately, so under metered billing the reverting arb pays for almost none of the gas it reserved.
#[derive(Debug, Clone)]
pub struct RevertArbPayload {
    /// Contract whose selector dispatch rejects the reverting arb.
    pub contract: Address,
    /// Declared gas limit carried by every reverting arb.
    pub gas_limit: u64,
}

impl RevertArbPayload {
    /// Creates a reverting arb against `contract` that declares `gas_limit`.
    pub const fn new(contract: Address, gas_limit: u64) -> Self {
        Self { contract, gas_limit }
    }
}

#[async_trait]
impl Payload for RevertArbPayload {
    fn name(&self) -> &'static str {
        "revert_arb"
    }

    fn uses_runner_recipient(&self) -> bool {
        false
    }

    fn generate(&self, _rng: &mut SeededRng, _from: Address, _to: Address) -> TransactionRequest {
        TransactionRequest::default()
            .with_to(self.contract)
            .with_input(Bytes::from_static(&REVERT_ARB_SELECTOR))
            .with_gas_limit(self.gas_limit)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{U256, address};

    use super::*;

    #[test]
    fn generates_unknown_selector_call_with_configured_gas_limit() {
        let contract = address!("1111111111111111111111111111111111111111");
        let runner_recipient = address!("2222222222222222222222222222222222222222");
        let request = RevertArbPayload::new(contract, 1_500_000).generate(
            &mut SeededRng::new(1),
            Address::ZERO,
            runner_recipient,
        );

        assert_eq!(request.to, Some(contract.into()), "reverting arb ignores the runner recipient");
        assert_eq!(request.input.input(), Some(&Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef])));
        assert_eq!(request.gas, Some(1_500_000));
        assert_eq!(request.value.unwrap_or(U256::ZERO), U256::ZERO);
    }
}
