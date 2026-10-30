//! Uniswap V3 `NonfungiblePositionManager` mint, increase, decrease and collect transactions.

use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
};

use alloy_network::{EthereumWallet, TransactionBuilder};
use alloy_primitives::{Address, Bytes, Signed, U256, Uint};
use alloy_provider::Provider;
use alloy_rpc_types::{Log, TransactionRequest};
use alloy_sol_types::{SolCall, SolEvent, sol};
use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt, stream};
use serde::{Deserialize, Serialize};

use super::Payload;
use crate::{
    BaselineError, Result,
    rpc::{BaseFeeExt, RpcResultExt, create_wallet_provider},
    workload::{
        SeededRng,
        chain_prep::{ChainPrepContext, prep_submission_max_fee},
    },
};

type U24 = Uint<24, 1>;
type I24 = Signed<24, 1>;

/// Tick spacings on each side of the setup tick, so swaps rarely move the price out of range.
const RANGE_SPACINGS: i32 = 100;
/// The setup position holds this many increases' worth of liquidity at the setup price.
const SETUP_MULTIPLIER: u64 = 64;
/// A decrease removes a quarter of an increase's worth, so with `decrease <= increase` the position
/// still grows on average when the price drifts to a range edge. There an increase adds 0.49x
/// (fee 500), 0.43x (fee 3000) or 0.27x (fee 10000) of its setup-price liquidity.
const DECREASES_PER_INCREASE: u64 = 4;
const MINT_GAS_LIMIT: u64 = 600_000;
const POSITION_GAS_LIMIT: u64 = 300_000;
const MULTICALL_GAS_LIMIT: u64 = 1_200_000;

sol! {
    interface INonfungiblePositionManager {
        struct MintParams {
            address token0;
            address token1;
            uint24 fee;
            int24 tickLower;
            int24 tickUpper;
            uint256 amount0Desired;
            uint256 amount1Desired;
            uint256 amount0Min;
            uint256 amount1Min;
            address recipient;
            uint256 deadline;
        }
        struct IncreaseLiquidityParams {
            uint256 tokenId;
            uint256 amount0Desired;
            uint256 amount1Desired;
            uint256 amount0Min;
            uint256 amount1Min;
            uint256 deadline;
        }
        struct DecreaseLiquidityParams {
            uint256 tokenId;
            uint128 liquidity;
            uint256 amount0Min;
            uint256 amount1Min;
            uint256 deadline;
        }
        struct CollectParams {
            uint256 tokenId;
            address recipient;
            uint128 amount0Max;
            uint128 amount1Max;
        }
        event IncreaseLiquidity(uint256 indexed tokenId, uint128 liquidity, uint256 amount0, uint256 amount1);
        function mint(MintParams calldata params) external payable;
        function increaseLiquidity(IncreaseLiquidityParams calldata params) external payable;
        function decreaseLiquidity(DecreaseLiquidityParams calldata params) external payable;
        function collect(CollectParams calldata params) external payable;
        function multicall(bytes[] calldata data) external payable;
        function factory() external view returns (address);
    }

    interface IUniswapV3LiquidityPool {
        function slot0() external view returns (uint160, int24 tick, uint16, uint16, uint16, uint8, bool);
        function tickSpacing() external view returns (int24);
    }

    function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address);
}

/// Relative weights of the position operations; omitted weights are zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LiquidityOps {
    /// Mints a new position.
    pub mint: u32,
    /// Adds `amount0`/`amount1` to the sender's setup position.
    pub increase: u32,
    /// Removes a quarter of an increase's worth of liquidity from the sender's setup position.
    pub decrease: u32,
    /// Collects owed tokens from the sender's setup position.
    pub collect: u32,
    /// Mints, increases, decreases and collects in one `multicall` transaction.
    pub multicall: u32,
}

impl LiquidityOps {
    /// Equal weights of the four single-operation transactions, used when `ops` is omitted.
    pub const fn singles() -> Self {
        Self { mint: 1, increase: 1, decrease: 1, collect: 1, multicall: 0 }
    }

    /// Weights in op-index order: mint, increase, decrease, collect, multicall.
    pub const fn weights(&self) -> [u32; 5] {
        [self.mint, self.increase, self.decrease, self.collect, self.multicall]
    }
}

/// A sender's setup position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderPosition {
    /// Position NFT ID.
    pub token_id: U256,
    /// Liquidity removed per decrease.
    pub decrease_liquidity: u128,
}

/// Tick range and per-sender positions minted during setup.
#[derive(Debug, Clone)]
pub struct UniswapPositions {
    /// Lower tick of every minted position.
    pub tick_lower: I24,
    /// Upper tick of every minted position.
    pub tick_upper: I24,
    /// Setup position of each sender.
    pub positions: HashMap<Address, SenderPosition>,
}

/// Generates Uniswap V3 position-manager transactions on a real pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UniswapV3LiquidityPayload {
    /// `NonfungiblePositionManager` address.
    pub position_manager: Address,
    /// Lower-address pool token.
    pub token0: Address,
    /// Higher-address pool token.
    pub token1: Address,
    /// Pool fee tier.
    pub fee: u32,
    /// Maximum `token0` raw units added per mint or increase.
    pub amount0: U256,
    /// Maximum `token1` raw units added per mint or increase.
    pub amount1: U256,
    /// Operation weights.
    #[serde(default = "LiquidityOps::singles")]
    pub ops: LiquidityOps,
    /// Setup state shared by calibration and measured generators.
    #[serde(skip)]
    pub positions: Arc<OnceLock<UniswapPositions>>,
}

impl UniswapV3LiquidityPayload {
    /// Rejects configurations that would revert or drain the setup position.
    pub fn validate(&self) -> Result<()> {
        let error =
            |reason: &str| Err(BaselineError::Config(format!("uniswap_v3_liquidity {reason}")));
        let ops = self.ops;
        if self.token0 >= self.token1 {
            return error("token0 must sort below token1");
        }
        if self.fee >= 1 << 24 {
            return error("fee exceeds u24");
        }
        if self.amount0.is_zero() || self.amount1.is_zero() {
            return error("amount0 and amount1 must be > 0");
        }
        if ops.decrease > ops.increase {
            return error("decrease weight must not exceed increase weight");
        }
        if ops.weights().iter().try_fold(0u32, |sum, w| sum.checked_add(*w)).is_none_or(|t| t == 0)
        {
            return error("ops weights must sum to 1..=u32::MAX");
        }
        Ok(())
    }

    /// Reads the position minted by `position_manager` from a mint's receipt logs.
    pub fn minted_position(position_manager: Address, logs: &[Log]) -> Result<SenderPosition> {
        let event = logs
            .iter()
            .filter(|log| log.address() == position_manager)
            .find_map(|log| {
                INonfungiblePositionManager::IncreaseLiquidity::decode_log_data(log.data()).ok()
            })
            .ok_or_else(|| {
                BaselineError::Transaction("mint emitted no IncreaseLiquidity".into())
            })?;
        Ok(SenderPosition {
            token_id: event.tokenId,
            decrease_liquidity: (event.liquidity
                / u128::from(SETUP_MULTIPLIER * DECREASES_PER_INCREASE))
            .max(1),
        })
    }

    fn mint(&self, state: &UniswapPositions, recipient: Address, scale: u64) -> Vec<u8> {
        let params = INonfungiblePositionManager::MintParams {
            token0: self.token0,
            token1: self.token1,
            fee: U24::from(self.fee),
            tickLower: state.tick_lower,
            tickUpper: state.tick_upper,
            amount0Desired: self.amount0.saturating_mul(U256::from(scale)),
            amount1Desired: self.amount1.saturating_mul(U256::from(scale)),
            amount0Min: U256::ZERO,
            amount1Min: U256::ZERO,
            recipient,
            deadline: U256::MAX,
        };
        INonfungiblePositionManager::mintCall { params }.abi_encode()
    }

    fn increase(&self, position: SenderPosition) -> Vec<u8> {
        let params = INonfungiblePositionManager::IncreaseLiquidityParams {
            tokenId: position.token_id,
            amount0Desired: self.amount0,
            amount1Desired: self.amount1,
            amount0Min: U256::ZERO,
            amount1Min: U256::ZERO,
            deadline: U256::MAX,
        };
        INonfungiblePositionManager::increaseLiquidityCall { params }.abi_encode()
    }

    fn decrease(position: SenderPosition) -> Vec<u8> {
        let params = INonfungiblePositionManager::DecreaseLiquidityParams {
            tokenId: position.token_id,
            liquidity: position.decrease_liquidity,
            amount0Min: U256::ZERO,
            amount1Min: U256::ZERO,
            deadline: U256::MAX,
        };
        INonfungiblePositionManager::decreaseLiquidityCall { params }.abi_encode()
    }

    fn collect(position: SenderPosition, recipient: Address) -> Vec<u8> {
        let params = INonfungiblePositionManager::CollectParams {
            tokenId: position.token_id,
            recipient,
            amount0Max: u128::MAX,
            amount1Max: u128::MAX,
        };
        INonfungiblePositionManager::collectCall { params }.abi_encode()
    }

    /// Builds the op at `op`'s index in [`LiquidityOps::weights`] for `from`'s setup position.
    fn op_request(&self, op: usize, from: Address) -> TransactionRequest {
        let state = self.positions.get().expect("uniswap_v3_liquidity setup must run first");
        let position = state.positions[&from];
        match op {
            0 => self.request(self.mint(state, from, 1), MINT_GAS_LIMIT),
            1 => self.request(self.increase(position), POSITION_GAS_LIMIT),
            2 => self.request(Self::decrease(position), POSITION_GAS_LIMIT),
            3 => self.request(Self::collect(position, from), POSITION_GAS_LIMIT),
            _ => {
                let data = [
                    self.mint(state, from, 1),
                    self.increase(position),
                    Self::decrease(position),
                    Self::collect(position, from),
                ]
                .map(Bytes::from)
                .to_vec();
                let call = INonfungiblePositionManager::multicallCall { data };
                self.request(call.abi_encode(), MULTICALL_GAS_LIMIT)
            }
        }
    }

    fn request(&self, input: Vec<u8>, gas_limit: u64) -> TransactionRequest {
        TransactionRequest::default()
            .with_to(self.position_manager)
            .with_input(Bytes::from(input))
            .with_gas_limit(gas_limit)
    }

    async fn call<C: SolCall>(
        ctx: &ChainPrepContext<'_>,
        to: Address,
        call: C,
    ) -> Result<C::Return> {
        let data = ctx
            .client
            .call(TransactionRequest::default().with_to(to).with_input(call.abi_encode()).into())
            .await
            .rpc("eth_call uniswap_v3_liquidity setup")?;
        C::abi_decode_returns(&data)
            .map_err(|error| BaselineError::Config(format!("uniswap_v3_liquidity: {error}")))
    }
}

#[async_trait]
impl Payload for UniswapV3LiquidityPayload {
    fn name(&self) -> &'static str {
        "uniswap_v3_liquidity"
    }

    fn uses_runner_recipient(&self) -> bool {
        false
    }

    async fn prepare(&self, ctx: &mut ChainPrepContext<'_>) -> Result<()> {
        let setup = ctx.real_token_setup.ok_or_else(|| {
            BaselineError::Config("uniswap_v3_liquidity requires real_token_setup".into())
        })?;
        super::real_token_lifecycle::prepare(ctx, setup).await?;
        if self.positions.get().is_some() {
            return Ok(());
        }
        let manager = self.position_manager;
        let factory = Self::call(ctx, manager, INonfungiblePositionManager::factoryCall {}).await?;
        let get_pool =
            getPoolCall { tokenA: self.token0, tokenB: self.token1, fee: U24::from(self.fee) };
        let pool = Self::call(ctx, factory, get_pool).await?;
        if pool == Address::ZERO {
            return Err(BaselineError::Config("uniswap_v3_liquidity pool does not exist".into()));
        }
        let tick = Self::call(ctx, pool, IUniswapV3LiquidityPool::slot0Call {}).await?.tick;
        let spacing =
            Self::call(ctx, pool, IUniswapV3LiquidityPool::tickSpacingCall {}).await?.as_i32();
        let center = tick.as_i32().div_euclid(spacing) * spacing;
        let mut state = UniswapPositions {
            tick_lower: I24::try_from(center - RANGE_SPACINGS * spacing).expect("valid tick"),
            tick_upper: I24::try_from(center + RANGE_SPACINGS * spacing).expect("valid tick"),
            positions: HashMap::new(),
        };

        let base_fee = ctx.client.get_base_fee().await?;
        let priority_fee = (base_fee / 10).max(1);
        let max_fee = prep_submission_max_fee(base_fee, priority_fee, ctx.max_gas_price);
        let accounts: Vec<_> =
            ctx.accounts.accounts().iter().map(|a| (a.address, a.signer.clone())).collect();
        let mints = accounts.into_iter().map(|(sender, signer)| {
            let provider = create_wallet_provider(
                ctx.primary_submission_rpc.clone(),
                EthereumWallet::from(signer),
            );
            let tx = self
                .request(self.mint(&state, sender, SETUP_MULTIPLIER), MINT_GAS_LIMIT)
                .with_chain_id(ctx.chain_id)
                .with_max_fee_per_gas(max_fee)
                .with_max_priority_fee_per_gas(priority_fee);
            async move {
                let nonce = provider
                    .get_transaction_count(sender)
                    .pending()
                    .await
                    .rpc("get pending transaction count")?;
                let receipt = provider
                    .send_transaction(tx.with_nonce(nonce))
                    .await
                    .rpc("send setup mint")?
                    .get_receipt()
                    .await
                    .rpc("confirm setup mint")?;
                if !receipt.status() {
                    return Err(BaselineError::Transaction(format!(
                        "setup mint reverted for sender {sender}"
                    )));
                }
                Ok((sender, Self::minted_position(manager, receipt.logs())?))
            }
        });
        state.positions =
            stream::iter(mints).buffer_unordered(ctx.concurrency).try_collect().await?;
        self.positions
            .set(state)
            .map_err(|_| BaselineError::Config("uniswap_v3_liquidity prepared twice".into()))?;
        ctx.outputs.needs_sender_refresh = true;
        Ok(())
    }

    fn generate(&self, rng: &mut SeededRng, from: Address, _to: Address) -> TransactionRequest {
        let weights = self.ops.weights();
        let pick = rng.gen_range(0..weights.iter().sum::<u32>());
        let mut threshold = 0;
        let op = weights
            .iter()
            .position(|weight| {
                threshold += weight;
                pick < threshold
            })
            .expect("pick is below the weight total");
        self.op_request(op, from)
    }

    /// Every op runs at a different gas cost, so calibration measures each enabled op.
    fn calibration_requests(
        &self,
        _rng: &mut SeededRng,
        from: Address,
        _to: Address,
    ) -> Vec<(TransactionRequest, u32)> {
        let weights = self.ops.weights().into_iter().enumerate();
        weights
            .filter(|(_, weight)| *weight > 0)
            .map(|(op, w)| (self.op_request(op, from), w))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::LogData;

    use super::*;
    use crate::{
        config::{TestConfig, TxTypeConfig},
        runner::TxType,
    };

    const MANAGER: Address = Address::repeat_byte(0x10);
    const SENDER: Address = Address::repeat_byte(0x33);
    const POSITION: SenderPosition =
        SenderPosition { token_id: U256::from_limbs([7, 0, 0, 0]), decrease_liquidity: 99 };

    fn sample(ops: LiquidityOps) -> UniswapV3LiquidityPayload {
        let state = UniswapPositions {
            tick_lower: I24::try_from(-1000).unwrap(),
            tick_upper: I24::try_from(1000).unwrap(),
            positions: HashMap::from([(SENDER, POSITION)]),
        };
        UniswapV3LiquidityPayload {
            position_manager: MANAGER,
            token0: Address::repeat_byte(0x11),
            token1: Address::repeat_byte(0x22),
            fee: 500,
            amount0: U256::from(17),
            amount1: U256::from(29),
            ops,
            positions: Arc::new(OnceLock::from(state)),
        }
    }

    fn mint_log(address: Address, token_id: u64, liquidity: u128) -> Log {
        let event = INonfungiblePositionManager::IncreaseLiquidity {
            tokenId: U256::from(token_id),
            liquidity,
            amount0: U256::from(1),
            amount1: U256::from(1),
        };
        let data: LogData = event.encode_log_data();
        Log { inner: alloy_primitives::Log { address, data }, ..Default::default() }
    }

    /// Checks one decoded call against the sender's setup position and configured amounts.
    fn assert_call(payload: &UniswapV3LiquidityPayload, input: &[u8]) -> &'static str {
        if let Ok(call) = INonfungiblePositionManager::mintCall::abi_decode(input) {
            let p = call.params;
            assert_eq!(
                (p.token0, p.token1, p.fee),
                (payload.token0, payload.token1, U24::from(500))
            );
            assert_eq!((p.tickLower.as_i32(), p.tickUpper.as_i32()), (-1000, 1000));
            assert_eq!((p.amount0Desired, p.amount1Desired), (payload.amount0, payload.amount1));
            assert_eq!(p.recipient, SENDER, "sender owns the new position");
            "mint"
        } else if let Ok(call) =
            INonfungiblePositionManager::increaseLiquidityCall::abi_decode(input)
        {
            let p = call.params;
            assert_eq!(p.tokenId, POSITION.token_id);
            assert_eq!((p.amount0Desired, p.amount1Desired), (payload.amount0, payload.amount1));
            "increase"
        } else if let Ok(call) =
            INonfungiblePositionManager::decreaseLiquidityCall::abi_decode(input)
        {
            assert_eq!(call.params.tokenId, POSITION.token_id);
            assert_eq!(call.params.liquidity, POSITION.decrease_liquidity);
            "decrease"
        } else {
            let call =
                INonfungiblePositionManager::collectCall::abi_decode(input).expect("known op");
            assert_eq!(call.params.tokenId, POSITION.token_id);
            assert_eq!(call.params.recipient, SENDER, "collected tokens return to the sender");
            "collect"
        }
    }

    #[test]
    fn generates_each_weighted_op_for_the_senders_position() {
        let ops = LiquidityOps { mint: 1, increase: 1, decrease: 1, collect: 1, multicall: 1 };
        let payload = sample(ops);
        let mut rng = SeededRng::new(42);
        let mut seen = HashMap::new();
        for _ in 0..200 {
            let tx = payload.generate(&mut rng, SENDER, Address::ZERO);
            assert_eq!(tx.to, Some(MANAGER.into()));
            let input = tx.input.input().expect("calldata");
            let op = match INonfungiblePositionManager::multicallCall::abi_decode(input) {
                Ok(call) => {
                    let ops: Vec<_> = call.data.iter().map(|d| assert_call(&payload, d)).collect();
                    assert_eq!(ops, ["mint", "increase", "decrease", "collect"]);
                    assert_eq!(tx.gas, Some(MULTICALL_GAS_LIMIT));
                    "multicall"
                }
                Err(_) => assert_call(&payload, input),
            };
            *seen.entry(op).or_insert(0) += 1;
        }
        assert_eq!(seen.len(), 5, "every op is generated: {seen:?}");

        let payload = sample(LiquidityOps { collect: 1, ..Default::default() });
        for _ in 0..20 {
            let tx = payload.generate(&mut rng, SENDER, Address::ZERO);
            assert_eq!(assert_call(&payload, tx.input.input().unwrap()), "collect");
        }
    }

    #[test]
    fn calibrates_every_enabled_op_by_weight() {
        let payload =
            sample(LiquidityOps { mint: 3, decrease: 1, increase: 2, multicall: 4, collect: 0 });
        let mut rng = SeededRng::new(42);
        let variants = payload.calibration_requests(&mut rng, SENDER, Address::ZERO);
        let ops: Vec<_> = variants
            .iter()
            .map(|(tx, weight)| {
                let input = tx.input.input().unwrap();
                let op = INonfungiblePositionManager::multicallCall::abi_decode(input)
                    .map_or_else(|_| assert_call(&payload, input), |_| "multicall");
                (op, *weight)
            })
            .collect();
        assert_eq!(
            ops,
            [("mint", 3), ("increase", 2), ("decrease", 1), ("multicall", 4)],
            "one variant per enabled op, in an order where every op is valid"
        );
        let transfer = super::super::TransferPayload::default();
        assert_eq!(
            transfer.calibration_requests(&mut rng, SENDER, Address::ZERO).len(),
            1,
            "other payloads keep a single calibration request"
        );
    }

    #[test]
    fn tracks_the_position_minted_by_the_manager() {
        let logs = [mint_log(Address::repeat_byte(0x99), 1, 6_400), mint_log(MANAGER, 42, 6_400)];
        let position = UniswapV3LiquidityPayload::minted_position(MANAGER, &logs).unwrap();
        assert_eq!(position.token_id, U256::from(42), "other contracts' events are ignored");
        assert_eq!(position.decrease_liquidity, 25, "one decrease removes 1/256 of setup");
        let tiny = UniswapV3LiquidityPayload::minted_position(MANAGER, &[mint_log(MANAGER, 1, 1)]);
        assert_eq!(tiny.unwrap().decrease_liquidity, 1, "decreases never burn zero liquidity");
        assert!(UniswapV3LiquidityPayload::minted_position(MANAGER, &logs[..1]).is_err());
    }

    #[test]
    fn config_parses_and_rejects_invalid_positions() {
        let yaml = include_str!("../../../examples/mainnet-mix-mainnet-snapshot.yaml").to_owned()
            + "  - {weight: 1, type: uniswap_v3_liquidity, position_manager: \"0x03a520b32C04BF3bEEf7BEb72E919cf822Ed34f1\",\n      token0: \"0x4200000000000000000000000000000000000006\", token1: \"0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913\", # gitleaks:allow\n      fee: 500, amount0: \"10000000000000\", amount1: \"23000\", ops: {multicall: 2}}\n";
        let config: TestConfig = serde_yaml::from_str(&yaml).expect("liquidity config parses");
        let load = config.to_load_config(None).expect("liquidity config converts");
        let TxType::UniswapV3Liquidity(parsed) = &load.transactions.last().unwrap().tx_type else {
            panic!("expected uniswap_v3_liquidity");
        };
        assert_eq!(
            parsed.ops,
            LiquidityOps { multicall: 2, ..Default::default() },
            "omitted op weights are zero"
        );
        let omitted: UniswapV3LiquidityPayload = serde_yaml::from_str(
            "{position_manager: \"0x03a520b32C04BF3bEEf7BEb72E919cf822Ed34f1\", token0: \"0x4200000000000000000000000000000000000006\", token1: \"0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913\", fee: 500, amount0: \"1\", amount1: \"1\"}", // gitleaks:allow
        )
        .unwrap();
        assert_eq!(omitted.ops, LiquidityOps::singles(), "omitted ops run the four single ops");
        assert!(
            config.parse_real_token_setup(8453).unwrap().is_some(),
            "pool matches setup tokens"
        );

        let mut no_setup = config.clone();
        no_setup.real_token_setup = None;
        no_setup
            .transactions
            .retain(|tx| matches!(tx.tx_type, TxTypeConfig::UniswapV3Liquidity(_)));
        assert_eq!(
            no_setup.to_load_config(None).unwrap_err().to_string(),
            "configuration error: uniswap_v3_liquidity requires enabled real_token_setup",
            "setup mints need real_token_setup"
        );

        let mut wrong_pool = config;
        let Some(TxTypeConfig::UniswapV3Liquidity(p)) =
            wrong_pool.transactions.last_mut().map(|tx| &mut tx.tx_type)
        else {
            unreachable!()
        };
        p.token1 = Address::repeat_byte(0xff);
        assert!(wrong_pool.parse_real_token_setup(8453).is_err(), "setup cannot fund other pools");

        let valid = sample(LiquidityOps::singles());
        assert!(valid.validate().is_ok());
        let invalid: [fn(&mut UniswapV3LiquidityPayload); 6] = [
            |p| std::mem::swap(&mut p.token0, &mut p.token1),
            |p| p.fee = 1 << 24,
            |p| p.amount1 = U256::ZERO,
            |p| p.ops.decrease = 2,
            |p| p.ops = LiquidityOps::default(),
            |p| p.ops.mint = u32::MAX,
        ];
        for (index, mutate) in invalid.iter().enumerate() {
            let mut payload = valid.clone();
            mutate(&mut payload);
            assert!(payload.validate().is_err(), "case {index} must be rejected");
        }
    }
}
