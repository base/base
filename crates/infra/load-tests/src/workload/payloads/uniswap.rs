use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes, U160, U256, Uint};
use alloy_rpc_types::TransactionRequest;
use alloy_sol_types::{SolCall, sol};

type U24 = Uint<24, 1>;

use super::Payload;
use crate::workload::SeededRng;

sol! {
    interface IUniswapV3Router {
        struct ExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint24 fee;
            address recipient;
            uint256 amountIn;
            uint256 amountOutMinimum;
            uint160 sqrtPriceLimitX96;
        }

        function exactInputSingle(
            ExactInputSingleParams calldata params
        ) external payable returns (uint256 amountOut);
    }
}

/// Generates Uniswap V3 style swap transactions.
#[derive(Debug, Clone)]
pub struct UniswapV3Payload {
    router: Address,
    token_in: Address,
    token_out: Address,
    fee: u32,
    min_amount: U256,
    max_amount: U256,
    reverse_min_amount: U256,
    reverse_max_amount: U256,
}

impl UniswapV3Payload {
    /// Creates a new `UniswapV3` payload.
    pub const fn new(
        router: Address,
        token_in: Address,
        token_out: Address,
        fee: u32,
        min_amount: U256,
        max_amount: U256,
        reverse_amounts: Option<(U256, U256)>,
    ) -> Self {
        let (reverse_min_amount, reverse_max_amount) = match reverse_amounts {
            Some(amounts) => amounts,
            None => (min_amount, max_amount),
        };
        Self {
            router,
            token_in,
            token_out,
            fee,
            min_amount,
            max_amount,
            reverse_min_amount,
            reverse_max_amount,
        }
    }
}

impl Payload for UniswapV3Payload {
    fn name(&self) -> &'static str {
        "uniswap_v3"
    }

    fn generate(&self, rng: &mut SeededRng, from: Address, _to: Address) -> TransactionRequest {
        // Randomly swap direction to exercise both sides of the pool.
        // V3 pools are keyed by (token0, token1, fee) with token0 < token1,
        // so the fee tier is direction-agnostic and this is safe.
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

        let call = IUniswapV3Router::exactInputSingleCall {
            params: IUniswapV3Router::ExactInputSingleParams {
                tokenIn: input,
                tokenOut: output,
                fee: U24::from(self.fee),
                recipient: from,
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
        let payload = UniswapV3Payload::new(
            router,
            token_in,
            token_out,
            500,
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
            let decoded = IUniswapV3Router::exactInputSingleCall::abi_decode(input)
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
