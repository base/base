//! Account funding, draining, txpool clearing, and swap-token mint helpers.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_network::{Ethereum, EthereumWallet, TransactionBuilder};
use alloy_primitives::{Address, Bytes, U256, utils::format_ether};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types::{BlockNumberOrTag, TransactionRequest};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, sol};
use futures::{StreamExt, stream};
use indicatif::{ProgressBar, ProgressStyle};
use tracing::{debug, error, info, instrument, trace, warn};

use super::{GasPricer, LoadRunner, TxType, load_runner::NONCE_RPC_TIMEOUT, nonce::NonceManager};
use crate::{
    BaselineError, Result,
    rpc::{BaseFeeExt, QueryProvider, RpcResultExt, TxpoolAdminClient, create_wallet_provider},
    workload::{await_token_balances, encode_erc20_balance_of},
};

/// Maximum number of concurrent RPC requests during funding/draining operations.
pub(super) const FUNDING_CONCURRENCY: usize = 32;

const FUNDING_REPLACEMENT_FEE_MULTIPLIER: u128 = 3;
const FUNDING_REPLACEMENT_MAX_ATTEMPTS: u32 = 8;

const TXPOOL_CLEAR_CONCURRENCY: usize = 64;
const PENDING_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(200);

impl LoadRunner {
    /// Funds all accounts from a funding key up to the specified amount.
    pub async fn fund_accounts(
        &mut self,
        funding_key: PrivateKeySigner,
        amount_per_account: U256,
    ) -> Result<()> {
        let total_accounts = self.accounts.len();
        let client = self.client.clone();
        let primary_submission_rpc = self.config.primary_submission_rpc().clone();
        let chain_id = self.config.chain_id;
        let pricer = GasPricer::new(self.config.max_gas_price);

        let pb_check = self.progress_bar(total_accounts as u64, "Checking balances");

        // Phase 1: Parallel balance + nonce queries.
        let addresses: Vec<(Address, usize)> =
            self.accounts.accounts().iter().enumerate().map(|(i, a)| (a.address, i)).collect();

        let balance_futs: Vec<_> = addresses
            .iter()
            .map(|&(addr, idx)| {
                let client = client.clone();
                async move {
                    let balance = client.get_balance(addr).await.rpc("get balance")?;
                    let nonce =
                        client.get_transaction_count(addr).await.rpc("get transaction count")?;
                    Ok::<_, BaselineError>((addr, idx, balance, nonce))
                }
            })
            .collect();

        let results: Vec<_> = stream::iter(balance_futs)
            .buffer_unordered(FUNDING_CONCURRENCY)
            .inspect(|_| pb_check.inc(1))
            .collect()
            .await;
        pb_check.finish_and_clear();

        let mut accounts_to_fund = Vec::new();
        for result in results {
            let (addr, idx, balance, nonce) = result?;
            let account = &mut self.accounts.accounts_mut()[idx];
            account.balance = balance;
            account.nonce = nonce;

            if balance < amount_per_account {
                let deficit = amount_per_account.saturating_sub(balance);
                accounts_to_fund.push((addr, deficit));
            } else {
                trace!(address = %addr, balance = %balance, "account already funded");
            }
        }

        let funder_address = funding_key.address();
        let wallet = EthereumWallet::from(funding_key);
        let funder_provider =
            Arc::new(create_wallet_provider(primary_submission_rpc.clone(), wallet));

        let mut txpool_endpoints = vec![primary_submission_rpc];
        txpool_endpoints.extend(self.config.txpool_nodes.iter().cloned());
        txpool_endpoints.sort();
        txpool_endpoints.dedup();
        for (endpoint_index, endpoint) in txpool_endpoints.iter().enumerate() {
            let txpool_client = TxpoolAdminClient::new(endpoint.clone())?;
            match txpool_client.drop_sender_transactions(funder_address).await {
                Ok(removed) if !removed.is_empty() => {
                    info!(
                        endpoint_index,
                        removed = removed.len(),
                        "dropped stale exclusive-funder transactions"
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    debug!(
                        endpoint_index,
                        error = %error,
                        "funder txpool admin cleanup unavailable"
                    );
                }
            }
        }

        // The funder is exclusively owned by this load tester, so reclaim any pending nonce range
        // left by an interrupted run instead of appending new transfers behind stale transactions.
        let canonical_nonce = funder_provider
            .get_transaction_count(funder_address)
            .await
            .rpc("get canonical transaction count")?;
        let pending_nonce = funder_provider
            .get_transaction_count(funder_address)
            .pending()
            .await
            .rpc("get pending transaction count")?;
        if pending_nonce < canonical_nonce {
            return Err(BaselineError::Transaction(format!(
                "inconsistent funder nonces: canonical {canonical_nonce}, pending {pending_nonce}"
            )));
        }
        let mut highest_txpool_nonce = None;
        let mut txpool_content_available = false;
        let mut txpool_content_failures = 0usize;
        let mut queued_funder_transactions = 0usize;
        for (endpoint_index, endpoint) in txpool_endpoints.iter().enumerate() {
            let txpool_client = TxpoolAdminClient::new(endpoint.clone())?;
            match txpool_client.sender_transaction_nonces(funder_address).await {
                Ok((pending_nonces, queued_nonces)) => {
                    txpool_content_available = true;
                    queued_funder_transactions =
                        queued_funder_transactions.saturating_add(queued_nonces.len());
                    let nonces = pending_nonces.into_iter().chain(queued_nonces);
                    if let Some(highest) = nonces.into_iter().max() {
                        highest_txpool_nonce = Some(
                            highest_txpool_nonce
                                .map_or(highest, |current: u64| current.max(highest)),
                        );
                    }
                }
                Err(error) => {
                    txpool_content_failures = txpool_content_failures.saturating_add(1);
                    debug!(
                        endpoint_index,
                        error = %error,
                        "sender txpool content unavailable"
                    );
                }
            }
        }
        if queued_funder_transactions > 0 {
            return Err(BaselineError::Transaction(format!(
                "exclusive funder {funder_address} still has {queued_funder_transactions} queued transaction(s) after cleanup; top up the funder for cumulative max-cost affordability or enable admin_dropSenderTransactions on every txpool node"
            )));
        }
        if !txpool_content_available {
            warn!(
                endpoint_count = txpool_endpoints.len(),
                failed_endpoint_count = txpool_content_failures,
                "sender txpool content unavailable; queued funder transactions behind nonce gaps cannot be discovered"
            );
        }
        let stale_end_nonce = highest_txpool_nonce
            .and_then(|nonce| nonce.checked_add(1))
            .unwrap_or(pending_nonce)
            .max(pending_nonce);
        let stale_nonce_count = stale_end_nonce.checked_sub(canonical_nonce).ok_or_else(|| {
            BaselineError::Transaction(format!(
                "inconsistent funder txpool range: canonical {canonical_nonce}, stale end {stale_end_nonce}"
            ))
        })?;

        if accounts_to_fund.is_empty() && stale_nonce_count == 0 {
            info!("all accounts already have sufficient balance, skipping funding");
            return Ok(());
        }

        if stale_nonce_count > 0 {
            warn!(
                from = %funder_address,
                canonical_nonce,
                pending_nonce,
                stale_end_nonce,
                stale_nonce_count,
                "replacing stale pending funder transactions"
            );
        }

        // Phase 2: Early balance validation — abort before sending any TXs if
        // the funder cannot cover the total cost, including cancellations for any
        // stale nonce tail longer than the set of accounts that need funding.
        let total_deficit: U256 = accounts_to_fund
            .iter()
            .map(|(_, deficit)| *deficit)
            .fold(U256::ZERO, |a, b| a.saturating_add(b));
        let funding_request_count =
            accounts_to_fund.len().max(usize::try_from(stale_nonce_count).map_err(|_| {
                BaselineError::Transaction("stale funder nonce range exceeds usize".into())
            })?);
        // Reth only classifies the full nonce chain as executable when the funder can afford every
        // transaction's maximum declared L2 cost. Budget at the same `2 * base_fee` quote the
        // funding txs will declare (1 wei tip) so affordability matches broadcast fees.
        let base_fee = client.get_base_fee().await?;
        let fees = pricer.funding_fees_for(base_fee);
        let mut gas_cost_per_tx = U256::from(21_000u64).saturating_mul(U256::from(fees.max_fee));
        let total_gas_cost = gas_cost_per_tx.saturating_mul(U256::from(funding_request_count));
        let total_needed = total_deficit.saturating_add(total_gas_cost);

        let funder_balance = client.get_balance(funder_address).await.rpc("get balance")?;

        if funder_balance < total_needed {
            let shortfall = total_needed.saturating_sub(funder_balance);
            return Err(BaselineError::Transaction(format!(
                "funder {} has insufficient balance: has {} ETH, needs {} ETH (deficit {} ETH + gas {} ETH), shortfall {} ETH",
                funder_address,
                format_ether(funder_balance),
                format_ether(total_needed),
                format_ether(total_deficit),
                format_ether(total_gas_cost),
                format_ether(shortfall),
            )));
        }

        info!(
            from = %funder_address,
            amount = %amount_per_account,
            accounts_needing_funds = accounts_to_fund.len(),
            stale_nonce_count,
            cancellation_count = funding_request_count.saturating_sub(accounts_to_fund.len()),
            funder_balance = %format_ether(funder_balance),
            total_needed = %format_ether(total_needed),
            "funding accounts"
        );

        // Phase 3+4: Send funding TXs in batches and confirm each batch before
        // sending the next. Existing stale nonces are reused for current funding
        // transfers; any stale tail is cancelled with zero-value self-transfers.
        let funding_requests: VecDeque<(Address, U256, u64, bool)> = (0..funding_request_count)
            .map(|i| {
                let nonce = canonical_nonce
                    .checked_add(u64::try_from(i).expect("account index exceeds u64"))
                    .expect("nonce overflow");
                match accounts_to_fund.get(i) {
                    Some(&(address, deficit)) => (address, deficit, nonce, true),
                    None => (funder_address, U256::ZERO, nonce, false),
                }
            })
            .collect();

        let total_txs = accounts_to_fund.len() as u64;
        let pb_fund = self.progress_bar(total_txs, "Funding accounts");
        let mut txs_remaining = funding_requests;
        let mut next_retry_nonce = canonical_nonce
            .checked_add(u64::try_from(funding_request_count).expect("request count exceeds u64"))
            .expect("nonce overflow");
        while !txs_remaining.is_empty() {
            let base_fee = client.get_base_fee().await?;
            let fees = pricer.funding_fees_for(base_fee);
            gas_cost_per_tx = U256::from(21_000u64).saturating_mul(U256::from(fees.max_fee));
            info!(
                base_fee,
                max_fee = fees.max_fee,
                priority_fee = fees.priority_fee,
                "pricing funding transaction batch"
            );
            let batch: Vec<_> = (0..self.config.max_in_flight_per_sender)
                .filter_map(|_| txs_remaining.pop_front())
                .collect();
            let reclaimed_nonce_target = batch
                .iter()
                .filter_map(|(_, _, nonce, _)| (*nonce < stale_end_nonce).then_some(*nonce + 1))
                .max();
            let mut batch_pending: Vec<Address> = Vec::with_capacity(batch.len());
            let mut retries: Vec<(Address, U256, u64, bool)> = Vec::new();
            let mut consumed_funding_nonces: Vec<(Address, U256, u64)> = Vec::new();
            let mut existing_nonce_targets = Vec::new();
            let mut fatal_errors: Vec<String> = Vec::new();

            let send_futs = batch.into_iter().map(|(address, deficit, nonce, fund_account)| {
                let provider = Arc::clone(&funder_provider);
                async move {
                    let tx = TransactionRequest::default()
                        .with_to(address)
                        .with_value(deficit)
                        .with_nonce(nonce)
                        .with_chain_id(chain_id)
                        .with_gas_limit(21_000)
                        .with_max_fee_per_gas(fees.max_fee)
                        .with_max_priority_fee_per_gas(fees.priority_fee);
                    let result = provider.send_transaction(tx).await;
                    (result, address, deficit, nonce, fund_account)
                }
            });

            let mut send_stream =
                stream::iter(send_futs).buffer_unordered(self.config.max_in_flight_per_sender);

            while let Some((result, address, deficit, nonce, fund_account)) =
                send_stream.next().await
            {
                match result {
                    Ok(pending) => {
                        let tx_hash = *pending.tx_hash();
                        trace!(to = %address, deficit = %deficit, nonce, tx_hash = %tx_hash, fund_account, "funder transaction sent");
                        if fund_account {
                            batch_pending.push(address);
                        }
                    }
                    Err(e) => {
                        let error_str = e.to_string();
                        if error_str.contains("already known") {
                            // The provider signed the exact request above, so "already known"
                            // identifies this intended transaction rather than an arbitrary tx at
                            // the same nonce.
                            trace!(to = %address, nonce, fund_account, "funder transaction already pending");
                            if fund_account {
                                batch_pending.push(address);
                            }
                        } else if error_str.contains("replacement transaction underpriced") {
                            retries.push((address, deficit, nonce, fund_account));
                        } else if error_str.contains("nonce too low") {
                            // A stale transaction raced this replacement into a block. Waiting for
                            // its canonical state below determines whether the intended recipient
                            // still needs a new transfer at the end of the reclaimed nonce range.
                            trace!(to = %address, nonce, fund_account, "funder nonce already consumed");
                            existing_nonce_targets.push(nonce.saturating_add(1));
                            if fund_account {
                                consumed_funding_nonces.push((address, deficit, nonce));
                            }
                        } else {
                            error!(to = %address, nonce, fund_account, error = %e, "failed to send funder transaction");
                            fatal_errors.push(format!(
                                "failed to send funder nonce {nonce} to {address}: {e}"
                            ));
                        }
                    }
                }
            }

            if !fatal_errors.is_empty() {
                pb_fund.finish_and_clear();
                return Err(BaselineError::Transaction(format!(
                    "{} funding tx(s) failed: {}",
                    fatal_errors.len(),
                    fatal_errors.join("; "),
                )));
            }

            if !retries.is_empty() {
                let replacement_addresses: Vec<Address> = retries
                    .iter()
                    .filter_map(|(address, _, _, fund)| fund.then_some(*address))
                    .collect();
                let retry_futs = retries.into_iter().map(|(address, deficit, nonce, fund_account)| {
                    let provider = Arc::clone(&funder_provider);
                    async move {
                        let mut replacement_fees = fees;

                        for attempt in 1..=FUNDING_REPLACEMENT_MAX_ATTEMPTS {
                            let next_fees =
                                pricer.bumped(replacement_fees, FUNDING_REPLACEMENT_FEE_MULTIPLIER);
                            if next_fees == replacement_fees {
                                // A transaction from a previous run may already use the configured
                                // absolute fee cap and therefore be impossible to replace. Let the
                                // executable original settle, then verify the intended recipient.
                                return Ok((
                                    address,
                                    deficit,
                                    nonce,
                                    None,
                                    attempt,
                                    fund_account,
                                    true,
                                ));
                            }
                            replacement_fees = next_fees;

                            let replacement = TransactionRequest::default()
                                .with_to(address)
                                .with_value(deficit)
                                .with_nonce(nonce)
                                .with_chain_id(chain_id)
                                .with_gas_limit(21_000)
                                .with_max_fee_per_gas(replacement_fees.max_fee)
                                .with_max_priority_fee_per_gas(replacement_fees.priority_fee);

                            match provider.send_transaction(replacement).await {
                                Ok(pending) => {
                                    return Ok((
                                        address,
                                        deficit,
                                        nonce,
                                        Some(*pending.tx_hash()),
                                        attempt,
                                        fund_account,
                                        false,
                                    ));
                                }
                                Err(e) => {
                                    let error = e.to_string();
                                    if error.contains("already known") {
                                        return Ok((
                                            address,
                                            deficit,
                                            nonce,
                                            None,
                                            attempt,
                                            fund_account,
                                            false,
                                        ));
                                    }
                                    if error.contains("nonce too low") {
                                        return Ok((
                                            address,
                                            deficit,
                                            nonce,
                                            None,
                                            attempt,
                                            fund_account,
                                            true,
                                        ));
                                    }
                                    if !error.contains("replacement transaction underpriced") {
                                        return Err(format!(
                                            "replacement funding tx for {address} nonce {nonce} failed: {e}"
                                        ));
                                    }
                                    warn!(
                                        to = %address,
                                        nonce,
                                        attempt,
                                        replacement_max_fee = replacement_fees.max_fee,
                                        replacement_priority_fee = replacement_fees.priority_fee,
                                        "replacement funding transaction still underpriced"
                                    );
                                }
                            }
                        }

                        Ok((
                            address,
                            deficit,
                            nonce,
                            None,
                            FUNDING_REPLACEMENT_MAX_ATTEMPTS,
                            fund_account,
                            true,
                        ))
                    }
                });

                let mut retry_stream =
                    stream::iter(retry_futs).buffer_unordered(self.config.max_in_flight_per_sender);

                while let Some(result) = retry_stream.next().await {
                    match result {
                        Ok((
                            address,
                            deficit,
                            nonce,
                            tx_hash,
                            attempt,
                            fund_account,
                            verify_after_existing,
                        )) => {
                            trace!(
                                to = %address,
                                nonce,
                                attempt,
                                tx_hash = ?tx_hash,
                                fund_account,
                                "replacement funder transaction accepted"
                            );
                            if fund_account && verify_after_existing {
                                consumed_funding_nonces.push((address, deficit, nonce));
                            }
                            if verify_after_existing {
                                existing_nonce_targets.push(nonce.saturating_add(1));
                            }
                        }
                        Err(error) => {
                            fatal_errors.push(error);
                        }
                    }
                }

                if !fatal_errors.is_empty() {
                    pb_fund.finish_and_clear();
                    return Err(BaselineError::Transaction(format!(
                        "{} replacement funding tx(s) failed: {}",
                        fatal_errors.len(),
                        fatal_errors.join("; "),
                    )));
                }

                // Do not wait for an intended recipient whose nonce was consumed by a different
                // stale transaction. It is checked and, if needed, requeued below.
                batch_pending.extend(replacement_addresses.into_iter().filter(|address| {
                    !consumed_funding_nonces.iter().any(|(consumed, _, _)| consumed == address)
                }));
            }

            // Balance polling cannot confirm zero-value cancellation transactions. Wait until the
            // canonical nonce has crossed every stale nonce reclaimed by this batch before moving
            // on, ensuring stale funding traffic cannot leak into setup calibration or the run.
            let settlement_target =
                reclaimed_nonce_target.into_iter().chain(existing_nonce_targets).max();
            if let Some(target_nonce) = settlement_target {
                let started = Instant::now();
                loop {
                    let observed_nonce = funder_provider
                        .get_transaction_count(funder_address)
                        .await
                        .rpc("get canonical transaction count")?;
                    if observed_nonce >= target_nonce {
                        break;
                    }
                    if started.elapsed() >= PENDING_CONFIRMATION_TIMEOUT {
                        pb_fund.finish_and_clear();
                        return Err(BaselineError::Timeout {
                            operation: format!(
                                "reclaiming stale funder nonces through {} (canonical nonce {})",
                                target_nonce.saturating_sub(1),
                                observed_nonce
                            ),
                            duration: PENDING_CONFIRMATION_TIMEOUT,
                        });
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }

            for (address, _, _) in consumed_funding_nonces {
                // Read through the same provider whose canonical nonce was observed above so a
                // lagging query endpoint cannot cause a duplicate transfer.
                let balance = funder_provider.get_balance(address).await.rpc("get balance")?;
                if balance >= amount_per_account {
                    pb_fund.inc(1);
                    continue;
                }

                let deficit = amount_per_account.saturating_sub(balance);
                let remaining_deficit = txs_remaining
                    .iter()
                    .map(|(_, value, _, _)| *value)
                    .fold(deficit, U256::saturating_add);
                let remaining_count = txs_remaining.len().saturating_add(1);
                let remaining_gas = gas_cost_per_tx.saturating_mul(U256::from(remaining_count));
                let remaining_needed = remaining_deficit.saturating_add(remaining_gas);
                let current_funder_balance =
                    funder_provider.get_balance(funder_address).await.rpc("get funder balance")?;
                if current_funder_balance < remaining_needed {
                    pb_fund.finish_and_clear();
                    return Err(BaselineError::Transaction(format!(
                        "funder {} has insufficient balance after stale nonce settlement: has {} ETH, needs {} ETH for remaining funding",
                        funder_address,
                        format_ether(current_funder_balance),
                        format_ether(remaining_needed),
                    )));
                }
                warn!(
                    to = %address,
                    balance = %balance,
                    nonce = next_retry_nonce,
                    "stale transaction consumed nonce without funding intended account; requeuing transfer"
                );
                txs_remaining.push_back((address, deficit, next_retry_nonce, true));
                next_retry_nonce = next_retry_nonce.checked_add(1).expect("nonce overflow");
            }

            Self::await_balances(&client, &mut batch_pending, amount_per_account, &pb_fund).await?;
        }
        pb_fund.finish_and_clear();

        // Phase 5: Parallel post-funding state refresh.
        let pb_refresh = self.progress_bar(total_accounts as u64, "Refreshing account state");
        let refresh_futs: Vec<_> = self
            .accounts
            .accounts()
            .iter()
            .map(|a| {
                let client = client.clone();
                let addr = a.address;
                async move {
                    let balance = client.get_balance(addr).await.rpc("get balance")?;
                    let nonce =
                        client.get_transaction_count(addr).await.rpc("get transaction count")?;
                    Ok::<_, BaselineError>((addr, balance, nonce))
                }
            })
            .collect();

        let refresh_results: Vec<_> = stream::iter(refresh_futs)
            .buffer_unordered(FUNDING_CONCURRENCY)
            .inspect(|_| pb_refresh.inc(1))
            .collect()
            .await;
        pb_refresh.finish_and_clear();

        let addr_to_idx: HashMap<Address, usize> =
            self.accounts.accounts().iter().enumerate().map(|(i, a)| (a.address, i)).collect();

        let refresh_provider = RootProvider::<Ethereum>::new_http(self.config.query_rpc.clone());

        for result in refresh_results {
            let (addr, balance, account_nonce) = result?;
            let idx = addr_to_idx[&addr];
            let account = &mut self.accounts.accounts_mut()[idx];
            account.balance = balance;
            account.nonce = account_nonce;

            let nonce_manager =
                NonceManager::new(refresh_provider.clone(), addr, NONCE_RPC_TIMEOUT)
                    .with_pending_tag();
            Arc::make_mut(&mut self.nonce_managers).insert(addr, nonce_manager);

            trace!(address = %addr, balance = %balance, nonce = account_nonce, "account state refreshed");
        }

        info!(funded = accounts_to_fund.len(), "funding complete");
        Ok(())
    }

    /// Collects unique token addresses from configured swap transaction types.
    pub fn collect_swap_tokens(&self) -> Vec<Address> {
        let mut tokens = HashSet::new();
        for tx_config in &self.config.transactions {
            match &tx_config.tx_type {
                TxType::UniswapV3 { token_in, token_out, .. }
                | TxType::AerodromeCl { token_in, token_out, .. } => {
                    tokens.insert(*token_in);
                    tokens.insert(*token_out);
                }
                TxType::Transfer
                | TxType::Calldata { .. }
                | TxType::Erc20 { .. }
                | TxType::Storage { .. }
                | TxType::B20
                | TxType::Precompile { .. }
                | TxType::Osaka { .. } => {}
            }
        }
        tokens.into_iter().collect()
    }

    /// Collects unique router addresses from configured swap transaction types.
    pub fn collect_swap_routers(&self) -> Vec<Address> {
        let mut routers = HashSet::new();
        for tx_config in &self.config.transactions {
            match &tx_config.tx_type {
                TxType::UniswapV3 { router, .. } | TxType::AerodromeCl { router, .. } => {
                    routers.insert(*router);
                }
                TxType::Transfer
                | TxType::Calldata { .. }
                | TxType::Erc20 { .. }
                | TxType::Storage { .. }
                | TxType::B20
                | TxType::Precompile { .. }
                | TxType::Osaka { .. } => {}
            }
        }
        routers.into_iter().collect()
    }

    /// Clears pending transactions from all configured txpool nodes for every test sender.
    #[instrument(skip(self), fields(nodes = self.config.txpool_nodes.len(), accounts = self.accounts.len()))]
    pub async fn clear_txpools(&self) -> Result<u64> {
        if self.config.txpool_nodes.is_empty() {
            return Ok(0);
        }

        info!(
            nodes = self.config.txpool_nodes.len(),
            accounts = self.accounts.len(),
            "clearing txpool sender transactions"
        );

        let clients: Vec<_> = self
            .config
            .txpool_nodes
            .iter()
            .cloned()
            .map(|node| {
                let client = TxpoolAdminClient::new(node.clone())?;
                Ok::<_, BaselineError>((node, client))
            })
            .collect::<Result<_>>()?;
        let addresses: Vec<_> =
            self.accounts.accounts().iter().map(|account| account.address).collect();
        let requests: Vec<_> = clients
            .iter()
            .flat_map(|(node, client)| {
                addresses
                    .iter()
                    .copied()
                    .map(move |address| (node.clone(), client.clone(), address))
            })
            .collect();

        let clear_results: Vec<_> =
            stream::iter(requests.into_iter().map(|(node, client, address)| async move {
                let removed = client.drop_sender_transactions(address).await.map_err(|e| {
                    BaselineError::Rpc(format!(
                        "failed to clear txpool node {node} for sender {address}: {e}"
                    ))
                })?;
                Ok::<_, BaselineError>((node, removed.len() as u64))
            }))
            .buffer_unordered(TXPOOL_CLEAR_CONCURRENCY)
            .collect()
            .await;

        let mut removed_by_node: HashMap<url::Url, u64> = HashMap::new();
        for result in clear_results {
            let (node, removed) = result?;
            removed_by_node
                .entry(node)
                .and_modify(|total| *total = total.saturating_add(removed))
                .or_insert(removed);
        }

        let mut removed_total = 0u64;
        for node in &self.config.txpool_nodes {
            let removed_for_node = removed_by_node.get(node).copied().unwrap_or(0);
            removed_total = removed_total.saturating_add(removed_for_node);
            info!(
                node = %node,
                removed = removed_for_node,
                "cleared txpool sender transactions from node"
            );
        }

        info!(removed = removed_total, "txpool clearing complete");
        Ok(removed_total)
    }

    /// Mints swap tokens to all sender accounts.
    ///
    /// Scans the configured transaction types for token addresses, then mints
    /// `amount_per_token` of each token to every sender that has insufficient balance.
    /// Skips accounts that already have enough tokens. Requires tokens that expose
    /// a public `mint(address,uint256)` function (e.g., `FreeTransferERC20`).
    #[instrument(skip(self, funding_key), fields(accounts = self.accounts.len()))]
    pub async fn setup_swap_tokens(
        &self,
        funding_key: PrivateKeySigner,
        amount_per_token: U256,
    ) -> Result<()> {
        let tokens = self.collect_swap_tokens();
        if tokens.is_empty() {
            debug!("no swap tokens configured, skipping token setup");
            return Ok(());
        }

        let sender_addresses: Vec<Address> =
            self.accounts.accounts().iter().map(|a| a.address).collect();
        let token_count = tokens.len();
        let total_pairs = token_count * sender_addresses.len();

        // Phase 1: Check existing token balances for all (token, sender) pairs.
        let pb_check = self.progress_bar(total_pairs as u64, "Checking token balances");
        let client = &self.client;

        let balance_futs: Vec<_> = tokens
            .iter()
            .flat_map(|&token| {
                sender_addresses.iter().map(move |&sender| {
                    let client = client.clone();
                    let call_data = encode_erc20_balance_of(sender);
                    async move {
                        let result = client
                            .call(
                                TransactionRequest::default()
                                    .with_to(token)
                                    .with_input(call_data)
                                    .into(),
                            )
                            .await
                            .rpc("eth_call")
                            .map(|bytes| U256::from_be_slice(bytes.as_ref()))
                            .unwrap_or(U256::ZERO);
                        (token, sender, result)
                    }
                })
            })
            .collect();

        let balance_results: Vec<_> = stream::iter(balance_futs)
            .buffer_unordered(FUNDING_CONCURRENCY)
            .inspect(|_| pb_check.inc(1))
            .collect()
            .await;
        pb_check.finish_and_clear();

        // Filter to only (token, sender) pairs that need funding.
        let mut transfers_needed: Vec<(Address, Address)> = Vec::new();
        let mut already_funded = 0usize;
        for (token, sender, balance) in balance_results {
            if balance < amount_per_token {
                transfers_needed.push((token, sender));
            } else {
                already_funded += 1;
                trace!(token = %token, sender = %sender, balance = %balance, "account already has sufficient tokens");
            }
        }

        if transfers_needed.is_empty() {
            info!(
                tokens = token_count,
                accounts = sender_addresses.len(),
                "all accounts already have sufficient token balances, skipping distribution"
            );
            return Ok(());
        }

        info!(
            transfers_needed = transfers_needed.len(),
            already_funded = already_funded,
            tokens = token_count,
            accounts = sender_addresses.len(),
            "distributing swap tokens"
        );

        // Phase 2: Setup for transfers.
        let funder_address = funding_key.address();
        let wallet = EthereumWallet::from(funding_key);
        let funder_provider =
            Arc::new(create_wallet_provider(self.config.primary_submission_rpc().clone(), wallet));
        let chain_id = self.config.chain_id;
        let pricer = GasPricer::new(self.config.max_gas_price);

        let base_fee = self.client.get_base_fee().await?;
        let fees = pricer.funding_fees_for(base_fee);

        // Pre-flight balance check — abort before sending any TXs if the funder
        // cannot cover the total gas cost for needed token transfers.
        let gas_cost_per_tx = U256::from(65_000u64).saturating_mul(U256::from(fees.max_fee));
        let total_gas_cost = gas_cost_per_tx.saturating_mul(U256::from(transfers_needed.len()));
        let funder_balance = self.client.get_balance(funder_address).await.rpc("get balance")?;

        if funder_balance < total_gas_cost {
            let shortfall = total_gas_cost.saturating_sub(funder_balance);
            return Err(BaselineError::Transaction(format!(
                "funder {} has insufficient balance for token distribution: has {} ETH, needs {} ETH (gas for {} txs), shortfall {} ETH",
                funder_address,
                format_ether(funder_balance),
                format_ether(total_gas_cost),
                transfers_needed.len(),
                format_ether(shortfall),
            )));
        }

        let mut nonce = funder_provider
            .get_transaction_count(funder_address)
            .pending()
            .await
            .rpc("get pending transaction count")?;

        // Phase 3: Execute transfers for accounts that need tokens.
        let pb = self.progress_bar(transfers_needed.len() as u64, "Minting tokens");
        let mut failed_count: usize = 0;

        let txs: Vec<(TransactionRequest, Address, Address)> = transfers_needed
            .into_iter()
            .map(|(token, sender)| {
                let mint_data = Self::encode_erc20_mint(sender, amount_per_token);
                let tx = TransactionRequest::default()
                    .with_to(token)
                    .with_input(mint_data)
                    .with_nonce(nonce)
                    .with_chain_id(chain_id)
                    .with_gas_limit(65_000)
                    .with_max_fee_per_gas(fees.max_fee)
                    .with_max_priority_fee_per_gas(fees.priority_fee);
                nonce += 1;
                (tx, token, sender)
            })
            .collect();

        let total_txs = txs.len();
        let mut txs_remaining = txs.into_iter().peekable();
        while txs_remaining.peek().is_some() {
            let batch: Vec<_> =
                txs_remaining.by_ref().take(self.config.max_in_flight_per_sender).collect();
            let mut pending_txs: Vec<(Address, Address)> = Vec::new();

            let send_futs = batch.into_iter().map(|(tx, token, sender)| {
                let provider = Arc::clone(&funder_provider);
                async move {
                    let result = provider.send_transaction(tx).await;
                    (result, token, sender)
                }
            });

            let mut send_stream =
                stream::iter(send_futs).buffer_unordered(self.config.max_in_flight_per_sender);

            while let Some((result, token, sender)) = send_stream.next().await {
                match result {
                    Ok(pending) => {
                        let tx_hash = *pending.tx_hash();
                        trace!(token = %token, to = %sender, tx_hash = %tx_hash, "token mint sent");
                        pending_txs.push((token, sender));
                    }
                    Err(e) => {
                        warn!(token = %token, to = %sender, error = %e, "token mint failed");
                        failed_count += 1;
                    }
                }
            }

            await_token_balances(&self.client, &mut pending_txs, amount_per_token, &pb).await?;
        }

        pb.finish_and_clear();

        if failed_count > 0 {
            return Err(BaselineError::Transaction(format!(
                "{failed_count}/{total_txs} token mints failed — senders with missing tokens will revert on swap"
            )));
        }

        info!(
            tokens = token_count,
            transfers = total_txs,
            skipped = already_funded,
            "swap token setup complete"
        );
        Ok(())
    }

    fn encode_erc20_mint(to: Address, amount: U256) -> Bytes {
        sol! {
            function mint(address to, uint256 amount) external;
        }
        Bytes::from(mintCall { to, amount }.abi_encode())
    }

    /// Drains all test account balances back to the funder address.
    ///
    /// Each account sends its entire balance minus gas costs back to the funder.
    /// Transactions that fail (e.g. zero balance) are skipped with a warning.
    pub async fn drain_accounts(&self, funding_key: PrivateKeySigner) -> Result<U256> {
        let funder_address = funding_key.address();
        let client = self.client.clone();
        let primary_submission_rpc = self.config.primary_submission_rpc().clone();
        let chain_id = self.config.chain_id;

        let base_fee = client.get_base_fee().await?;
        let fees = GasPricer::new(self.config.max_gas_price).funding_fees_for(base_fee);
        let drain_gas_limit = 21_000u128;
        // L1 data fee on Base is typically ~0.0001 ETH for a simple transfer; keep a modest
        // buffer so post-load dust can still be swept instead of skipping every account.
        let l1_fee_buffer = 100_000_000_000_000u128; // 0.0001 ETH
        let drain_gas_cost = U256::from(drain_gas_limit * fees.max_fee + l1_fee_buffer);

        let total_accounts = self.accounts.len();
        let pb_drain = self.progress_bar(total_accounts as u64, "Draining accounts");

        // Each account has its own signer, so drains are fully independent.
        let account_data: Vec<_> =
            self.accounts.accounts().iter().map(|a| (a.address, a.signer.clone())).collect();

        let drain_futs: Vec<_> = account_data
            .into_iter()
            .map(|(address, signer)| {
                let client = client.clone();
                let primary_submission_rpc = primary_submission_rpc.clone();
                async move {
                    let balance = client
                        .get_balance(address)
                        .block_id(BlockNumberOrTag::Pending.into())
                        .await
                        .rpc("get pending balance")?;
                    if balance <= drain_gas_cost {
                        trace!(
                            address = %address,
                            balance = %balance,
                            "skipping drain, balance too low to cover gas"
                        );
                        return Ok::<_, BaselineError>(None);
                    }

                    let send_amount = balance.saturating_sub(drain_gas_cost);
                    let wallet = EthereumWallet::from(signer);
                    let provider = create_wallet_provider(primary_submission_rpc, wallet);
                    let nonce = provider
                        .get_transaction_count(address)
                        .pending()
                        .await
                        .rpc("get pending transaction count")?;

                    let tx = TransactionRequest::default()
                        .with_to(funder_address)
                        .with_value(send_amount)
                        .with_nonce(nonce)
                        .with_chain_id(chain_id)
                        .with_gas_limit(drain_gas_limit as u64)
                        .with_max_fee_per_gas(fees.max_fee)
                        .with_max_priority_fee_per_gas(fees.priority_fee);

                    match provider.send_transaction(tx).await {
                        Ok(pending) => {
                            let tx_hash = *pending.tx_hash();
                            trace!(
                                from = %address,
                                amount = %send_amount,
                                tx_hash = %tx_hash,
                                "drain tx sent"
                            );
                            Ok(Some((address, send_amount)))
                        }
                        Err(e) => {
                            warn!(from = %address, error = %e, "drain tx failed, skipping");
                            Ok(None)
                        }
                    }
                }
            })
            .collect();

        let drain_results: Vec<_> = stream::iter(drain_futs)
            .buffer_unordered(FUNDING_CONCURRENCY)
            .inspect(|_| pb_drain.inc(1))
            .collect()
            .await;
        pb_drain.finish_and_clear();

        let mut pending_txs = Vec::new();
        let mut total_drained = U256::ZERO;
        for result in drain_results {
            if let Some((address, amount)) = result? {
                pending_txs.push(address);
                total_drained = total_drained.saturating_add(amount);
            }
        }

        if pending_txs.is_empty() {
            info!("no accounts to drain");
            return Ok(U256::ZERO);
        }

        let pb_confirm = self.progress_bar(pending_txs.len() as u64, "Waiting for drained funds");
        info!(count = pending_txs.len(), total = %total_drained, "waiting for drained balances");

        if let Err(e) =
            Self::await_drained_balances(&client, &mut pending_txs, drain_gas_cost, &pb_confirm)
                .await
        {
            warn!(error = %e, "some drain balances did not settle within timeout");
        }
        pb_confirm.finish_and_clear();

        info!(total = %total_drained, "drain complete");
        Ok(total_drained)
    }

    pub(super) fn progress_bar(&self, total: u64, prefix: &str) -> ProgressBar {
        if let Some(display) = &self.display {
            return display.progress_bar(total, prefix);
        }
        if self.snapshot_tx.is_some() {
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

    /// Waits for account balances to reach a target after funding transfers.
    async fn await_balances(
        client: &QueryProvider,
        pending_accounts: &mut Vec<Address>,
        target_balance: U256,
        pb: &ProgressBar,
    ) -> Result<usize> {
        let timeout = Duration::from_secs(60);
        let poll_interval = Duration::from_millis(500);
        let start = Instant::now();

        let mut settled = 0usize;

        while !pending_accounts.is_empty() && start.elapsed() < timeout {
            tokio::time::sleep(poll_interval).await;

            let mut still_pending = Vec::new();
            for address in pending_accounts.drain(..) {
                match client.get_balance(address).await.rpc("get balance") {
                    Ok(balance) if balance >= target_balance => {
                        trace!(address = %address, balance = %balance, "funding balance settled");
                        settled += 1;
                        pb.inc(1);
                    }
                    Ok(_) => {
                        still_pending.push(address);
                    }
                    Err(e) => {
                        debug!(address = %address, error = %e, "failed to check funding balance");
                        still_pending.push(address);
                    }
                }
            }
            *pending_accounts = still_pending;
        }

        if !pending_accounts.is_empty() {
            let sample: Vec<_> = pending_accounts.iter().take(3).copied().collect();
            return Err(BaselineError::Transaction(format!(
                "{} accounts did not reach funding target within timeout; sample: {sample:?}",
                pending_accounts.len(),
            )));
        }

        Ok(settled)
    }

    pub(super) async fn refresh_sender_state(&mut self) -> Result<()> {
        let total_accounts = self.accounts.len();
        let client = self.client.clone();
        let pb_refresh = self.progress_bar(total_accounts as u64, "Refreshing account state");

        let refresh_futs: Vec<_> = self
            .accounts
            .accounts()
            .iter()
            .map(|a| {
                let client = client.clone();
                let addr = a.address;
                async move {
                    let balance = client.get_balance(addr).await.rpc("get balance")?;
                    let nonce =
                        client.get_transaction_count(addr).await.rpc("get transaction count")?;
                    Ok::<_, BaselineError>((addr, balance, nonce))
                }
            })
            .collect();

        let refresh_results: Vec<_> = stream::iter(refresh_futs)
            .buffer_unordered(FUNDING_CONCURRENCY)
            .inspect(|_| pb_refresh.inc(1))
            .collect()
            .await;
        pb_refresh.finish_and_clear();

        let addr_to_idx: HashMap<Address, usize> =
            self.accounts.accounts().iter().enumerate().map(|(i, a)| (a.address, i)).collect();

        let refresh_provider = RootProvider::<Ethereum>::new_http(self.config.query_rpc.clone());

        for result in refresh_results {
            let (addr, balance, account_nonce) = result?;
            let idx = addr_to_idx[&addr];
            let account = &mut self.accounts.accounts_mut()[idx];
            account.balance = balance;
            account.nonce = account_nonce;

            let nonce_manager =
                NonceManager::new(refresh_provider.clone(), addr, NONCE_RPC_TIMEOUT)
                    .with_pending_tag();
            Arc::make_mut(&mut self.nonce_managers).insert(addr, nonce_manager);

            trace!(address = %addr, balance = %balance, nonce = account_nonce, "account state refreshed");
        }

        Ok(())
    }

    /// Waits for source account balances to drop to the post-drain dust threshold.
    async fn await_drained_balances(
        client: &QueryProvider,
        pending_accounts: &mut Vec<Address>,
        max_remaining: U256,
        pb: &ProgressBar,
    ) -> Result<usize> {
        let timeout = Duration::from_secs(60);
        let poll_interval = Duration::from_millis(500);
        let start = Instant::now();
        let mut settled = 0usize;

        while !pending_accounts.is_empty() && start.elapsed() < timeout {
            tokio::time::sleep(poll_interval).await;

            let mut still_pending = Vec::new();
            for address in pending_accounts.drain(..) {
                match client.get_balance(address).await.rpc("get balance") {
                    Ok(balance) if balance <= max_remaining => {
                        trace!(address = %address, balance = %balance, "drain balance settled");
                        settled += 1;
                        pb.inc(1);
                    }
                    Ok(_) => {
                        still_pending.push(address);
                    }
                    Err(e) => {
                        debug!(address = %address, error = %e, "failed to check drain balance");
                        still_pending.push(address);
                    }
                }
            }
            *pending_accounts = still_pending;
        }

        if !pending_accounts.is_empty() {
            let sample: Vec<_> = pending_accounts.iter().take(3).copied().collect();
            return Err(BaselineError::Transaction(format!(
                "{} accounts did not drain within timeout; sample: {sample:?}",
                pending_accounts.len(),
            )));
        }

        Ok(settled)
    }
}
