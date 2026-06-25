//! B-20 precompile token lifecycle for load tests: per-sender token creation with self-mint
//! during setup, and per-sender burning during teardown.
//!
//! Each sender creates and owns its own B-20 ASSET token (mirroring the parallel uniswap/aerodrome
//! setup), so setup is fully parallel with each sender on its own nonce stream — there is no funder
//! bottleneck and no admin-gated role grants routed through a single account.

use std::time::Duration;

use alloy_network::{Ethereum, EthereumWallet, TransactionBuilder};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{PendingTransactionBuilder, Provider};
use alloy_rpc_types::TransactionRequest;
use alloy_sol_types::{SolCall, SolValue};
use base_common_precompiles::{B20FactoryStorage, B20TokenRole, B20Variant, IB20, IB20Factory};
use futures::{StreamExt, stream};
use tracing::{debug, info, warn};

use super::{LoadRunner, SubmissionPipeline, TxType, load_runner::FUNDING_CONCURRENCY};
use crate::{
    BaselineError, Result,
    rpc::{BaseFeeExt, RpcResultExt, WalletProvider, create_wallet_provider},
    workload::{b20_salt_for, b20_token_for},
};

/// Gas limit for a `createB20` tx (token creation plus the self-grant and self-mint init calls).
const B20_CREATE_GAS_LIMIT: u64 = 10_000_000;
/// Gas limit for a single `burn` tx during teardown.
const B20_BURN_GAS_LIMIT: u64 = 200_000;
/// Timeout for awaiting a setup/teardown tx receipt before treating it as failed.
const B20_RECEIPT_TIMEOUT: Duration = Duration::from_secs(120);
/// Fee multiplier applied when replacing a stuck pending tx (matches the funding-path bump).
const REPLACEMENT_FEE_MULTIPLIER: u128 = 3;

impl LoadRunner {
    /// Returns `true` if any configured transaction type is [`TxType::B20`].
    pub fn needs_b20_setup(&self) -> bool {
        self.config.transactions.iter().any(|t| matches!(t.tx_type, TxType::B20))
    }

    /// Creates a per-sender B-20 ASSET token, with each sender minting its own supply.
    ///
    /// Every sender sends ONE `createB20` tx from its own account, in parallel. The token's
    /// `initialAdmin` is the sender itself, and the create's privileged `initCalls` grant the
    /// sender `BURN_ROLE` (so teardown can burn) and mint `amount_per_sender` to the sender.
    /// Because each sender uses its own nonce stream, a stuck pending tx from a prior run at the
    /// same nonce is replaced by a fee-bumped resend (see [`Self::submit_b20_create`]).
    ///
    /// A fresh per-run salt is generated and stored on the runner so the derived token addresses
    /// differ from previous runs (avoiding `TokenAlreadyExists`) and so the load payload and
    /// teardown can recompute the same addresses.
    pub async fn setup_b20_tokens(&mut self, amount_per_sender: U256) -> Result<()> {
        let chain_id = self.config.chain_id;
        let max_gas_price = self.config.max_gas_price;
        let base_fee = self.client.get_base_fee().await?;
        let max_priority_fee = (base_fee / 10).max(1);
        let max_fee =
            SubmissionPipeline::submission_max_fee(base_fee, max_priority_fee, max_gas_price);
        let replacement_max_fee = max_fee.saturating_mul(REPLACEMENT_FEE_MULTIPLIER);
        let replacement_priority_fee = max_priority_fee.saturating_mul(REPLACEMENT_FEE_MULTIPLIER);

        // A fresh random salt per run keeps each run's token addresses distinct, so re-running the
        // same sender set never collides with the previous run's tokens.
        let run_salt = B256::from(rand::random::<[u8; 32]>());
        self.b20_run_salt = Some(run_salt);

        let account_data: Vec<(Address, _)> =
            self.accounts.accounts().iter().map(|a| (a.address, a.signer.clone())).collect();
        let total = account_data.len();
        let rpc_url = self.config.primary_submission_rpc().clone();
        let params_version = B20Variant::Asset.supported_version();

        info!(senders = total, amount = %amount_per_sender, "creating per-sender B-20 tokens");

        // Phase 1: submit every sender's create tx in parallel. Submission is fast (one RPC per
        // sender) and does not block on confirmation, so all creates land in the mempool quickly
        // instead of the stream stalling behind a slow receipt at the buffer head.
        let pb_submit = self.progress_bar(total as u64, "Submitting per-sender B-20 creates");
        let submit_futs = account_data.into_iter().map(|(sender, signer)| {
            let rpc_url = rpc_url.clone();
            let pb_submit = pb_submit.clone();
            async move {
                let wallet = EthereumWallet::from(signer);
                let provider = create_wallet_provider(rpc_url, wallet);

                // Build the create tx for this sender's own token. The init calls run with factory
                // privilege, so the grant and mint bypass the normal role checks: the sender ends
                // up holding BURN_ROLE and `amount_per_sender` of its own supply.
                let salt = b20_salt_for(sender, run_salt);
                let create_params = IB20Factory::B20AssetCreateParams {
                    version: params_version,
                    name: "Load Test B20".to_string(),
                    symbol: "LTB20".to_string(),
                    initialAdmin: sender,
                    decimals: 6,
                };
                let grant_burn =
                    IB20::grantRoleCall { role: B20TokenRole::Burn.id(), account: sender };
                let mint = IB20::mintCall { to: sender, amount: amount_per_sender };
                let create_call = IB20Factory::createB20Call {
                    variant: IB20Factory::B20Variant::ASSET,
                    salt,
                    params: create_params.abi_encode().into(),
                    initCalls: vec![
                        Bytes::from(grant_burn.abi_encode()),
                        Bytes::from(mint.abi_encode()),
                    ],
                };
                let input = Bytes::from(create_call.abi_encode());

                let result = Self::submit_b20_create(
                    provider,
                    sender,
                    input,
                    chain_id,
                    B20_CREATE_GAS_LIMIT,
                    max_fee,
                    max_priority_fee,
                    replacement_max_fee,
                    replacement_priority_fee,
                )
                .await;
                pb_submit.inc(1);
                (sender, result)
            }
        });

        let submit_results: Vec<_> =
            stream::iter(submit_futs).buffer_unordered(FUNDING_CONCURRENCY).collect().await;
        pb_submit.finish_and_clear();

        // Phase 2: collect all create receipts in parallel. Confirmation latency now overlaps
        // across senders instead of serializing at the submit stream's head.
        let pb_confirm = self.progress_bar(total as u64, "Confirming per-sender B-20 creates");
        let mut failed = 0usize;
        let mut first_error: Option<String> = None;
        let mut receipt_futs = Vec::with_capacity(submit_results.len());
        for (sender, result) in submit_results {
            match result {
                Ok(pending) => receipt_futs.push(async move {
                    (sender, pending.with_timeout(Some(B20_RECEIPT_TIMEOUT)).get_receipt().await)
                }),
                Err(e) => {
                    warn!(sender = %sender, error = %e, "B-20 create submission failed");
                    failed += 1;
                    if first_error.is_none() {
                        first_error = Some(e.to_string());
                    }
                    pb_confirm.inc(1);
                }
            }
        }

        let mut receipt_stream = stream::iter(receipt_futs).buffer_unordered(FUNDING_CONCURRENCY);
        while let Some((sender, result)) = receipt_stream.next().await {
            match result {
                Ok(receipt) if receipt.status() => {
                    debug!(sender = %sender, tx_hash = %receipt.transaction_hash, "B-20 token created");
                }
                Ok(receipt) => {
                    warn!(sender = %sender, tx_hash = %receipt.transaction_hash, "B-20 create reverted");
                    failed += 1;
                    if first_error.is_none() {
                        first_error =
                            Some(format!("createB20 reverted (tx {})", receipt.transaction_hash));
                    }
                }
                Err(e) => {
                    warn!(sender = %sender, error = %e, "B-20 create receipt failed");
                    failed += 1;
                    if first_error.is_none() {
                        first_error = Some(format!("receipt failed: {e}"));
                    }
                }
            }
            pb_confirm.inc(1);
        }
        pb_confirm.finish_and_clear();

        if failed > 0 {
            let detail = first_error.unwrap_or_else(|| "unknown error".to_string());
            return Err(BaselineError::Transaction(format!(
                "{failed}/{total} per-sender B-20 token setups failed (first: {detail}). \
                 Likely cause: B-20 ASSET feature not activated on this chain, \
                 a factory validation error, or clear the txpool and try again."
            )));
        }

        // Rebuild the generator now that the per-run salt is known, so the B-20 payload derives the
        // correct per-sender token addresses.
        self.generator =
            Self::create_generator(self.workload_config(), &self.config, Some(run_salt))?;

        info!(senders = total, amount = %amount_per_sender, "B-20 token setup complete");
        Ok(())
    }

    /// Submits one `createB20` tx for a sender, replacing a stuck pending tx if necessary.
    ///
    /// Returns the pending tx so the caller can await its receipt in a separate phase, keeping
    /// submission decoupled from confirmation. Mirrors the funding path's replacement handling: a
    /// `replacement transaction underpriced` or `already known` rejection (a leftover pending tx
    /// from a prior run at the same nonce) is retried at the same nonce with bumped fees, and a
    /// `nonce too low` rejection refetches the pending nonce.
    #[allow(clippy::too_many_arguments)]
    async fn submit_b20_create(
        provider: WalletProvider,
        sender: Address,
        input: Bytes,
        chain_id: u64,
        gas_limit: u64,
        max_fee: u128,
        max_priority_fee: u128,
        replacement_max_fee: u128,
        replacement_priority_fee: u128,
    ) -> Result<PendingTransactionBuilder<Ethereum>> {
        let nonce = provider
            .get_transaction_count(sender)
            .pending()
            .await
            .rpc("get pending transaction count")?;

        let build = |nonce: u64, max_fee: u128, priority_fee: u128| {
            TransactionRequest::default()
                .with_to(B20FactoryStorage::ADDRESS)
                .with_input(input.clone())
                .with_nonce(nonce)
                .with_chain_id(chain_id)
                .with_gas_limit(gas_limit)
                .with_max_fee_per_gas(max_fee)
                .with_max_priority_fee_per_gas(priority_fee)
        };

        match provider.send_transaction(build(nonce, max_fee, max_priority_fee)).await {
            Ok(pending) => Ok(pending),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("replacement transaction underpriced")
                    || msg.contains("already known")
                {
                    // A prior run left a pending tx at this nonce; replace it at a higher fee.
                    provider
                        .send_transaction(build(
                            nonce,
                            replacement_max_fee,
                            replacement_priority_fee,
                        ))
                        .await
                        .map_err(|e| {
                            BaselineError::Transaction(format!("replacement send failed: {e}"))
                        })
                } else if msg.contains("nonce too low") {
                    // The pending nonce was stale; refetch and resend.
                    let fresh = provider
                        .get_transaction_count(sender)
                        .pending()
                        .await
                        .rpc("refetch pending transaction count")?;
                    provider
                        .send_transaction(build(fresh, max_fee, max_priority_fee))
                        .await
                        .map_err(|e| {
                            BaselineError::Transaction(format!("nonce-refreshed send failed: {e}"))
                        })
                } else {
                    Err(BaselineError::Transaction(format!("send failed: {e}")))
                }
            }
        }
    }

    /// Burns each sender's remaining balance of its own B-20 token.
    ///
    /// No-op when no per-run salt is recorded (setup never ran). Each sender holds `BURN_ROLE` on
    /// its own token (granted at creation), so the burn passes the role check. Each sender signs
    /// its own single tx, so nonce streams are independent and a failed send strands nothing.
    pub async fn teardown_b20_tokens(&self) -> Result<()> {
        let Some(run_salt) = self.b20_run_salt else {
            return Ok(());
        };

        let chain_id = self.config.chain_id;
        let max_gas_price = self.config.max_gas_price;
        let base_fee = self.client.get_base_fee().await?;
        let max_priority_fee = (base_fee / 10).max(1);
        let max_fee =
            SubmissionPipeline::submission_max_fee(base_fee, max_priority_fee, max_gas_price);

        let sender_addresses: Vec<Address> =
            self.accounts.accounts().iter().map(|a| a.address).collect();
        let signers = Self::build_signers(&self.accounts);
        let client = &self.client;
        let rpc_url = self.config.primary_submission_rpc().clone();

        // Phase 1: Query each sender's balance of its own token in parallel.
        let balance_futs: Vec<_> = sender_addresses
            .iter()
            .map(|&sender| {
                let client = client.clone();
                let token = b20_token_for(sender, run_salt);
                let call_data = Self::encode_erc20_balance_of(sender);
                async move {
                    let balance = client
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
                    (sender, token, balance)
                }
            })
            .collect();

        let balances: Vec<_> =
            stream::iter(balance_futs).buffer_unordered(FUNDING_CONCURRENCY).collect().await;

        let senders_with_balance: Vec<_> =
            balances.into_iter().filter(|(_, _, balance)| !balance.is_zero()).collect();

        if senders_with_balance.is_empty() {
            info!("all B-20 balances are zero, skipping teardown");
            return Ok(());
        }

        // Phase 2: Burn each sender's balance of its own token.
        let pb = self.progress_bar(senders_with_balance.len() as u64, "Burning B-20 tokens");
        let mut burn_failed = 0usize;
        let mut burn_count = 0usize;

        let submit_futs: Vec<_> = senders_with_balance
            .into_iter()
            .filter_map(|(sender, token, balance)| {
                let signer = signers.get(&sender)?.clone();
                let wallet = EthereumWallet::from(signer);
                let provider = create_wallet_provider(rpc_url.clone(), wallet);
                Some(async move {
                    let sender_nonce = match provider.get_transaction_count(sender).pending().await
                    {
                        Ok(n) => n,
                        Err(e) => {
                            return Err((sender, eyre::eyre!("nonce fetch failed: {e}")));
                        }
                    };

                    let burn_call = IB20::burnCall { amount: balance };
                    let tx = TransactionRequest::default()
                        .with_to(token)
                        .with_input(Bytes::from(burn_call.abi_encode()))
                        .with_nonce(sender_nonce)
                        .with_chain_id(chain_id)
                        .with_gas_limit(B20_BURN_GAS_LIMIT)
                        .with_max_fee_per_gas(max_fee)
                        .with_max_priority_fee_per_gas(max_priority_fee);

                    match provider.send_transaction(tx).await {
                        Ok(pending) => Ok((sender, balance, pending)),
                        Err(e) => Err((sender, eyre::eyre!("send failed: {e}"))),
                    }
                })
            })
            .collect();

        let submit_results: Vec<_> =
            stream::iter(submit_futs).buffer_unordered(FUNDING_CONCURRENCY).collect().await;

        let mut receipt_futs = Vec::with_capacity(submit_results.len());
        for result in submit_results {
            match result {
                Ok((sender, balance, pending)) => {
                    receipt_futs.push(async move {
                        (
                            sender,
                            balance,
                            pending.with_timeout(Some(B20_RECEIPT_TIMEOUT)).get_receipt().await,
                        )
                    });
                }
                Err((sender, e)) => {
                    warn!(sender = %sender, error = %e, "B-20 burn failed");
                    burn_failed += 1;
                    pb.inc(1);
                }
            }
        }

        let mut receipt_stream = stream::iter(receipt_futs).buffer_unordered(FUNDING_CONCURRENCY);
        while let Some((sender, balance, result)) = receipt_stream.next().await {
            match result {
                Ok(receipt) if receipt.status() => {
                    debug!(sender = %sender, amount = %balance, tx_hash = %receipt.transaction_hash, "B-20 burn confirmed");
                    burn_count += 1;
                }
                Ok(receipt) => {
                    warn!(sender = %sender, tx_hash = %receipt.transaction_hash, "B-20 burn reverted");
                    burn_failed += 1;
                }
                Err(e) => {
                    warn!(sender = %sender, error = %e, "B-20 burn receipt failed");
                    burn_failed += 1;
                }
            }
            pb.inc(1);
        }

        pb.finish_and_clear();

        if burn_failed > 0 {
            warn!(failed = burn_failed, succeeded = burn_count, "some B-20 burns failed");
        }

        info!(burned = burn_count, failed = burn_failed, "B-20 teardown complete");
        Ok(())
    }
}
