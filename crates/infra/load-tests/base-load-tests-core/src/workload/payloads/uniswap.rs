use alloy_primitives::{Address, Bytes, U256};

use super::Payload;
use crate::{rpc::TransactionRequest, workload::SeededRng};

/// Generates Uniswap V2 style swap transactions.
#[derive(Debug, Clone)]
pub struct UniswapV2Payload {
    /// Router contract address.
    pub router: Address,
    /// WETH address.
    pub weth: Address,
    /// Token to swap.
    pub token: Address,
    /// Minimum swap amount.
    pub min_amount: U256,
    /// Maximum swap amount.
    pub max_amount: U256,
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

    fn encode_swap_exact_eth_for_tokens(
        amount_out_min: U256,
        path: &[Address],
        to: Address,
        deadline: U256,
    ) -> Bytes {
        let mut data = Vec::with_capacity(4 + 32 * 4 + 32 * path.len());
        data.extend_from_slice(&[0x7f, 0xf3, 0x6a, 0xb5]);
        data.extend_from_slice(&amount_out_min.to_be_bytes::<32>());
        data.extend_from_slice(&U256::from(128).to_be_bytes::<32>());
        data.extend_from_slice(&[0u8; 12]);
        data.extend_from_slice(to.as_slice());
        data.extend_from_slice(&deadline.to_be_bytes::<32>());
        data.extend_from_slice(&U256::from(path.len()).to_be_bytes::<32>());
        for addr in path {
            data.extend_from_slice(&[0u8; 12]);
            data.extend_from_slice(addr.as_slice());
        }
        Bytes::from(data)
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

        let path = [self.weth, self.token];
        let deadline = U256::from(u64::MAX);
        let data = Self::encode_swap_exact_eth_for_tokens(U256::ZERO, &path, to, deadline);

        TransactionRequest::contract_call(self.router, data)
            .with_value(amount)
            .with_gas_limit(200_000)
    }
}

/// Generates Uniswap V3 style swap transactions.
#[derive(Debug, Clone)]
pub struct UniswapV3Payload {
    /// Router contract address.
    pub router: Address,
    /// Token in.
    pub token_in: Address,
    /// Token out.
    pub token_out: Address,
    /// Pool fee tier (500, 3000, or 10000).
    pub fee: u32,
    /// Minimum swap amount.
    pub min_amount: U256,
    /// Maximum swap amount.
    pub max_amount: U256,
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

    fn encode_exact_input_single(
        token_in: Address,
        token_out: Address,
        fee: u32,
        recipient: Address,
        amount_in: U256,
    ) -> Bytes {
        let mut data = Vec::with_capacity(4 + 32 * 6);
        data.extend_from_slice(&[0x41, 0x4b, 0xf3, 0x89]);
        data.extend_from_slice(&[0u8; 12]);
        data.extend_from_slice(token_in.as_slice());
        data.extend_from_slice(&[0u8; 12]);
        data.extend_from_slice(token_out.as_slice());
        data.extend_from_slice(&U256::from(fee).to_be_bytes::<32>());
        data.extend_from_slice(&[0u8; 12]);
        data.extend_from_slice(recipient.as_slice());
        data.extend_from_slice(&amount_in.to_be_bytes::<32>());
        data.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
        Bytes::from(data)
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

        let data =
            Self::encode_exact_input_single(self.token_in, self.token_out, self.fee, to, amount);

        TransactionRequest::contract_call(self.router, data).with_gas_limit(250_000)
    }
}
