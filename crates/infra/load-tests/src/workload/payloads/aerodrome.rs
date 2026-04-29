use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes, Signed, U160, U256};
use alloy_rpc_types::TransactionRequest;
use alloy_sol_types::{SolCall, sol};

use super::Payload;
use crate::workload::SeededRng;

type I24 = Signed<24, 1>;

sol! {
    interface IAerodromeClRouter {
        struct ExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            int24 tickSpacing;
            address recipient;
            uint256 deadline;
            uint256 amountIn;
            uint256 amountOutMinimum;
            uint160 sqrtPriceLimitX96;
        }

        function exactInputSingle(
            ExactInputSingleParams calldata params
        ) external payable returns (uint256 amountOut);
    }
}

/// Generates Aerodrome Slipstream (concentrated liquidity) swap transactions.
#[derive(Debug, Clone)]
pub struct AerodromeClPayload {
    /// CL Router contract address.
    pub router: Address,
    /// Input token address.
    pub token_in: Address,
    /// Output token address.
    pub token_out: Address,
    /// Tick spacing (pre-converted to `i24` at construction time).
    pub tick_spacing: I24,
    /// Minimum swap amount.
    pub min_amount: U256,
    /// Maximum swap amount.
    pub max_amount: U256,
}

impl AerodromeClPayload {
    /// Creates a new `AerodromeCl` payload.
    ///
    /// # Panics
    ///
    /// Panics if `tick_spacing` does not fit in an `i24`. Callers must validate
    /// the range before calling (config parsing validates this).
    pub fn new(
        router: Address,
        token_in: Address,
        token_out: Address,
        tick_spacing: i32,
        min_amount: U256,
        max_amount: U256,
    ) -> Self {
        let tick_spacing =
            I24::try_from(tick_spacing).expect("tick_spacing validated to fit i24 at config parse");
        Self { router, token_in, token_out, tick_spacing, min_amount, max_amount }
    }
}

impl Payload for AerodromeClPayload {
    fn name(&self) -> &'static str {
        "aerodrome_cl"
    }

    fn generate(&self, rng: &mut SeededRng, from: Address, _to: Address) -> TransactionRequest {
        let amount = if self.min_amount == self.max_amount {
            self.min_amount
        } else {
            let min: u128 =
                self.min_amount.try_into().expect("validated <= u128::MAX at config parse");
            let max: u128 =
                self.max_amount.try_into().expect("validated <= u128::MAX at config parse");
            U256::from(rng.gen_range(min..=max))
        };

        let (input, output) = if rng.random::<bool>() {
            (self.token_in, self.token_out)
        } else {
            (self.token_out, self.token_in)
        };

        let call = IAerodromeClRouter::exactInputSingleCall {
            params: IAerodromeClRouter::ExactInputSingleParams {
                tokenIn: input,
                tokenOut: output,
                tickSpacing: self.tick_spacing,
                recipient: from,
                deadline: U256::from(u64::MAX),
                amountIn: amount,
                amountOutMinimum: U256::ZERO,
                sqrtPriceLimitX96: U160::ZERO,
            },
        };

        TransactionRequest::default()
            .with_to(self.router)
            .with_input(Bytes::from(call.abi_encode()))
            .with_gas_limit(250_000)
    }
}
