use std::{
    collections::{BTreeSet, HashMap, HashSet},
    time::{Duration, Instant},
};

use alloy_network::{EthereumWallet, TransactionBuilder};
use alloy_primitives::{Address, Bytes, Signed, TxHash, U160, U256, Uint, utils::format_ether};
use alloy_provider::Provider;
use alloy_rpc_types::{BlockNumberOrTag, TransactionRequest};
use alloy_sol_types::{SolCall, sol};
use futures::{StreamExt, stream};
use indicatif::ProgressBar;
use tracing::{debug, info, instrument, warn};

use super::{
    BlockReceipt, BlockWatcher, GasPricer, RealTokenAcquisition, RealTokenRecoverySummary,
    RealTokenSetup,
    load_runner::{FUNDING_CONCURRENCY, LoadRunner},
};
use crate::{
    BaselineError, Result,
    rpc::{BaseFeeExt, QueryProvider, RpcResultExt, create_wallet_provider},
};

const WETH_DEPOSIT_GAS_LIMIT: u64 = 100_000;
const WETH_WITHDRAW_GAS_LIMIT: u64 = 45_000;
const ERC20_APPROVE_GAS_LIMIT: u64 = 65_000;
const SETUP_SWAP_GAS_LIMIT: u64 = 250_000;
/// Timeout for confirming setup/recovery transactions via the block watcher.
const REAL_TOKEN_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(120);
/// A single sent transaction, labeled for descriptive revert/timeout errors.
type LabeledTxHash = (TxHash, &'static str);

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

impl LoadRunner {
    fn collect_real_token_setup_approvals(
        &self,
        setup: &RealTokenSetup,
    ) -> Vec<(Address, Address)> {
        let mut approvals = BTreeSet::new();
        for router in self.collect_swap_routers() {
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
    #[instrument(skip(self, setup), fields(accounts = self.accounts.len()))]
    pub async fn setup_real_tokens(&mut self, setup: &RealTokenSetup) -> Result<()> {
        let swap_routers = self.collect_swap_routers();
        if swap_routers.is_empty() {
            return Err(BaselineError::Config(
                "real-token setup requires at least one swap router".into(),
            ));
        }
        let approval_targets = self.collect_real_token_setup_approvals(setup);

        let base_fee = self.client.get_base_fee().await?;
        let fees = GasPricer::new(self.config.max_gas_price).fees_for(base_fee);

        let account_data: Vec<_> =
            self.accounts.accounts().iter().map(|a| (a.address, a.signer.clone())).collect();
        let client = self.client.clone();
        let primary_submission_rpc = self.config.primary_submission_rpc().clone();
        let chain_id = self.config.chain_id;
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

        let pb_setup = self.progress_bar(total_accounts as u64, "Setting up real tokens");
        let setup_futs = account_data.into_iter().map(|(sender, signer)| {
            let client = client.clone();
            let primary_submission_rpc = primary_submission_rpc.clone();
            let setup = setup.clone();
            let approval_targets = approval_targets.clone();
            async move {
                let weth_balance = Self::read_erc20_balance(&client, setup.weth, sender).await?;
                let pair_balance =
                    Self::read_erc20_balance(&client, setup.pair_token.token, sender).await?;
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
                        Self::read_erc20_allowance(&client, *token, sender, *router).await?;
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
                let setup_gas_cost =
                    U256::from(setup_gas_limit).saturating_mul(U256::from(fees.max_fee));
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
                // Steps are submitted back-to-back at increasing nonces without waiting for
                // each one to land; same-sender nonce ordering guarantees they can only be
                // included in that order. Confirmation happens once, for every sender, via a
                // single block-watching pass below.
                let mut hashes: Vec<LabeledTxHash> = Vec::new();

                if deposit_deficit > U256::ZERO {
                    let tx = TransactionRequest::default()
                        .with_to(setup.weth)
                        .with_value(deposit_deficit)
                        .with_input(Self::encode_weth_deposit())
                        .with_nonce(nonce)
                        .with_chain_id(chain_id)
                        .with_gas_limit(WETH_DEPOSIT_GAS_LIMIT)
                        .with_max_fee_per_gas(fees.max_fee)
                        .with_max_priority_fee_per_gas(fees.priority_fee);
                    let pending = provider.send_transaction(tx).await.map_err(|e| {
                        BaselineError::Transaction(format!(
                            "WETH deposit failed to send for sender {sender}: {e}"
                        ))
                    })?;
                    hashes.push((*pending.tx_hash(), "WETH deposit"));
                    nonce += 1;
                }

                for (token, router) in approvals {
                    let tx = TransactionRequest::default()
                        .with_to(token)
                        .with_input(Self::encode_erc20_approve(router, setup.approval_amount))
                        .with_nonce(nonce)
                        .with_chain_id(chain_id)
                        .with_gas_limit(ERC20_APPROVE_GAS_LIMIT)
                        .with_max_fee_per_gas(fees.max_fee)
                        .with_max_priority_fee_per_gas(fees.priority_fee);
                    let pending = provider.send_transaction(tx).await.map_err(|e| {
                        BaselineError::Transaction(format!(
                            "ERC20 approval failed to send for sender {sender}: {e}"
                        ))
                    })?;
                    hashes.push((*pending.tx_hash(), "ERC20 approval"));
                    nonce += 1;
                }

                if needs_pair_token {
                    let (router, data) = Self::encode_pair_token_swap(
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
                        .with_max_fee_per_gas(fees.max_fee)
                        .with_max_priority_fee_per_gas(fees.priority_fee);
                    let pending = provider.send_transaction(tx).await.map_err(|e| {
                        BaselineError::Transaction(format!(
                            "pair-token acquisition swap failed to send for sender {sender}: {e}"
                        ))
                    })?;
                    hashes.push((*pending.tx_hash(), "pair-token acquisition swap"));
                }

                Ok::<_, BaselineError>((sender, hashes))
            }
        });

        let setup_results: Vec<_> = stream::iter(setup_futs)
            .buffer_unordered(FUNDING_CONCURRENCY)
            .inspect(|_| pb_setup.inc(1))
            .collect()
            .await;
        pb_setup.finish_and_clear();

        let mut sent_total = 0usize;
        let mut hashes_by_sender = Vec::new();
        for result in setup_results {
            let (sender, hashes) = result?;
            sent_total += hashes.len();
            hashes_by_sender.push((sender, hashes));
        }
        Self::confirm_real_token_txs(&self.client, &hashes_by_sender).await?;

        let sender_addresses: Vec<Address> =
            self.accounts.accounts().iter().map(|a| a.address).collect();

        let mut weth_pending: Vec<_> =
            sender_addresses.iter().map(|sender| (setup.weth, *sender)).collect();
        let pb_weth = self.progress_bar(weth_pending.len() as u64, "Waiting for WETH balances");
        Self::await_token_balances(
            &self.client,
            &mut weth_pending,
            setup.weth_amount_per_sender,
            &pb_weth,
        )
        .await?;
        pb_weth.finish_and_clear();

        let mut pair_pending: Vec<_> =
            sender_addresses.iter().map(|sender| (setup.pair_token.token, *sender)).collect();
        let pb_pair =
            self.progress_bar(pair_pending.len() as u64, "Waiting for pair-token balances");
        Self::await_token_balances(
            &self.client,
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
            self.progress_bar(allowance_pending.len() as u64, "Waiting for router allowances");
        Self::await_erc20_allowances(
            &self.client,
            &mut allowance_pending,
            setup.approval_amount,
            &pb_allowance,
        )
        .await?;
        pb_allowance.finish_and_clear();

        self.refresh_sender_state().await?;

        info!(
            accounts = total_accounts,
            setup_transactions = sent_total,
            "real-token setup complete"
        );
        Ok(())
    }

    /// Recovers real-token balances by swapping the configured pair token back
    /// into WETH, unwrapping WETH to native ETH, and leaving native drain to
    /// [`Self::drain_accounts`].
    pub async fn recover_real_tokens(
        &self,
        setup: &RealTokenSetup,
    ) -> Result<RealTokenRecoverySummary> {
        let client = self.client.clone();
        let primary_submission_rpc = self.config.primary_submission_rpc().clone();
        let chain_id = self.config.chain_id;

        let base_fee = client.get_base_fee().await?;
        let fees = GasPricer::new(self.config.max_gas_price).fees_for(base_fee);

        let account_data: Vec<_> =
            self.accounts.accounts().iter().map(|a| (a.address, a.signer.clone())).collect();
        let total_accounts = account_data.len();
        let pb_recover = self.progress_bar(total_accounts as u64, "Recovering real tokens");

        let recover_futs: Vec<_> = account_data
            .into_iter()
            .map(|(sender, signer)| {
                let client = client.clone();
                let primary_submission_rpc = primary_submission_rpc.clone();
                let setup = setup.clone();
                async move {
                    let wallet = EthereumWallet::from(signer);
                    let provider = create_wallet_provider(primary_submission_rpc, wallet);
                    let mut nonce = provider
                        .get_transaction_count(sender)
                        .pending()
                        .await
                        .rpc("get pending transaction count")?;
                    // Steps are submitted back-to-back without waiting for confirmation; see
                    // the block-watching pass below and `confirm_real_token_txs`.
                    let mut hashes: Vec<LabeledTxHash> = Vec::new();
                    let mut pair_swap: Option<(TxHash, U256)> = None;
                    let mut weth_withdraw: Option<(TxHash, U256)> = None;

                    let pair_balance =
                        Self::read_erc20_balance(&client, setup.pair_token.token, sender).await?;
                    if pair_balance > U256::ZERO {
                        let recovery_router = setup.pair_token.acquisition.router();
                        let allowance = Self::read_erc20_allowance(
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
                                .with_input(Self::encode_erc20_approve(
                                    recovery_router,
                                    approval_amount,
                                ))
                                .with_nonce(nonce)
                                .with_chain_id(chain_id)
                                .with_gas_limit(ERC20_APPROVE_GAS_LIMIT)
                                .with_max_fee_per_gas(fees.max_fee)
                                .with_max_priority_fee_per_gas(fees.priority_fee);
                            let pending = provider.send_transaction(tx).await.map_err(|e| {
                                BaselineError::Transaction(format!(
                                    "pair-token approval failed to send for sender {sender}: {e}"
                                ))
                            })?;
                            hashes.push((*pending.tx_hash(), "pair-token approval"));
                            nonce += 1;
                        }

                        let (router, data) = Self::encode_pair_token_swap(
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
                            .with_max_fee_per_gas(fees.max_fee)
                            .with_max_priority_fee_per_gas(fees.priority_fee);
                        let pending = provider.send_transaction(tx).await.map_err(|e| {
                            BaselineError::Transaction(format!(
                                "pair-token recovery swap failed to send for sender {sender}: {e}"
                            ))
                        })?;
                        hashes.push((*pending.tx_hash(), "pair-token recovery swap"));
                        pair_swap = Some((*pending.tx_hash(), pair_balance));
                        nonce += 1;
                    }

                    let weth_balance =
                        Self::read_erc20_balance(&client, setup.weth, sender).await?;
                    if weth_balance > U256::ZERO {
                        let tx = TransactionRequest::default()
                            .with_to(setup.weth)
                            .with_input(Self::encode_weth_withdraw(weth_balance))
                            .with_nonce(nonce)
                            .with_chain_id(chain_id)
                            .with_gas_limit(WETH_WITHDRAW_GAS_LIMIT)
                            .with_max_fee_per_gas(fees.max_fee)
                            .with_max_priority_fee_per_gas(fees.priority_fee);
                        let pending = provider.send_transaction(tx).await.map_err(|e| {
                            BaselineError::Transaction(format!(
                                "WETH withdraw failed to send for sender {sender}: {e}"
                            ))
                        })?;
                        hashes.push((*pending.tx_hash(), "WETH withdraw"));
                        weth_withdraw = Some((*pending.tx_hash(), weth_balance));
                    }

                    Ok::<_, BaselineError>((sender, hashes, pair_swap, weth_withdraw))
                }
            })
            .collect();

        let recover_results: Vec<_> = stream::iter(recover_futs)
            .buffer_unordered(FUNDING_CONCURRENCY)
            .inspect(|_| pb_recover.inc(1))
            .collect()
            .await;
        pb_recover.finish_and_clear();

        let mut hashes_by_sender = Vec::new();
        let mut pair_swaps = Vec::new();
        let mut weth_withdraws = Vec::new();
        for result in recover_results {
            let (sender, hashes, pair_swap, weth_withdraw) = result?;
            hashes_by_sender.push((sender, hashes));
            pair_swaps.extend(pair_swap);
            weth_withdraws.extend(weth_withdraw);
        }
        let receipts = Self::confirm_real_token_txs(&client, &hashes_by_sender).await?;

        let mut summary = RealTokenRecoverySummary::default();
        for (tx_hash, amount) in pair_swaps {
            if receipts.get(&tx_hash).is_some_and(|r| r.success) {
                summary.pair_token_swapped = summary.pair_token_swapped.saturating_add(amount);
            }
        }
        for (tx_hash, amount) in weth_withdraws {
            if receipts.get(&tx_hash).is_some_and(|r| r.success) {
                summary.weth_unwrapped = summary.weth_unwrapped.saturating_add(amount);
            }
        }

        info!(
            pair_token_swapped = %summary.pair_token_swapped,
            weth_unwrapped = %summary.weth_unwrapped,
            "real-token recovery complete"
        );
        Ok(summary)
    }

    /// Confirms every sender's submitted transactions by watching canonical blocks and
    /// batch-fetching receipts for the touched blocks, instead of polling
    /// `eth_getTransactionReceipt` once per transaction. Returns the receipts so callers
    /// that need per-transaction outcomes (e.g. recovery's swapped/unwrapped amounts) can
    /// look them up by hash.
    async fn confirm_real_token_txs(
        client: &QueryProvider,
        hashes_by_sender: &[(Address, Vec<LabeledTxHash>)],
    ) -> Result<HashMap<TxHash, BlockReceipt>> {
        let pending_hashes: HashSet<TxHash> = hashes_by_sender
            .iter()
            .flat_map(|(_, hashes)| hashes.iter().map(|(hash, _)| *hash))
            .collect();
        if pending_hashes.is_empty() {
            return Ok(HashMap::new());
        }

        let receipts = BlockWatcher::confirm_and_fetch_receipts(
            client,
            pending_hashes,
            REAL_TOKEN_CONFIRMATION_TIMEOUT,
            |_hash| {},
        )
        .await?;

        let mut failed = 0usize;
        let mut first_error: Option<String> = None;
        for (sender, hashes) in hashes_by_sender {
            for (tx_hash, label) in hashes {
                match receipts.get(tx_hash) {
                    Some(receipt) if receipt.success => {}
                    Some(_) => {
                        failed += 1;
                        warn!(sender = %sender, tx_hash = %tx_hash, label, "real-token transaction reverted");
                        first_error.get_or_insert_with(|| {
                            format!("{label} reverted for sender {sender} (tx {tx_hash})")
                        });
                    }
                    None => {
                        failed += 1;
                        warn!(sender = %sender, tx_hash = %tx_hash, label, "real-token transaction receipt unavailable");
                        first_error.get_or_insert_with(|| {
                            format!(
                                "{label} receipt unavailable for sender {sender} (tx {tx_hash})"
                            )
                        });
                    }
                }
            }
        }

        if failed > 0 {
            return Err(BaselineError::Transaction(format!(
                "{failed} real-token setup/recovery transaction(s) failed: {}",
                first_error.unwrap_or_else(|| "unknown error".to_string())
            )));
        }
        Ok(receipts)
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
                    .with_input(Self::encode_erc20_balance_of(owner))
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
                    .with_input(Self::encode_erc20_allowance(owner, spender))
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
                    BaselineError::Config(format!(
                        "real-token swap tick spacing does not fit i24: {e}"
                    ))
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
                match Self::read_erc20_allowance(client, token, owner, spender).await {
                    Ok(allowance) if allowance >= target_allowance => {
                        debug!(token = %token, owner = %owner, spender = %spender, "allowance settled");
                        settled += 1;
                        pb.inc(1);
                    }
                    Ok(_) => {
                        still_pending.push((token, owner, spender));
                    }
                    Err(e) => {
                        warn!(
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
            return Err(BaselineError::Transaction(format!(
                "allowances did not reach target within timeout: {pending_allowances:?}"
            )));
        }

        Ok(settled)
    }
}
