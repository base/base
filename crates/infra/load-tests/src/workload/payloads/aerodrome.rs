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
    /// Minimum amount when swapping `token_out` to `token_in`.
    pub reverse_min_amount: U256,
    /// Maximum amount when swapping `token_out` to `token_in`.
    pub reverse_max_amount: U256,
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
        reverse_amounts: Option<(U256, U256)>,
    ) -> Self {
        let tick_spacing =
            I24::try_from(tick_spacing).expect("tick_spacing validated to fit i24 at config parse");
        let (reverse_min_amount, reverse_max_amount) =
            reverse_amounts.unwrap_or((min_amount, max_amount));
        Self {
            router,
            token_in,
            token_out,
            tick_spacing,
            min_amount,
            max_amount,
            reverse_min_amount,
            reverse_max_amount,
        }
    }
}

impl Payload for AerodromeClPayload {
    fn name(&self) -> &'static str {
        "aerodrome_cl"
    }

    fn generate(&self, rng: &mut SeededRng, from: Address, _to: Address) -> TransactionRequest {
        let (input, output, min_amount, max_amount) = if rng.random::<bool>() {
            (self.token_in, self.token_out, self.min_amount, self.max_amount)
        } else {
            (self.token_out, self.token_in, self.reverse_min_amount, self.reverse_max_amount)
        };

        let amount = if min_amount == max_amount {
            min_amount
        } else {
            let min: u128 = min_amount.try_into().expect("validated <= u128::MAX at config parse");
            let max: u128 = max_amount.try_into().expect("validated <= u128::MAX at config parse");
            U256::from(rng.gen_range(min..=max))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_calldata_uses_direction_specific_amounts() {
        let router = Address::repeat_byte(0x10);
        let token_in = Address::repeat_byte(0x11);
        let token_out = Address::repeat_byte(0x22);
        let sender = Address::repeat_byte(0x33);
        let forward_amount = U256::from(10_000_000_000_000u64);
        let reverse_amount = U256::from(100_000u64);
        let payload = AerodromeClPayload::new(
            router,
            token_in,
            token_out,
            100,
            forward_amount,
            forward_amount,
            Some((reverse_amount, reverse_amount)),
        );
        let mut rng = SeededRng::new(42);
        let mut saw_forward = false;
        let mut saw_reverse = false;

        for _ in 0..32 {
            let tx = payload.generate(&mut rng, sender, Address::ZERO);
            let input = tx.input.input().expect("swap calldata should be set");
            let decoded = IAerodromeClRouter::exactInputSingleCall::abi_decode(input)
                .expect("swap calldata should decode");

            if decoded.params.tokenIn == token_in {
                saw_forward = true;
                assert_eq!(decoded.params.tokenOut, token_out);
                assert_eq!(decoded.params.amountIn, forward_amount);
            } else if decoded.params.tokenIn == token_out {
                saw_reverse = true;
                assert_eq!(decoded.params.tokenOut, token_in);
                assert_eq!(decoded.params.amountIn, reverse_amount);
            } else {
                panic!("unexpected tokenIn {}", decoded.params.tokenIn);
            }
        }

        assert!(saw_forward, "expected at least one forward swap");
        assert!(saw_reverse, "expected at least one reverse swap");
    }
}
