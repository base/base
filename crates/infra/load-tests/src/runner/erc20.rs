//! Standalone Solidity ERC20 token lifecycle for load tests: deployment of a vanilla
//! ERC20 contract and balance distribution to senders during setup.
//!
//! Unlike the B-20 precompile (see [`super::b20`]), this deploys a regular Solidity ERC20
//! contract via a `CREATE` transaction and mints balances so that `transfer(...)` workloads
//! exercise real token movement rather than reverting on zero-balance senders.

use std::sync::Arc;

use alloy_network::{EthereumWallet, TransactionBuilder};
use alloy_primitives::{Address, U256};
use alloy_provider::Provider;
use alloy_rpc_types::TransactionRequest;
use alloy_signer_local::PrivateKeySigner;
use base_test_utils::LoadTestERC20;
use futures::{StreamExt, stream};
use tracing::{debug, info, instrument, warn};

use super::{LoadRunner, TxType, load_runner::BATCH_SIZE};
use crate::{
    BaselineError, Result,
    config::WorkloadConfig,
    rpc::{RpcResultExt, create_wallet_provider},
};

/// Gas limit for the one-off ERC20 deployment transaction.
const ERC20_DEPLOY_GAS_LIMIT: u64 = 1_500_000;

/// Gas limit for each ERC20 mint transaction during distribution.
const ERC20_MINT_GAS_LIMIT: u64 = 100_000;

impl LoadRunner {
    /// Returns `true` if any configured transaction type is [`TxType::Erc20`].
    pub fn needs_erc20_setup(&self) -> bool {
        self.config.transactions.iter().any(|t| matches!(t.tx_type, TxType::Erc20 { .. }))
    }

    /// Deploys a standalone ERC20 token (when no contract address is configured) and mints
    /// `amount_per_sender` to every sender account.
    ///
    /// If an ERC20 transaction config already has a resolved `contract` address, deployment is
    /// skipped and that token is reused for minting. The resolved address is written back into
    /// every [`TxType::Erc20`] config and the workload generator is rebuilt so the measured
    /// run targets the deployed token.
    #[instrument(skip(self, funding_key), fields(accounts = self.accounts.len()))]
    pub async fn setup_erc20_tokens(
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
        let gas_price = self.client.get_gas_price().await.rpc("get gas price")?;
        let max_priority_fee = (gas_price / 10).max(1);
        let max_fee = gas_price.saturating_mul(2).max(max_priority_fee).min(max_gas_price);

        let mut nonce = funder_provider
            .get_transaction_count(funder_address)
            .pending()
            .await
            .rpc("get pending transaction count")?;

        // Phase 1: Deploy a standalone ERC20 if no contract address is configured.
        let mut token_address: Option<Address> = self.config.transactions.iter().find_map(|t| {
            match &t.tx_type {
                TxType::Erc20 { contract: Some(addr) } => Some(*addr),
                _ => None,
            }
        });

        if token_address.is_none() {
            info!("deploying standalone ERC20 token");

            let predicted = funder_address.create(nonce);

            let tx = TransactionRequest::default()
                .with_deploy_code(LoadTestERC20::BYTECODE.clone())
                .with_nonce(nonce)
                .with_chain_id(chain_id)
                .with_gas_limit(ERC20_DEPLOY_GAS_LIMIT)
                .with_max_fee_per_gas(max_fee)
                .with_max_priority_fee_per_gas(max_priority_fee);
            nonce += 1;

            let pending = funder_provider.send_transaction(tx).await.map_err(|e| {
                BaselineError::Transaction(format!("failed to deploy ERC20 token: {e}"))
            })?;
            let receipt = pending.get_receipt().await.map_err(|e| {
                BaselineError::Transaction(format!("ERC20 deployment receipt failed: {e}"))
            })?;

            if !receipt.status() {
                return Err(BaselineError::Transaction(format!(
                    "ERC20 token deployment reverted (tx {})",
                    receipt.transaction_hash
                )));
            }

            let deployed = receipt.contract_address.unwrap_or(predicted);
            info!(token = %deployed, "ERC20 token deployed");
            token_address = Some(deployed);
        }

        let token = token_address.ok_or_else(|| {
            BaselineError::Config("ERC20 token address was not resolved during setup".into())
        })?;

        for tx_config in &mut self.config.transactions {
            if let TxType::Erc20 { contract } = &mut tx_config.tx_type {
                *contract = Some(token);
            }
        }

        // Phase 2: Mint tokens to all senders so transfers do not revert.
        let sender_addresses: Vec<Address> =
            self.accounts.accounts().iter().map(|a| a.address).collect();
        let total_mints = sender_addresses.len();
        let pb = self.progress_bar(total_mints as u64, "Minting ERC20 tokens");

        let mint_txs: Vec<(TransactionRequest, Address)> = sender_addresses
            .iter()
            .map(|&sender| {
                let tx = TransactionRequest::default()
                    .with_to(token)
                    .with_input(Self::encode_erc20_mint(sender, amount_per_sender))
                    .with_nonce(nonce)
                    .with_chain_id(chain_id)
                    .with_gas_limit(ERC20_MINT_GAS_LIMIT)
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
                        debug!(to = %sender, tx_hash = %receipt.transaction_hash, "ERC20 mint confirmed");
                        pb.inc(1);
                    }
                    Ok((receipt, sender)) => {
                        warn!(to = %sender, tx_hash = %receipt.transaction_hash, "ERC20 mint reverted");
                        mint_failed += 1;
                        pb.inc(1);
                    }
                    Err(e) => {
                        warn!(error = %e, "ERC20 mint failed");
                        mint_failed += 1;
                        pb.inc(1);
                    }
                }
            }
        }

        pb.finish_and_clear();
        if mint_failed > 0 {
            return Err(BaselineError::Transaction(format!(
                "{mint_failed}/{total_mints} ERC20 mints failed — senders with missing tokens \
                 will revert on transfer"
            )));
        }

        // Rebuild the workload generator now that the ERC20 contract address is resolved.
        let workload_config = WorkloadConfig::new("load-test").with_seed(self.config.seed);
        self.generator = Self::create_generator(workload_config, &self.config)?;

        info!(
            token = %token,
            senders = total_mints,
            amount = %amount_per_sender,
            "ERC20 token setup complete"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;

    use super::*;
    use crate::runner::{TxConfig, TxType};

    #[test]
    fn erc20_deploy_bytecode_is_nonempty() {
        assert!(!LoadTestERC20::BYTECODE.is_empty(), "ERC20 deploy bytecode must not be empty");
    }

    #[test]
    fn needs_erc20_setup_matches_resolved_and_unresolved_configs() {
        let unresolved = TxConfig { weight: 1, tx_type: TxType::Erc20 { contract: None } };
        let resolved = TxConfig {
            weight: 1,
            tx_type: TxType::Erc20 { contract: Some(Address::repeat_byte(0x11)) },
        };
        let other = TxConfig { weight: 1, tx_type: TxType::Transfer };

        assert!(matches!(unresolved.tx_type, TxType::Erc20 { .. }));
        assert!(matches!(resolved.tx_type, TxType::Erc20 { .. }));
        assert!(!matches!(other.tx_type, TxType::Erc20 { .. }));
    }
}
