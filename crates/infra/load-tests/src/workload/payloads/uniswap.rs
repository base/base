use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes, U160, U256, U512, Uint};
use alloy_provider::Provider;
use alloy_rpc_types::TransactionRequest;
use alloy_sol_types::{SolCall, sol};
use async_trait::async_trait;
use base_common_consensus::Call;
use serde::{Deserialize, Serialize};

type U24 = Uint<24, 1>;

use super::Payload;
use crate::{
    BaselineError, Result,
    rpc::{QueryProvider, RpcResultExt},
    workload::{SeededRng, chain_prep::ChainPrepContext},
};

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

        struct ExactInputParams {
            bytes path;
            address recipient;
            uint256 amountIn;
            uint256 amountOutMinimum;
        }

        function exactInput(
            ExactInputParams calldata params
        ) external payable returns (uint256 amountOut);

        function factory() external view returns (address);
    }

    interface IUniswapV3Factory {
        function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address);
    }

    interface IUniswapV3Pool {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint8 feeProtocol,
            bool unlocked
        );
    }
}

/// An intermediate token on a multi-hop `uniswap_v3` route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UniswapV3Hop {
    /// Intermediate token.
    pub token: Address,
    /// Fee tier of the pool that swaps into `token`.
    pub fee: u32,
}

/// Generates Uniswap V3 style swap transactions.
#[derive(Debug, Clone)]
pub struct UniswapV3Payload {
    native: bool,
    router: Address,
    token_in: Address,
    token_out: Address,
    fee: u32,
    min_amount: U256,
    max_amount: U256,
    reverse_min_amount: U256,
    reverse_max_amount: U256,
    /// Forward and reverse `exactInput` paths and their pool count, for multi-hop routes.
    paths: Option<(Bytes, Bytes, u64)>,
}

impl UniswapV3Payload {
    /// Returns the pools, as `(input, output, fee)`, from `token_in` through each `via` token to
    /// `token_out`; the last pool uses `fee`.
    pub fn pools(
        token_in: Address,
        via: &[UniswapV3Hop],
        fee: u32,
        token_out: Address,
    ) -> Vec<(Address, Address, u32)> {
        let mut input = token_in;
        let mut pools = Vec::with_capacity(via.len() + 1);
        for hop in via {
            pools.push((input, hop.token, hop.fee));
            input = hop.token;
        }
        pools.push((input, token_out, fee));
        pools
    }

    /// Encodes pools as an `exactInput` path: the first input token, then a 3-byte fee and the
    /// output token per pool.
    pub fn encode_path(pools: &[(Address, Address, u32)]) -> Bytes {
        let mut path = pools[0].0.to_vec();
        for (_, output, fee) in pools {
            path.extend_from_slice(&fee.to_be_bytes()[1..]);
            path.extend_from_slice(output.as_slice());
        }
        path.into()
    }

    /// Routes swaps through `via` with `exactInput`; an empty `via` keeps `exactInputSingle`.
    pub fn with_via(mut self, via: &[UniswapV3Hop]) -> Self {
        if via.is_empty() {
            return self;
        }
        let pools = Self::pools(self.token_in, via, self.fee, self.token_out);
        let reverse: Vec<_> =
            pools.iter().rev().map(|&(input, output, fee)| (output, input, fee)).collect();
        self.paths =
            Some((Self::encode_path(&pools), Self::encode_path(&reverse), pools.len() as u64));
        self
    }

    /// Expands a native swap request into an exact approval followed by the swap.
    pub fn native_calls(router: Address, data: &Bytes) -> Result<Vec<Call>> {
        let swap = IUniswapV3Router::exactInputSingleCall::abi_decode(data).map_err(|error| {
            crate::BaselineError::Transaction(format!("invalid native swap: {error}"))
        })?;
        Ok(vec![
            Call {
                to: swap.params.tokenIn,
                value: U256::ZERO,
                data: super::real_token_lifecycle::encode_erc20_approve(
                    router,
                    swap.params.amountIn,
                ),
            },
            Call { to: router, value: U256::ZERO, data: data.clone() },
        ])
    }

    /// Reads `sqrtPriceX96` of the pool the router uses for `token_in`/`token_out` at `fee`.
    pub async fn read_sqrt_price_x96(
        client: &QueryProvider,
        router: Address,
        token_in: Address,
        token_out: Address,
        fee: u32,
    ) -> Result<U256> {
        let call = |to: Address, input: Vec<u8>| {
            client.call(TransactionRequest::default().with_to(to).with_input(input).into())
        };
        let decode_error = |error: alloy_sol_types::Error| {
            BaselineError::Config(format!("uniswap_v3 reverse_amounts: auto: {error}"))
        };
        let data = call(router, IUniswapV3Router::factoryCall {}.abi_encode())
            .await
            .rpc("eth_call factory")?;
        let factory =
            IUniswapV3Router::factoryCall::abi_decode_returns(&data).map_err(decode_error)?;
        let get_pool = IUniswapV3Factory::getPoolCall {
            tokenA: token_in,
            tokenB: token_out,
            fee: U24::from(fee),
        };
        let data = call(factory, get_pool.abi_encode()).await.rpc("eth_call getPool")?;
        let pool =
            IUniswapV3Factory::getPoolCall::abi_decode_returns(&data).map_err(decode_error)?;
        if pool == Address::ZERO {
            return Err(BaselineError::Config(format!(
                "uniswap_v3 reverse_amounts: auto: no pool for {token_in}/{token_out} at fee {fee}"
            )));
        }
        let data =
            call(pool, IUniswapV3Pool::slot0Call {}.abi_encode()).await.rpc("eth_call slot0")?;
        let slot0 = IUniswapV3Pool::slot0Call::abi_decode_returns(&data).map_err(decode_error)?;
        Ok(U256::from(slot0.sqrtPriceX96))
    }

    /// Converts `amount` of `token_in` into `token_out` raw units at `sqrt_price_x96`, rounded to
    /// the nearest unit.
    ///
    /// Uniswap prices token1 in token0 raw units as `(sqrt_price_x96 / 2^96)^2`, and token0 is
    /// the lower address, so decimals need no separate handling.
    pub fn amount_at_price(
        amount: U256,
        sqrt_price_x96: U256,
        token_in: Address,
        token_out: Address,
    ) -> Result<U256> {
        if sqrt_price_x96.is_zero() {
            return Err(BaselineError::Config(
                "uniswap_v3 reverse_amounts: auto: pool is not initialized".into(),
            ));
        }
        let sqrt_price = U512::from(sqrt_price_x96);
        let price = sqrt_price * sqrt_price;
        let q192 = U512::from(1) << 192;
        let (numerator, denominator) = if token_in < token_out {
            (U512::from(amount) * price, q192)
        } else {
            (U512::from(amount) * q192, price)
        };
        let converted = (numerator + denominator / U512::from(2)) / denominator;
        if converted.is_zero() || converted > U512::from(u128::MAX) {
            return Err(BaselineError::Config(format!(
                "uniswap_v3 reverse_amounts: auto: amount {amount} converts to {converted} token_out units; must be between 1 and u128::MAX"
            )));
        }
        Ok(U256::from(converted))
    }

    /// Enables native approval-and-swap batching without changing ordinary swaps.
    pub const fn with_native(mut self, native: bool) -> Self {
        self.native = native;
        self
    }

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
            native: false,
            router,
            token_in,
            token_out,
            fee,
            min_amount,
            max_amount,
            reverse_min_amount,
            reverse_max_amount,
            paths: None,
        }
    }
}

#[async_trait]
impl Payload for UniswapV3Payload {
    fn name(&self) -> &'static str {
        "uniswap_v3"
    }

    fn uses_runner_recipient(&self) -> bool {
        false
    }

    async fn prepare(&self, ctx: &mut ChainPrepContext<'_>) -> Result<()> {
        if let Some(setup) = ctx.real_token_setup {
            super::real_token_lifecycle::prepare(ctx, setup).await?;
        }
        Ok(())
    }

    fn generate(&self, rng: &mut SeededRng, from: Address, _to: Address) -> TransactionRequest {
        // Randomly swap direction to exercise both sides of the pool.
        // V3 pools are keyed by (token0, token1, fee) with token0 < token1,
        // so the fee tier is direction-agnostic and this is safe.
        let forward = rng.random::<bool>();
        let (input, output, min_amount, max_amount) = if forward {
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

        let (data, gas_limit) = if let Some((forward_path, reverse_path, pools)) = &self.paths {
            let call = IUniswapV3Router::exactInputCall {
                params: IUniswapV3Router::ExactInputParams {
                    path: if forward { forward_path.clone() } else { reverse_path.clone() },
                    recipient: from,
                    amountIn: amount,
                    amountOutMinimum: U256::ZERO,
                },
            };
            (call.abi_encode(), 250_000 * pools)
        } else {
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
            (call.abi_encode(), if self.native { 500_000 } else { 250_000 })
        };

        let mut request = TransactionRequest::default()
            .with_to(self.router)
            .with_input(Bytes::from(data))
            .with_gas_limit(gas_limit);
        if self.native {
            request.transaction_type =
                Some(base_common_consensus::Eip8130Constants::EIP8130_TX_TYPE);
        }
        request
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::Decodable2718;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_consensus::BaseTxEnvelope;

    use super::*;
    use crate::runner::{Fees, PreparedTransaction, SubmissionPipeline, SubmitCohort};

    /// The swap profile supports one actor and shared-pool contention without sidecars.
    #[test]
    fn native_swap_profile_preserves_setup_and_actor_counts() {
        let yaml = include_str!("../../../examples/eip8430-swap-mainnet-snapshot.yaml");
        for actors in [1, 16] {
            let mut config: crate::config::TestConfig =
                serde_yaml::from_str(yaml).expect("swap profile parses");
            config.sender_count = actors;
            let load = config.to_load_config(None).expect("native swap config converts");
            assert_eq!(
                load.account_count, actors as usize,
                "configured actors share the same router"
            );
            assert_eq!(load.transactions.len(), 1, "profile selects only the swap workload");
            assert_eq!(load.transactions[0].weight, 100, "all traffic is native swaps");
            assert!(
                matches!(
                    load.transactions[0].tx_type,
                    crate::runner::TxType::UniswapV3 { native: true, .. }
                ),
                "native option survives config conversion"
            );
            assert_eq!(load.validity_ratio, 0.0, "native profile must not use validity sidecars");
            assert!(
                config.parse_real_token_setup(load.chain_id).expect("valid token setup").is_some(),
                "reuse WETH and pair-token funding"
            );
            let mut generator = crate::workload::WorkloadGenerator::from_tx_configs(
                crate::config::WorkloadConfig::default(),
                &load.transactions,
                None,
            )
            .expect("native workload builds");
            let request = generator
                .generate_payload(Address::repeat_byte(3), Address::ZERO)
                .expect("swap generates");
            assert_eq!(
                request.transaction_type,
                Some(0x79),
                "config must generate native requests"
            );
            let mut invalid = load.clone();
            invalid.validity_ratio = 1.0;
            assert!(invalid.validate().is_err(), "native swaps must reject validity-RPC routing");
        }
        let config: crate::config::TestConfig =
            serde_yaml::from_str(&yaml.replace("native: true", "native: false"))
                .expect("ordinary swap profile parses");
        let load = config.to_load_config(None).expect("ordinary swap converts");
        let mut generator = crate::workload::WorkloadGenerator::from_tx_configs(
            crate::config::WorkloadConfig::default(),
            &load.transactions,
            None,
        )
        .expect("ordinary workload builds");
        let request = generator
            .generate_payload(Address::repeat_byte(3), Address::ZERO)
            .expect("ordinary swap generates");
        assert_eq!(request.transaction_type, None, "ordinary swaps keep Ethereum signing");
        assert_eq!(request.gas, Some(250_000), "ordinary swap gas ceiling is unchanged");
    }

    /// Native exact approvals must not invalidate an ordinary swap's setup allowance.
    #[test]
    fn native_swaps_reject_overlapping_ordinary_allowances() {
        let config: crate::config::TestConfig = serde_yaml::from_str(include_str!(
            "../../../examples/eip8430-swap-mainnet-snapshot.yaml"
        ))
        .expect("swap profile parses");
        let load = config.to_load_config(None).expect("native swap converts");
        let mut mixed = load;
        let mut ordinary = mixed.transactions[0].clone();
        if let crate::runner::TxType::UniswapV3 { native, .. } = &mut ordinary.tx_type {
            *native = false;
        }
        mixed.transactions.push(ordinary);
        assert_eq!(mixed.transactions.len(), 2, "test mixes exactly two workload entries");
        assert_eq!(
            mixed.validate().expect_err("overlapping allowance must be rejected").to_string(),
            "configuration error: native and ordinary swaps cannot share a router/token allowance",
            "report the allowance conflict before setup"
        );
        mixed.transactions[1].weight = 0;
        assert!(mixed.validate().is_ok(), "zero-weight ordinary swaps cannot consume allowance");
        mixed.transactions[1].weight = 100;
        if let crate::runner::TxType::UniswapV3 { router, .. } = &mut mixed.transactions[1].tx_type
        {
            *router = Address::repeat_byte(0x99);
        }
        assert!(mixed.validate().is_ok(), "different routers have separate allowances");
        mixed.transactions[1] = mixed.transactions[0].clone();
        assert!(mixed.validate().is_ok(), "multiple native workloads each set their own approval");
    }

    /// Native swaps sign one phase containing an exact approval before the router call.
    #[test]
    fn native_swaps_batch_approval_then_swap() {
        let signer = PrivateKeySigner::random();
        let router = Address::repeat_byte(0x10);
        let input = Address::repeat_byte(0x11);
        let output = Address::repeat_byte(0x22);
        let payload = UniswapV3Payload::new(
            router,
            input,
            output,
            500,
            U256::from(17),
            U256::from(17),
            Some((U256::from(29), U256::from(29))),
        )
        .with_native(true);
        let mut rng = SeededRng::new(42);
        let mut directions = [0; 2];
        for nonce in 0..32 {
            let request = payload.generate(&mut rng, signer.address(), Address::ZERO);
            assert_eq!(
                request.transaction_type,
                Some(0x79),
                "swap must select the native envelope"
            );
            assert_eq!(request.gas, Some(500_000), "native swap includes approval overhead");
            let data = request.input.input().expect("swap calldata").clone();
            let swap =
                IUniswapV3Router::exactInputSingleCall::abi_decode(&data).expect("valid swap ABI");
            let forward = swap.params.tokenIn == input;
            directions[usize::from(!forward)] += 1;
            assert_eq!(
                swap.params.tokenIn,
                if forward { input } else { output },
                "input token follows direction"
            );
            assert_eq!(
                swap.params.tokenOut,
                if forward { output } else { input },
                "output token follows direction"
            );
            assert_eq!(
                swap.params.amountIn,
                U256::from(if forward { 17 } else { 29 }),
                "amount follows token units"
            );
            assert_eq!(swap.params.recipient, signer.address(), "sender receives swap output");
            assert_eq!(swap.params.fee, U24::from(500), "fee tier is preserved");
            assert_eq!(
                swap.params.amountOutMinimum,
                U256::ZERO,
                "load fixture has no slippage bound"
            );
            assert_eq!(swap.params.sqrtPriceLimitX96, U160::ZERO, "fixture uses no price limit");
            let prepared = PreparedTransaction {
                native: true,
                from: signer.address(),
                to: Some(router),
                value: U256::ZERO,
                data: data.clone(),
                gas_limit: 500_000,
                estimated_gas: 0,
                validity: vec![],
                cohort: SubmitCohort::Plain,
            };
            let signed = SubmissionPipeline::sign_at_nonce(
                &signer,
                &prepared,
                8453,
                nonce,
                Fees { max_fee: 100, priority_fee: 10 },
            )
            .expect("native swap signs");
            let BaseTxEnvelope::Eip8130(native) =
                BaseTxEnvelope::decode_2718(&mut signed.raw.as_ref())
                    .expect("native wire encoding")
            else {
                panic!("swap must be native")
            };
            let mut approval = vec![0x09, 0x5e, 0xa7, 0xb3];
            approval.extend_from_slice(&[0; 12]);
            approval.extend_from_slice(router.as_slice());
            approval.extend_from_slice(&swap.params.amountIn.to_be_bytes::<32>());
            assert_eq!(
                native.tx().calls,
                vec![vec![
                    Call { to: swap.params.tokenIn, value: U256::ZERO, data: approval.into() },
                    Call { to: router, value: U256::ZERO, data }
                ]],
                "exactly one phase must approve the exact amount before swapping"
            );
            assert_eq!(
                native.tx().nonce_sequence,
                nonce,
                "swap consumes exactly one protocol nonce"
            );
            assert_eq!(
                native.recover_eoa_sender().expect("EOA signature"),
                Some(signer.address()),
                "signature binds the batch to its sender"
            );
        }
        assert_eq!(directions.iter().sum::<i32>(), 32, "exactly 32 native batches generated");
        assert!(directions[0] > 0 && directions[1] > 0, "both swap directions must be exercised");
        assert!(
            UniswapV3Payload::native_calls(router, &Bytes::from_static(&[1, 2, 3])).is_err(),
            "malformed swaps must fail before signing"
        );
    }

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

    #[test]
    fn encode_path_packs_token_fee_token() {
        let (weth, cbbtc, usdc) =
            (Address::repeat_byte(0x42), Address::repeat_byte(0xcb), Address::repeat_byte(0x83));
        let pools =
            UniswapV3Payload::pools(weth, &[UniswapV3Hop { token: cbbtc, fee: 500 }], 3000, usdc);
        assert_eq!(pools, vec![(weth, cbbtc, 500), (cbbtc, usdc, 3000)]);
        let mut expected = weth.to_vec();
        expected.extend_from_slice(&[0x00, 0x01, 0xf4]);
        expected.extend_from_slice(cbbtc.as_slice());
        expected.extend_from_slice(&[0x00, 0x0b, 0xb8]);
        expected.extend_from_slice(usdc.as_slice());
        assert_eq!(UniswapV3Payload::encode_path(&pools), Bytes::from(expected));
    }

    #[test]
    fn via_generates_exact_input_along_the_route_in_both_directions() {
        let (router, token_in, token_out) =
            (Address::repeat_byte(0x10), Address::repeat_byte(0x11), Address::repeat_byte(0x22));
        let via = [
            UniswapV3Hop { token: Address::repeat_byte(0x33), fee: 500 },
            UniswapV3Hop { token: Address::repeat_byte(0x44), fee: 100 },
        ];
        let payload = UniswapV3Payload::new(
            router,
            token_in,
            token_out,
            3000,
            U256::from(17),
            U256::from(17),
            Some((U256::from(29), U256::from(29))),
        )
        .with_via(&via);
        let forward = UniswapV3Payload::encode_path(&UniswapV3Payload::pools(
            token_in, &via, 3000, token_out,
        ));
        let reverse = UniswapV3Payload::encode_path(&[
            (token_out, via[1].token, 3000),
            (via[1].token, via[0].token, 100),
            (via[0].token, token_in, 500),
        ]);
        let sender = Address::repeat_byte(0x55);
        let mut rng = SeededRng::new(42);
        let mut seen = [false; 2];
        for _ in 0..32 {
            let tx = payload.generate(&mut rng, sender, Address::ZERO);
            assert_eq!(tx.gas, Some(750_000), "gas ceiling scales with pool count");
            let swap = IUniswapV3Router::exactInputCall::abi_decode(tx.input.input().unwrap())
                .expect("multi-hop swaps use exactInput")
                .params;
            assert_eq!(swap.recipient, sender);
            assert_eq!(swap.amountOutMinimum, U256::ZERO);
            let is_forward = swap.path == forward;
            assert!(is_forward || swap.path == reverse, "unexpected path {}", swap.path);
            assert_eq!(swap.amountIn, U256::from(if is_forward { 17 } else { 29 }));
            seen[usize::from(is_forward)] = true;
        }
        assert_eq!(seen, [true, true], "both directions are exercised");
    }

    /// 18-decimal WETH priced at 2,320 6-decimal USDC, with WETH as either pool token.
    #[test]
    fn amount_at_price_converts_raw_units_for_both_token_orders() {
        let low = Address::repeat_byte(0x11);
        let high = Address::repeat_byte(0x22);
        // sqrt(2320e6 / 1e18) * 2^96: WETH is token0, USDC is token1.
        let weth_token0 = U256::from(3_816_133_648_877_761_641_136_391u128);
        // sqrt(1e18 / 2320e6) * 2^96: USDC is token0, WETH is token1.
        let weth_token1 = U256::from(1_644_885_193_481_793_810_834_651_330_288_053u128);
        for (weth_wei, usdc_units) in
            [(10_000_000_000_000u64, 23_200u64), (100_000_000_000_000, 232_000)]
        {
            for (sqrt_price, weth, usdc) in [(weth_token0, low, high), (weth_token1, high, low)] {
                assert_eq!(
                    UniswapV3Payload::amount_at_price(U256::from(weth_wei), sqrt_price, weth, usdc)
                        .unwrap(),
                    U256::from(usdc_units),
                    "WETH -> USDC"
                );
                assert_eq!(
                    UniswapV3Payload::amount_at_price(
                        U256::from(usdc_units),
                        sqrt_price,
                        usdc,
                        weth
                    )
                    .unwrap(),
                    U256::from(weth_wei),
                    "USDC -> WETH"
                );
            }
        }
    }

    #[test]
    fn amount_at_price_rejects_zero_results_and_uninitialized_pools() {
        let weth = Address::repeat_byte(0x11);
        let usdc = Address::repeat_byte(0x22);
        let sqrt_price = U256::from(3_816_133_648_877_761_641_136_391u128);
        assert!(
            UniswapV3Payload::amount_at_price(U256::from(100), sqrt_price, weth, usdc).is_err()
        );
        assert!(UniswapV3Payload::amount_at_price(U256::from(1), U256::ZERO, weth, usdc).is_err());
    }
}
