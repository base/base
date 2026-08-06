//! Real-token chain preparation for Uniswap/Aerodrome swap workloads.

use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

use alloy_network::{EthereumWallet, TransactionBuilder};
use alloy_primitives::{Address, Bytes, Signed, U160, U256, Uint, utils::format_ether};
use alloy_provider::Provider;
use alloy_rpc_types::{BlockNumberOrTag, TransactionRequest};
use alloy_sol_types::{SolCall, sol};
use futures::{StreamExt, stream};
use indicatif::{ProgressBar, ProgressStyle};
use tracing::{info, instrument, trace};
use url::Url;

use crate::{
    BaselineError, Result,
    rpc::{BaseFeeExt, QueryProvider, RpcResultExt, create_wallet_provider},
    workload::{
        AccountPool,
        chain_prep::{
            ChainPrepContext, RealTokenAcquisition, RealTokenRecoverySummary, RealTokenSetup,
            await_token_balances, encode_erc20_balance_of, prep_submission_max_fee,
        },
    },
};

fn progress_bar(hide_progress: bool, total: u64, prefix: &str) -> ProgressBar {
    if hide_progress {
        return ProgressBar::hidden();
    }
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template("{prefix} [{bar:40.cyan/blue}] {pos}/{len} ({eta})")
            .expect("valid template")
            .progress_chars("█▓░"),
    );
    pb.set_prefix(prefix.to_string());
    pb
}

const WETH_DEPOSIT_GAS_LIMIT: u64 = 100_000;
const WETH_WITHDRAW_GAS_LIMIT: u64 = 45_000;
const ERC20_APPROVE_GAS_LIMIT: u64 = 65_000;
const SETUP_SWAP_GAS_LIMIT: u64 = 250_000;

type U24 = Uint<24, 1>;
type I24 = Signed<24, 1>;

sol! {
    interface IRealTokenUniswapV3Router {
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

    interface IRealTokenAerodromeClRouter {
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

enum PairSwapDirection {
    AcquirePairToken,
    RecoverWeth,
}

impl PairSwapDirection {
    const fn tokens(&self, setup: &RealTokenSetup) -> (Address, Address) {
        match self {
            Self::AcquirePairToken => (setup.weth, setup.pair_token.token),
            Self::RecoverWeth => (setup.pair_token.token, setup.weth),
        }
    }
}

fn collect_real_token_setup_approvals(
    setup: &RealTokenSetup,
    swap_routers: &[Address],
) -> Vec<(Address, Address)> {
    let mut approvals = BTreeSet::new();
    for &router in swap_routers {
        approvals.insert((setup.weth, router));
        approvals.insert((setup.pair_token.token, router));
    }
    approvals.insert((setup.weth, setup.pair_token.acquisition.router()));
    approvals.into_iter().collect()
}

/// Sets up real-token balances and approvals for bidirectional swap workloads.
///
/// This preserves the existing random-direction swap payload semantics by
/// ensuring every sender has both WETH and the paired token before the
/// measured loop starts.
#[instrument(skip(ctx, setup), fields(accounts = ctx.accounts.len()))]
pub(super) async fn prepare(ctx: &mut ChainPrepContext<'_>, setup: &RealTokenSetup) -> Result<()> {
    if ctx.outputs.real_tokens_prepared {
        return Ok(());
    }
    let swap_routers = ctx.swap_routers.clone();
    if swap_routers.is_empty() {
        return Err(BaselineError::Config(
            "real-token setup requires at least one swap router".into(),
        ));
    }
    let approval_targets = collect_real_token_setup_approvals(setup, &swap_routers);

    let base_fee = ctx.client.get_base_fee().await?;
    let max_priority_fee = (base_fee / 10).max(1);
    let max_fee = prep_submission_max_fee(base_fee, max_priority_fee, ctx.max_gas_price);

    let account_data: Vec<_> =
        ctx.accounts.accounts().iter().map(|a| (a.address, a.signer.clone())).collect();
    let client = ctx.client.clone();
    let primary_submission_rpc = ctx.primary_submission_rpc.clone();
    let chain_id = ctx.chain_id;
    let setup = setup.clone();
    let total_accounts = account_data.len();

    info!(
        accounts = total_accounts,
        weth = %setup.weth,
        pair_token = %setup.pair_token.token,
        measured_routers = swap_routers.len(),
        approval_targets = approval_targets.len(),
        "setting up real-token swap balances"
    );

    let pb_setup = ctx.progress_bar(total_accounts as u64, "Setting up real tokens");
    let setup_futs = account_data.into_iter().map(|(sender, signer)| {
            let client = client.clone();
            let primary_submission_rpc = primary_submission_rpc.clone();
            let setup = setup.clone();
            let approval_targets = approval_targets.clone();
            async move {
                let weth_balance = read_erc20_balance(&client, setup.weth, sender).await?;
                let pair_balance =
                    read_erc20_balance(&client, setup.pair_token.token, sender).await?;
                let needs_pair_token = pair_balance < setup.pair_token.amount_per_sender;
                let acquisition_amount = if needs_pair_token {
                    setup.pair_token.acquisition.amount_in()
                } else {
                    U256::ZERO
                };
                let required_weth =
                    setup.weth_amount_per_sender.saturating_add(acquisition_amount);
                let deposit_deficit = required_weth.saturating_sub(weth_balance);

                let mut approvals = Vec::new();
                for (token, router) in &approval_targets {
                    let allowance =
                        read_erc20_allowance(&client, *token, sender, *router).await?;
                    if allowance < setup.approval_amount {
                        approvals.push((*token, *router));
                    }
                }

                let mut setup_gas_limit = approvals
                    .len()
                    .saturating_mul(ERC20_APPROVE_GAS_LIMIT as usize)
                    as u64;
                if deposit_deficit > U256::ZERO {
                    setup_gas_limit = setup_gas_limit.saturating_add(WETH_DEPOSIT_GAS_LIMIT);
                }
                if needs_pair_token {
                    setup_gas_limit = setup_gas_limit.saturating_add(SETUP_SWAP_GAS_LIMIT);
                }

                let native_balance = client
                    .get_balance(sender)
                    .block_id(BlockNumberOrTag::Pending.into())
                    .await
                    .rpc("get pending balance")?;
                let setup_gas_cost = U256::from(setup_gas_limit).saturating_mul(U256::from(max_fee));
                let total_setup_cost = deposit_deficit.saturating_add(setup_gas_cost);
                if native_balance < total_setup_cost {
                    return Err(BaselineError::Transaction(format!(
                        "sender {} has insufficient balance for real-token setup: has {} ETH, needs {} ETH (deposit {} ETH + setup gas {} ETH)",
                        sender,
                        format_ether(native_balance),
                        format_ether(total_setup_cost),
                        format_ether(deposit_deficit),
                        format_ether(setup_gas_cost),
                    )));
                }

                let wallet = EthereumWallet::from(signer);
                let provider = create_wallet_provider(primary_submission_rpc, wallet);
                let mut nonce = provider
                    .get_transaction_count(sender)
                    .pending()
                    .await
                    .rpc("get pending transaction count")?;
                let mut sent = 0usize;

                if deposit_deficit > U256::ZERO {
                    let tx = TransactionRequest::default()
                        .with_to(setup.weth)
                        .with_value(deposit_deficit)
                        .with_input(encode_weth_deposit())
                        .with_nonce(nonce)
                        .with_chain_id(chain_id)
                        .with_gas_limit(WETH_DEPOSIT_GAS_LIMIT)
                        .with_max_fee_per_gas(max_fee)
                        .with_max_priority_fee_per_gas(max_priority_fee);
                    let pending = provider.send_transaction(tx).await.rpc("send WETH deposit")?;
                    trace!(
                        sender = %sender,
                        tx_hash = %pending.tx_hash(),
                        amount = %deposit_deficit,
                        "WETH deposit sent"
                    );
                    let receipt = pending.get_receipt().await.rpc("confirm WETH deposit")?;
                    if !receipt.status() {
                        return Err(BaselineError::Transaction(format!(
                            "WETH deposit reverted for sender {sender}"
                        )));
                    }
                    nonce += 1;
                    sent += 1;
                }

                for (token, router) in approvals {
                    let tx = TransactionRequest::default()
                        .with_to(token)
                        .with_input(encode_erc20_approve(router, setup.approval_amount))
                        .with_nonce(nonce)
                        .with_chain_id(chain_id)
                        .with_gas_limit(ERC20_APPROVE_GAS_LIMIT)
                        .with_max_fee_per_gas(max_fee)
                        .with_max_priority_fee_per_gas(max_priority_fee);
                    let pending = provider.send_transaction(tx).await.rpc("send ERC20 approval")?;
                    trace!(
                        sender = %sender,
                        token = %token,
                        router = %router,
                        tx_hash = %pending.tx_hash(),
                        "router approval sent"
                    );
                    let receipt = pending.get_receipt().await.rpc("confirm ERC20 approval")?;
                    if !receipt.status() {
                        return Err(BaselineError::Transaction(format!(
                            "ERC20 approval reverted for sender {sender}"
                        )));
                    }
                    nonce += 1;
                    sent += 1;
                }

                if needs_pair_token {
                    let (router, data) = encode_pair_token_swap(
                        &setup,
                        sender,
                        PairSwapDirection::AcquirePairToken,
                        setup.pair_token.acquisition.amount_in(),
                        setup.pair_token.acquisition.min_amount_out(),
                    )?;
                    let tx = TransactionRequest::default()
                        .with_to(router)
                        .with_input(data)
                        .with_nonce(nonce)
                        .with_chain_id(chain_id)
                        .with_gas_limit(SETUP_SWAP_GAS_LIMIT)
                        .with_max_fee_per_gas(max_fee)
                        .with_max_priority_fee_per_gas(max_priority_fee);
                    let pending = provider.send_transaction(tx).await.rpc("send setup swap")?;
                    trace!(
                        sender = %sender,
                        router = %router,
                        tx_hash = %pending.tx_hash(),
                        "pair-token acquisition swap sent"
                    );
                    let receipt = pending.get_receipt().await.rpc("confirm setup swap")?;
                    if !receipt.status() {
                        return Err(BaselineError::Transaction(format!(
                            "pair-token acquisition swap reverted for sender {sender}"
                        )));
                    }
                    sent += 1;
                }

                Ok::<_, BaselineError>((sender, sent))
            }
        });

    let setup_results: Vec<_> = stream::iter(setup_futs)
        .buffer_unordered(ctx.concurrency)
        .inspect(|_| pb_setup.inc(1))
        .collect()
        .await;
    pb_setup.finish_and_clear();

    let mut sent_total = 0usize;
    for result in setup_results {
        let (_, sent) = result?;
        sent_total += sent;
    }

    let sender_addresses: Vec<Address> =
        ctx.accounts.accounts().iter().map(|a| a.address).collect();

    let mut weth_pending: Vec<_> =
        sender_addresses.iter().map(|sender| (setup.weth, *sender)).collect();
    let pb_weth = ctx.progress_bar(weth_pending.len() as u64, "Waiting for WETH balances");
    await_token_balances(ctx.client, &mut weth_pending, setup.weth_amount_per_sender, &pb_weth)
        .await?;
    pb_weth.finish_and_clear();

    let mut pair_pending: Vec<_> =
        sender_addresses.iter().map(|sender| (setup.pair_token.token, *sender)).collect();
    let pb_pair = ctx.progress_bar(pair_pending.len() as u64, "Waiting for pair-token balances");
    await_token_balances(
        ctx.client,
        &mut pair_pending,
        setup.pair_token.amount_per_sender,
        &pb_pair,
    )
    .await?;
    pb_pair.finish_and_clear();

    let mut allowance_pending = Vec::new();
    for sender in &sender_addresses {
        for (token, router) in &approval_targets {
            allowance_pending.push((*token, *sender, *router));
        }
    }
    let pb_allowance =
        ctx.progress_bar(allowance_pending.len() as u64, "Waiting for router allowances");
    await_erc20_allowances(
        ctx.client,
        &mut allowance_pending,
        setup.approval_amount,
        &pb_allowance,
    )
    .await?;
    pb_allowance.finish_and_clear();

    ctx.outputs.needs_sender_refresh = true;
    ctx.outputs.real_tokens_prepared = true;

    info!(accounts = total_accounts, setup_transactions = sent_total, "real-token setup complete");
    Ok(())
}

/// Recovers real-token balances by swapping the configured pair token back
/// into WETH, unwrapping WETH to native ETH, and leaving native drain to the runner.
pub async fn recover_real_tokens(
    client: &QueryProvider,
    accounts: &AccountPool,
    chain_id: u64,
    max_gas_price: u128,
    primary_submission_rpc: Url,
    hide_progress: bool,
    setup: &RealTokenSetup,
) -> Result<RealTokenRecoverySummary> {
    let client = client.clone();

    let base_fee = client.get_base_fee().await?;
    let max_priority_fee = (base_fee / 10).max(1);
    let max_fee = prep_submission_max_fee(base_fee, max_priority_fee, max_gas_price);

    let account_data: Vec<_> =
        accounts.accounts().iter().map(|a| (a.address, a.signer.clone())).collect();
    let total_accounts = account_data.len();
    let pb_recover = progress_bar(hide_progress, total_accounts as u64, "Recovering real tokens");

    let recover_futs: Vec<_> = account_data
        .into_iter()
        .map(|(sender, signer)| {
            let client = client.clone();
            let primary_submission_rpc = primary_submission_rpc.clone();
            let setup = setup.clone();
            async move {
                let mut summary = RealTokenRecoverySummary::default();
                let wallet = EthereumWallet::from(signer);
                let provider = create_wallet_provider(primary_submission_rpc, wallet);
                let mut nonce = provider
                    .get_transaction_count(sender)
                    .pending()
                    .await
                    .rpc("get pending transaction count")?;

                let pair_balance =
                    read_erc20_balance(&client, setup.pair_token.token, sender).await?;
                if pair_balance > U256::ZERO {
                    let recovery_router = setup.pair_token.acquisition.router();
                    let allowance = read_erc20_allowance(
                        &client,
                        setup.pair_token.token,
                        sender,
                        recovery_router,
                    )
                    .await?;

                    if allowance < pair_balance {
                        let approval_amount = setup.approval_amount.max(pair_balance);
                        let tx = TransactionRequest::default()
                            .with_to(setup.pair_token.token)
                            .with_input(encode_erc20_approve(recovery_router, approval_amount))
                            .with_nonce(nonce)
                            .with_chain_id(chain_id)
                            .with_gas_limit(ERC20_APPROVE_GAS_LIMIT)
                            .with_max_fee_per_gas(max_fee)
                            .with_max_priority_fee_per_gas(max_priority_fee);
                        let pending =
                            provider.send_transaction(tx).await.rpc("send pair approval")?;
                        trace!(
                            sender = %sender,
                            token = %setup.pair_token.token,
                            router = %recovery_router,
                            tx_hash = %pending.tx_hash(),
                            "recovery pair-token approval sent"
                        );
                        let receipt = pending.get_receipt().await.rpc("confirm pair approval")?;
                        if !receipt.status() {
                            return Err(BaselineError::Transaction(format!(
                                "pair-token approval reverted for sender {sender}"
                            )));
                        }
                        nonce += 1;
                    }

                    let (router, data) = encode_pair_token_swap(
                        &setup,
                        sender,
                        PairSwapDirection::RecoverWeth,
                        pair_balance,
                        U256::ZERO,
                    )?;
                    let tx = TransactionRequest::default()
                        .with_to(router)
                        .with_input(data)
                        .with_nonce(nonce)
                        .with_chain_id(chain_id)
                        .with_gas_limit(SETUP_SWAP_GAS_LIMIT)
                        .with_max_fee_per_gas(max_fee)
                        .with_max_priority_fee_per_gas(max_priority_fee);
                    let pending =
                        provider.send_transaction(tx).await.rpc("send pair-token recovery swap")?;
                    trace!(
                        sender = %sender,
                        router = %router,
                        amount = %pair_balance,
                        tx_hash = %pending.tx_hash(),
                        "pair-token recovery swap sent"
                    );
                    let receipt =
                        pending.get_receipt().await.rpc("confirm pair-token recovery swap")?;
                    if !receipt.status() {
                        return Err(BaselineError::Transaction(format!(
                            "pair-token recovery swap reverted for sender {sender}"
                        )));
                    }
                    nonce += 1;
                    summary.pair_token_swapped =
                        summary.pair_token_swapped.saturating_add(pair_balance);
                }

                let weth_balance = read_erc20_balance(&client, setup.weth, sender).await?;
                if weth_balance > U256::ZERO {
                    let tx = TransactionRequest::default()
                        .with_to(setup.weth)
                        .with_input(encode_weth_withdraw(weth_balance))
                        .with_nonce(nonce)
                        .with_chain_id(chain_id)
                        .with_gas_limit(WETH_WITHDRAW_GAS_LIMIT)
                        .with_max_fee_per_gas(max_fee)
                        .with_max_priority_fee_per_gas(max_priority_fee);
                    let pending = provider.send_transaction(tx).await.rpc("send WETH withdraw")?;
                    trace!(
                        sender = %sender,
                        amount = %weth_balance,
                        tx_hash = %pending.tx_hash(),
                        "recovery WETH withdraw sent"
                    );
                    let receipt = pending.get_receipt().await.rpc("confirm WETH withdraw")?;
                    if !receipt.status() {
                        return Err(BaselineError::Transaction(format!(
                            "WETH withdraw reverted for sender {sender}"
                        )));
                    }
                    summary.weth_unwrapped = summary.weth_unwrapped.saturating_add(weth_balance);
                }

                Ok::<_, BaselineError>(summary)
            }
        })
        .collect();

    let recover_results: Vec<_> = stream::iter(recover_futs)
        .buffer_unordered(crate::workload::chain_prep::PREP_CONCURRENCY)
        .inspect(|_| pb_recover.inc(1))
        .collect()
        .await;
    pb_recover.finish_and_clear();

    let mut summary = RealTokenRecoverySummary::default();
    for result in recover_results {
        let account_summary = result?;
        summary.pair_token_swapped =
            summary.pair_token_swapped.saturating_add(account_summary.pair_token_swapped);
        summary.weth_unwrapped =
            summary.weth_unwrapped.saturating_add(account_summary.weth_unwrapped);
    }

    info!(
        pair_token_swapped = %summary.pair_token_swapped,
        weth_unwrapped = %summary.weth_unwrapped,
        "real-token recovery complete"
    );
    Ok(summary)
}

async fn read_erc20_balance(
    client: &QueryProvider,
    token: Address,
    owner: Address,
) -> Result<U256> {
    client
        .call(
            TransactionRequest::default()
                .with_to(token)
                .with_input(encode_erc20_balance_of(owner))
                .into(),
        )
        .await
        .rpc("eth_call balanceOf")
        .map(|bytes| U256::from_be_slice(bytes.as_ref()))
}

async fn read_erc20_allowance(
    client: &QueryProvider,
    token: Address,
    owner: Address,
    spender: Address,
) -> Result<U256> {
    client
        .call(
            TransactionRequest::default()
                .with_to(token)
                .with_input(encode_erc20_allowance(owner, spender))
                .into(),
        )
        .await
        .rpc("eth_call allowance")
        .map(|bytes| U256::from_be_slice(bytes.as_ref()))
}

fn encode_weth_deposit() -> Bytes {
    sol! {
        function deposit() external payable;
    }
    Bytes::from(depositCall {}.abi_encode())
}

fn encode_weth_withdraw(amount: U256) -> Bytes {
    sol! {
        function withdraw(uint256 wad) external;
    }
    Bytes::from(withdrawCall { wad: amount }.abi_encode())
}

fn encode_erc20_approve(spender: Address, amount: U256) -> Bytes {
    sol! {
        function approve(address spender, uint256 amount) external returns (bool);
    }
    Bytes::from(approveCall { spender, amount }.abi_encode())
}

fn encode_erc20_allowance(owner: Address, spender: Address) -> Bytes {
    sol! {
        function allowance(address owner, address spender) external view returns (uint256);
    }
    Bytes::from(allowanceCall { owner, spender }.abi_encode())
}

fn encode_pair_token_swap(
    setup: &RealTokenSetup,
    recipient: Address,
    direction: PairSwapDirection,
    amount_in: U256,
    min_amount_out: U256,
) -> Result<(Address, Bytes)> {
    let (token_in, token_out) = direction.tokens(setup);
    match &setup.pair_token.acquisition {
        RealTokenAcquisition::UniswapV3ExactInput { router, fee, .. } => {
            let call = IRealTokenUniswapV3Router::exactInputSingleCall {
                params: IRealTokenUniswapV3Router::ExactInputSingleParams {
                    tokenIn: token_in,
                    tokenOut: token_out,
                    fee: U24::from(*fee),
                    recipient,
                    amountIn: amount_in,
                    amountOutMinimum: min_amount_out,
                    sqrtPriceLimitX96: U160::ZERO,
                },
            };
            Ok((*router, Bytes::from(call.abi_encode())))
        }
        RealTokenAcquisition::AerodromeClExactInput { router, tick_spacing, .. } => {
            let tick_spacing = I24::try_from(*tick_spacing).map_err(|e| {
                BaselineError::Config(format!("real-token swap tick spacing does not fit i24: {e}"))
            })?;
            let call = IRealTokenAerodromeClRouter::exactInputSingleCall {
                params: IRealTokenAerodromeClRouter::ExactInputSingleParams {
                    tokenIn: token_in,
                    tokenOut: token_out,
                    tickSpacing: tick_spacing,
                    recipient,
                    deadline: U256::from(u64::MAX),
                    amountIn: amount_in,
                    amountOutMinimum: min_amount_out,
                    sqrtPriceLimitX96: U160::ZERO,
                },
            };
            Ok((*router, Bytes::from(call.abi_encode())))
        }
    }
}

/// Waits for ERC20 allowances to reach a target.
async fn await_erc20_allowances(
    client: &QueryProvider,
    pending_allowances: &mut Vec<(Address, Address, Address)>,
    target_allowance: U256,
    pb: &ProgressBar,
) -> Result<usize> {
    let timeout = Duration::from_secs(60);
    let poll_interval = Duration::from_millis(500);
    let start = Instant::now();
    let mut settled = 0usize;

    while !pending_allowances.is_empty() && start.elapsed() < timeout {
        tokio::time::sleep(poll_interval).await;

        let mut still_pending = Vec::new();
        for (token, owner, spender) in pending_allowances.drain(..) {
            match read_erc20_allowance(client, token, owner, spender).await {
                Ok(allowance) if allowance >= target_allowance => {
                    trace!(token = %token, owner = %owner, spender = %spender, "allowance settled");
                    settled += 1;
                    pb.inc(1);
                }
                Ok(_) => {
                    still_pending.push((token, owner, spender));
                }
                Err(e) => {
                    trace!(
                        token = %token,
                        owner = %owner,
                        spender = %spender,
                        error = %e,
                        "failed to check allowance"
                    );
                    still_pending.push((token, owner, spender));
                }
            }
        }
        *pending_allowances = still_pending;
    }

    if !pending_allowances.is_empty() {
        let sample: Vec<_> = pending_allowances.iter().take(3).copied().collect();
        return Err(BaselineError::Transaction(format!(
            "{} allowances did not reach target within timeout; sample: {sample:?}",
            pending_allowances.len(),
        )));
    }

    Ok(settled)
}
