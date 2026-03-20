use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes, U160, U256, Uint};
use alloy_rpc_types::TransactionRequest;
use alloy_sol_types::{SolCall, sol};

type U24 = Uint<24, 1>;

use super::Payload;
use crate::workload::SeededRng;

sol! {
    interface IUniswapV2Router {
        function swapExactETHForTokens(
            uint256 amountOutMin,
            address[] calldata path,
            address to,
            uint256 deadline
        ) external payable returns (uint256[] memory amounts);
    }

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

/// Generates Uniswap V2 style swap transactions.
#[derive(Debug, Clone)]
pub struct UniswapV2Payload {
    router: Address,
    weth: Address,
    token: Address,
    min_amount: U256,
    max_amount: U256,
}

impl UniswapV2Payload {
    /// Creates a new `UniswapV2` payload.
    pub const fn new(
        router: Address,
        weth: Address,
        token: Address,
        min_amount: U256,
        max_amount: U256,
    ) -> Self {
        Self { router, weth, token, min_amount, max_amount }
    }
}

impl Payload for UniswapV2Payload {
    fn name(&self) -> &'static str {
        "uniswap_v2"
    }

    fn generate(&self, rng: &mut SeededRng, _from: Address, to: Address) -> TransactionRequest {
        let amount = if self.min_amount == self.max_amount {
            self.min_amount
        } else {
            let min: u128 = self.min_amount.try_into().unwrap_or(u128::MAX);
            let max: u128 = self.max_amount.try_into().unwrap_or(u128::MAX);
            U256::from(rng.gen_range(min..=max))
        };

        let call = IUniswapV2Router::swapExactETHForTokensCall {
            amountOutMin: U256::ZERO,
            path: vec![self.weth, self.token],
            to,
            deadline: U256::from(u64::MAX),
        };

        TransactionRequest::default()
            .with_to(self.router)
            .with_input(Bytes::from(call.abi_encode()))
            .with_value(amount)
            .with_gas_limit(200_000)
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

    fn generate(&self, rng: &mut SeededRng, _from: Address, to: Address) -> TransactionRequest {
        let amount = if self.min_amount == self.max_amount {
            self.min_amount
        } else {
            let min: u128 = self.min_amount.try_into().unwrap_or(u128::MAX);
            let max: u128 = self.max_amount.try_into().unwrap_or(u128::MAX);
            U256::from(rng.gen_range(min..=max))
        };

        let call = IUniswapV3Router::exactInputSingleCall {
            params: IUniswapV3Router::ExactInputSingleParams {
                tokenIn: self.token_in,
                tokenOut: self.token_out,
                fee: U24::from(self.fee),
                recipient: to,
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
