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
    ) -> Self {
        Self { router, token_in, token_out, fee, min_amount, max_amount }
    }
}

impl Payload for UniswapV3Payload {
    fn name(&self) -> &'static str {
        "uniswap_v3"
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

        // Randomly swap direction to exercise both sides of the pool.
        // V3 pools are keyed by (token0, token1, fee) with token0 < token1,
        // so the fee tier is direction-agnostic and this is safe.
        let (input, output) = if rng.random::<bool>() {
            (self.token_in, self.token_out)
        } else {
            (self.token_out, self.token_in)
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
