//! B-20 precompile token lifecycle for load tests: creation, role grants,
//! minting during setup, and burning during teardown.

use std::sync::Arc;

use alloy_network::{EthereumWallet, TransactionBuilder};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::Provider;
use alloy_rpc_types::TransactionRequest;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolValue};
use base_common_precompiles::{B20FactoryStorage, B20TokenRole, B20Variant, IB20, IB20Factory};
use futures::{StreamExt, stream};
use tracing::{debug, info, instrument, warn};

use super::{
    LoadRunner, SubmissionPipeline, TxType,
    load_runner::{BATCH_SIZE, FUNDING_CONCURRENCY},
};
use crate::{
    BaselineError, Result,
    config::WorkloadConfig,
    rpc::{BaseFeeExt, RpcResultExt, create_wallet_provider},
};

impl LoadRunner {
    /// Returns `true` if any configured transaction type is [`TxType::B20`].
    pub fn needs_b20_setup(&self) -> bool {
        self.config.transactions.iter().any(|t| matches!(t.tx_type, TxType::B20 { .. }))
    }

    /// Creates a B-20 token via the factory, grants `MINT_ROLE` and `BURN_ROLE` to all senders,
    /// then mints tokens to every sender account.
    ///
    /// If all B-20 transaction configs already have a resolved `contract` address, this is a
    /// no-op for creation but still handles role grants and minting.
    #[instrument(skip(self, funding_key), fields(accounts = self.accounts.len()))]
    pub async fn setup_b20_tokens(
        &mut self,
        funding_key: PrivateKeySigner,
        amount_per_sender: U256,
    ) -> Result<()> {
        let funder_address = funding_key.address();
        let wallet = EthereumWallet::from(funding_key);
        let funder_provider =
            Arc::new(create_wallet_provider(self.config.primary_submission_rpc().clone(), wallet));
        let chain_id = self.config.chain_id;
        let max_gas_price = self.config.max_gas_price;
        let base_fee = self.client.get_base_fee().await?;
        let max_priority_fee = (base_fee / 10).max(1);
        let max_fee =
            SubmissionPipeline::submission_max_fee(base_fee, max_priority_fee, max_gas_price);
        let b20_gas_limit = 10_000_000u64;

        let mut nonce = funder_provider
            .get_transaction_count(funder_address)
            .pending()
            .await
            .rpc("get pending transaction count")?;

        // Phase 1: Create B-20 token if no contract address is configured.
        let mut token_address: Option<Address> = None;
        for tx_config in &self.config.transactions {
            if let TxType::B20 { contract: Some(addr) } = &tx_config.tx_type {
                token_address = Some(*addr);
                break;
            }
        }

        if token_address.is_none() {
            // B-20 activation is a one-time chain-lifecycle operation performed by the activation
            // admin (see `ActivationRegistry`), not by the load tester. The funder only creates a
            // token; if the feature is not active, the factory's `ensure_activated` check will
            // revert the create tx with `FeatureNotActivated`.
            info!("creating new B-20 token via factory");

            let salt = B256::from(rand::random::<[u8; 32]>());
            let predicted = B20Variant::Asset.compute_address(funder_address, salt).0;

            let params = IB20Factory::B20AssetCreateParams {
                version: B20Variant::Asset.supported_version(),
                name: "Load Test B20".to_string(),
                symbol: "LTB20".to_string(),
                initialAdmin: funder_address,
                decimals: 6,
            };

            // Factory sets the cap to `DEFAULT_SUPPLY_CAP` (== `B20_MAX_SUPPLY_CAP`) at
            // creation, so no init call is needed; an `updateSupplyCap(U256::MAX)` would
            // revert on builds that clamp the cap to `B20_MAX_SUPPLY_CAP`.
            let create_call = IB20Factory::createB20Call {
                variant: IB20Factory::B20Variant::ASSET,
                salt,
                params: params.abi_encode().into(),
                initCalls: Vec::new(),
            };

            let tx = TransactionRequest::default()
                .with_to(B20FactoryStorage::ADDRESS)
                .with_input(Bytes::from(create_call.abi_encode()))
                .with_nonce(nonce)
                .with_chain_id(chain_id)
                .with_gas_limit(b20_gas_limit)
                .with_max_fee_per_gas(max_fee)
                .with_max_priority_fee_per_gas(max_priority_fee);
            nonce += 1;

            let pending = funder_provider.send_transaction(tx).await.map_err(|e| {
                BaselineError::Transaction(format!("failed to create B-20 token: {e}"))
            })?;

            let receipt = pending.get_receipt().await.map_err(|e| {
                BaselineError::Transaction(format!("B-20 creation receipt failed: {e}"))
            })?;

            if !receipt.status() {
                return Err(BaselineError::Transaction(format!(
                    "B-20 token creation reverted (tx {}). \
                     Likely causes: B-20 feature not yet activated on this chain \
                     (activation is done once by the activation admin, not the load tester), \
                     or a factory validation error (decimals/version/initCall). \
                     Inspect the tx trace for the precise revert reason.",
                    receipt.transaction_hash
                )));
            }

            info!(token = %predicted, "B-20 token created");
            token_address = Some(predicted);
        }

        let token = token_address.ok_or_else(|| {
            BaselineError::Config("b20 token address was not resolved during setup".into())
        })?;

        for tx_config in &mut self.config.transactions {
            if let TxType::B20 { contract } = &mut tx_config.tx_type {
                *contract = Some(token);
            }
        }

        // Phase 2: Grant MINT_ROLE to funder + MINT_ROLE and BURN_ROLE to all senders.
        let sender_addresses: Vec<Address> =
            self.accounts.accounts().iter().map(|a| a.address).collect();
        let roles = [B20TokenRole::Mint.id(), B20TokenRole::Burn.id()];

        let total_grants = 1 + sender_addresses.len() * roles.len();
        let pb = self.progress_bar(total_grants as u64, "Granting B-20 roles");

        let mut grant_txs: Vec<TransactionRequest> = Vec::with_capacity(total_grants);

        // Funder needs MINT_ROLE to execute Phase 3 mints.
        let funder_mint_grant =
            IB20::grantRoleCall { role: B20TokenRole::Mint.id(), account: funder_address };
        grant_txs.push(
            TransactionRequest::default()
                .with_to(token)
                .with_input(Bytes::from(funder_mint_grant.abi_encode()))
                .with_nonce(nonce)
                .with_chain_id(chain_id)
                .with_gas_limit(b20_gas_limit)
                .with_max_fee_per_gas(max_fee)
                .with_max_priority_fee_per_gas(max_priority_fee),
        );
        nonce += 1;

        for &sender in &sender_addresses {
            for &role in &roles {
                let call = IB20::grantRoleCall { role, account: sender };
                grant_txs.push(
                    TransactionRequest::default()
                        .with_to(token)
                        .with_input(Bytes::from(call.abi_encode()))
                        .with_nonce(nonce)
                        .with_chain_id(chain_id)
                        .with_gas_limit(b20_gas_limit)
                        .with_max_fee_per_gas(max_fee)
                        .with_max_priority_fee_per_gas(max_priority_fee),
                );
                nonce += 1;
            }
        }

        let mut grant_failed = 0usize;
        let mut txs_remaining = grant_txs.into_iter().peekable();
        while txs_remaining.peek().is_some() {
            let batch: Vec<_> = txs_remaining.by_ref().take(BATCH_SIZE).collect();
            let send_futs = batch.into_iter().map(|tx| {
                let provider = Arc::clone(&funder_provider);
                async move {
                    let pending = provider.send_transaction(tx).await?;
                    pending.get_receipt().await
                }
            });

            let mut send_stream = stream::iter(send_futs).buffer_unordered(BATCH_SIZE);
            while let Some(result) = send_stream.next().await {
                match result {
                    Ok(receipt) if receipt.status() => pb.inc(1),
                    Ok(receipt) => {
                        warn!(tx_hash = %receipt.transaction_hash, "B-20 role grant reverted");
                        grant_failed += 1;
                        pb.inc(1);
                    }
                    Err(e) => {
                        warn!(error = %e, "B-20 role grant failed");
                        grant_failed += 1;
                        pb.inc(1);
                    }
                }
            }
        }

        pb.finish_and_clear();
        if grant_failed > 0 {
            return Err(BaselineError::Transaction(format!(
                "{grant_failed}/{total_grants} B-20 role grants failed"
            )));
        }

        info!(roles = total_grants, "B-20 roles granted");

        // Phase 3: Mint tokens to all senders.
        let total_mints = sender_addresses.len();
        let pb_mint = self.progress_bar(total_mints as u64, "Minting B-20 tokens");

        let mint_txs: Vec<(TransactionRequest, Address)> = sender_addresses
            .iter()
            .map(|&sender| {
                let call = IB20::mintCall { to: sender, amount: amount_per_sender };
                let tx = TransactionRequest::default()
                    .with_to(token)
                    .with_input(Bytes::from(call.abi_encode()))
                    .with_nonce(nonce)
                    .with_chain_id(chain_id)
                    .with_gas_limit(b20_gas_limit)
                    .with_max_fee_per_gas(max_fee)
                    .with_max_priority_fee_per_gas(max_priority_fee);
                nonce += 1;
                (tx, sender)
            })
            .collect();

        let mut mint_failed = 0usize;
        let mut txs_remaining = mint_txs.into_iter().peekable();

        while txs_remaining.peek().is_some() {
            let batch: Vec<_> = txs_remaining.by_ref().take(BATCH_SIZE).collect();
            let send_futs = batch.into_iter().map(|(tx, sender)| {
                let provider = Arc::clone(&funder_provider);
                async move {
                    match provider.send_transaction(tx).await {
                        Ok(pending) => {
                            let receipt = pending
                                .get_receipt()
                                .await
                                .map_err(|e| eyre::eyre!("mint receipt failed: {e}"))?;
                            Ok::<_, eyre::Report>((receipt, sender))
                        }
                        Err(e) => Err(eyre::eyre!("mint send failed: {e}")),
                    }
                }
            });

            let mut send_stream = stream::iter(send_futs).buffer_unordered(BATCH_SIZE);
            while let Some(result) = send_stream.next().await {
                match result {
                    Ok((receipt, sender)) if receipt.status() => {
                        debug!(to = %sender, tx_hash = %receipt.transaction_hash, "B-20 mint confirmed");
                        pb_mint.inc(1);
                    }
                    Ok((receipt, sender)) => {
                        warn!(to = %sender, tx_hash = %receipt.transaction_hash, "B-20 mint reverted");
                        mint_failed += 1;
                        pb_mint.inc(1);
                    }
                    Err(e) => {
                        warn!(error = %e, "B-20 mint failed");
                        mint_failed += 1;
                        pb_mint.inc(1);
                    }
                }
            }
        }

        pb_mint.finish_and_clear();
        if mint_failed > 0 {
            return Err(BaselineError::Transaction(format!(
                "{mint_failed}/{total_mints} B-20 mints failed"
            )));
        }

        // Rebuild the workload generator now that the B-20 contract address is resolved.
        let workload_config = WorkloadConfig::new("load-test").with_seed(self.config.seed);
        self.generator = Self::create_generator(workload_config, &self.config)?;

        info!(
            token = %token,
            senders = total_mints,
            amount = %amount_per_sender,
            "B-20 token setup complete"
        );
        Ok(())
    }

    /// Burns remaining B-20 token balances from all sender accounts.
    ///
    /// Each sender calls `burn(uint256)` with their full balance. Requires senders to hold
    /// `BURN_ROLE`, which is granted during [`Self::setup_b20_tokens`].
    #[instrument(skip(self), fields(accounts = self.accounts.len()))]
    pub async fn teardown_b20_tokens(&self) -> Result<()> {
        let token = self.config.transactions.iter().find_map(|t| match &t.tx_type {
            TxType::B20 { contract: Some(addr) } => Some(*addr),
            _ => None,
        });

        let Some(token) = token else {
            return Ok(());
        };

        let chain_id = self.config.chain_id;
        let max_gas_price = self.config.max_gas_price;
        let base_fee = self.client.get_base_fee().await?;
        let max_priority_fee = (base_fee / 10).max(1);
        let max_fee =
            SubmissionPipeline::submission_max_fee(base_fee, max_priority_fee, max_gas_price);
        let burn_gas_limit = 200_000u64;

        let sender_addresses: Vec<Address> =
            self.accounts.accounts().iter().map(|a| a.address).collect();

        let signers = Self::build_signers(&self.accounts);
        let client = &self.client;
        let rpc_url = self.config.primary_submission_rpc().clone();

        // Phase 1: Query all balances in parallel.
        let balance_futs: Vec<_> = sender_addresses
            .iter()
            .map(|&sender| {
                let client = client.clone();
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
                    (sender, balance)
                }
            })
            .collect();

        let balances: Vec<_> =
            stream::iter(balance_futs).buffer_unordered(FUNDING_CONCURRENCY).collect().await;

        let senders_with_balance: Vec<_> =
            balances.into_iter().filter(|(_, balance)| !balance.is_zero()).collect();

        if senders_with_balance.is_empty() {
            info!("all B-20 balances are zero, skipping teardown");
            return Ok(());
        }

        // Phase 2: Build burn txs — each sender burns their own balance.
        let pb = self.progress_bar(senders_with_balance.len() as u64, "Burning B-20 tokens");
        let mut burn_failed = 0usize;
        let mut burn_count = 0usize;

        let burn_futs: Vec<_> = senders_with_balance
            .into_iter()
            .filter_map(|(sender, balance)| {
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
                        .with_gas_limit(burn_gas_limit)
                        .with_max_fee_per_gas(max_fee)
                        .with_max_priority_fee_per_gas(max_priority_fee);

                    match provider.send_transaction(tx).await {
                        Ok(pending) => match pending.get_receipt().await {
                            Ok(receipt) => Ok((sender, balance, receipt)),
                            Err(e) => Err((sender, eyre::eyre!("receipt failed: {e}"))),
                        },
                        Err(e) => Err((sender, eyre::eyre!("send failed: {e}"))),
                    }
                })
            })
            .collect();

        let mut burn_stream = stream::iter(burn_futs).buffer_unordered(BATCH_SIZE);
        while let Some(result) = burn_stream.next().await {
            match result {
                Ok((sender, balance, receipt)) if receipt.status() => {
                    debug!(sender = %sender, amount = %balance, tx_hash = %receipt.transaction_hash, "B-20 burn confirmed");
                    burn_count += 1;
                }
                Ok((sender, _, receipt)) => {
                    warn!(sender = %sender, tx_hash = %receipt.transaction_hash, "B-20 burn reverted");
                    burn_failed += 1;
                }
                Err((sender, e)) => {
                    warn!(sender = %sender, error = %e, "B-20 burn failed");
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
