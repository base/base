//! Transaction generator for load testing.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_consensus::SignableTransaction;
use alloy_eips::{BlockNumberOrTag, eip2718::Encodable2718};
use alloy_primitives::{Address, B256, Bytes};
use alloy_provider::{Provider, RootProvider};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use eyre::{Result, WrapErr};
use op_alloy_network::{Optimism, TransactionBuilder};
use op_alloy_rpc_types::OpTransactionRequest;
use rand::Rng;
use tokio::{
    sync::Semaphore,
    time::{interval, sleep},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::{
    config::LoadConfig,
    simulator::{build_simulator_config, encode_run_call},
    stats::Stats,
};

/// Generates load test transactions at a configurable rate.
#[derive(Debug)]
pub struct Generator {
    /// The provider to use for sending transactions.
    provider: RootProvider<Optimism>,
    /// The signer to use for signing transactions.
    signer: PrivateKeySigner,
    /// The configuration for the generator.
    config: LoadConfig,
    /// The address of the contract to test.
    contract_address: Address,
    /// The chain ID to use for the generator.
    chain_id: u64,
    /// The nonce to use for the generator.
    nonce: Arc<AtomicU64>,
    /// The statistics for the generator.
    stats: Arc<Stats>,
}

impl Generator {
    /// Creates a new generator with the given configuration.
    pub async fn new(
        provider: RootProvider<Optimism>,
        signer: PrivateKeySigner,
        config: LoadConfig,
        contract_address: Address,
        chain_id: u64,
    ) -> Result<Self> {
        let nonce = provider
            .get_transaction_count(signer.address())
            .block_id(BlockNumberOrTag::Latest.into())
            .await
            .wrap_err("Failed to get initial nonce")?;

        Ok(Self {
            provider,
            signer,
            config,
            contract_address,
            chain_id,
            nonce: Arc::new(AtomicU64::new(nonce)),
            stats: Arc::new(Stats::new()),
        })
    }

    /// Returns a shared handle to the statistics collector.
    pub fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
    }

    /// Runs the generator until duration elapses or shutdown is signaled.
    pub async fn run(&self, shutdown: CancellationToken) -> Result<()> {
        let semaphore = Arc::new(Semaphore::new(self.config.parallel));
        let tx_interval = Duration::from_secs_f64(1.0 / self.config.tx_rate);
        let mut interval_timer = interval(tx_interval);

        let gas_price = self.provider.get_gas_price().await.unwrap_or(1_000_000_000);
        let max_fee = gas_price * 2;

        info!(
            tx_rate = self.config.tx_rate,
            parallel = self.config.parallel,
            duration_secs = self.config.duration_secs,
            "Starting load generator"
        );

        let start = Instant::now();
        let duration = Duration::from_secs(self.config.duration_secs);

        loop {
            if start.elapsed() >= duration {
                info!("Duration elapsed, stopping generator");
                break;
            }

            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("Shutdown signal received");
                    break;
                }
                _ = interval_timer.tick() => {
                    let permit = match Arc::clone(&semaphore).try_acquire_owned() {
                        Ok(p) => p,
                        Err(_) => {
                            debug!("At max parallelism, skipping tick");
                            continue;
                        }
                    };

                    let nonce_increment = if self.config.calldata_bytes > 0 { 2 } else { 1 };
                    let tx_nonce = self.nonce.fetch_add(nonce_increment, Ordering::SeqCst);
                    let priority_fee = sample_priority_fee(&self.config);

                    let provider = self.provider.clone();
                    let signer = self.signer.clone();
                    let config = self.config.clone();
                    let contract_address = self.contract_address;
                    let chain_id = self.chain_id;
                    let stats = Arc::clone(&self.stats);

                    tokio::spawn(async move {
                        let result = send_transaction(
                            &provider,
                            &signer,
                            &config,
                            contract_address,
                            chain_id,
                            tx_nonce,
                            priority_fee,
                            max_fee,
                        )
                        .await;

                        match result {
                            Ok(tx_hash) => {
                                stats.record_submitted();
                                debug!(%tx_hash, nonce = tx_nonce, "Transaction sent");
                            }
                            Err(e) => {
                                stats.record_failed();
                                warn!(error = %e, nonce = tx_nonce, "Transaction failed");
                            }
                        }

                        drop(permit);
                    });
                }
            }
        }

        let _ = semaphore.acquire_many(self.config.parallel as u32).await;
        info!("All in-flight transactions completed");

        Ok(())
    }

    /// Waits for pending transaction receipts up to the given timeout.
    pub async fn wait_for_receipts(&self, timeout_secs: u64) -> Result<()> {
        let submitted = self.stats.submitted();
        if submitted == 0 {
            return Ok(());
        }
        info!(submitted, "Waiting for transaction receipts (up to {}s)", timeout_secs);

        let start = Instant::now();
        let timeout = Duration::from_secs(timeout_secs);

        while start.elapsed() < timeout {
            let block_number = self.provider.get_block_number().await?;
            debug!(block_number, "Current block");

            if start.elapsed() > Duration::from_secs(5) {
                break;
            }
            sleep(Duration::from_secs(1)).await;
        }

        Ok(())
    }
}

/// Sends a transaction to the contract with the given parameters.
#[allow(clippy::too_many_arguments)]
async fn send_transaction(
    provider: &RootProvider<Optimism>,
    signer: &PrivateKeySigner,
    config: &LoadConfig,
    contract_address: Address,
    chain_id: u64,
    nonce: u64,
    priority_fee: u128,
    max_fee: u128,
) -> Result<B256> {
    let sim_config = build_simulator_config(config.create_storage, config.create_accounts);
    let calldata = encode_run_call(&sim_config);

    let gas_limit = 1_000_000u64;

    let mut tx_request = OpTransactionRequest::default()
        .from(signer.address())
        .to(contract_address)
        .input(calldata.into())
        .with_nonce(nonce)
        .with_gas_limit(gas_limit)
        .with_max_fee_per_gas(max_fee)
        .with_max_priority_fee_per_gas(priority_fee);
    tx_request.set_chain_id(chain_id);

    let tx = tx_request
        .build_typed_tx()
        .map_err(|e| eyre::eyre!("Failed to build typed tx: {:?}", e))?;

    let signature = signer.sign_hash_sync(&tx.signature_hash())?;
    let signed_tx = tx.into_signed(signature);
    let raw_tx: Bytes = signed_tx.encoded_2718().into();
    let tx_hash = *signed_tx.hash();

    let _ =
        provider.send_raw_transaction(&raw_tx).await.wrap_err("Failed to send raw transaction")?;

    debug!(
        %tx_hash,
        nonce,
        priority_fee_gwei = priority_fee as f64 / 1e9,
        "Simulator tx sent"
    );

    if config.calldata_bytes > 0 {
        let da_nonce = nonce + 1;
        let da_raw_tx =
            build_da_tx(config.calldata_bytes, signer, da_nonce, priority_fee, max_fee, chain_id)?;

        let da_tx_hash = alloy_primitives::keccak256(&da_raw_tx);

        let _ = provider
            .send_raw_transaction(&da_raw_tx)
            .await
            .wrap_err("Failed to send DA transaction")?;

        debug!(
            %da_tx_hash,
            da_nonce,
            calldata_bytes = config.calldata_bytes,
            "DA tx sent"
        );
    }

    Ok(tx_hash)
}

/// Builds a DA transaction with the given parameters.
fn build_da_tx(
    calldata_bytes: usize,
    signer: &PrivateKeySigner,
    nonce: u64,
    priority_fee: u128,
    max_fee: u128,
    chain_id: u64,
) -> Result<Bytes> {
    let mut rng = rand::rng();

    let mut calldata = vec![0u8; calldata_bytes];
    rng.fill(&mut calldata[..]);

    // 21000 base + 16 per non-zero byte + buffer for L1 data costs on OP chains
    let gas_limit = 21_000 + (calldata_bytes as u64 * 16) + 200_000;

    let mut tx_request = OpTransactionRequest::default()
        .from(signer.address())
        .to(Address::ZERO)
        .input(Bytes::from(calldata).into())
        .with_nonce(nonce)
        .with_gas_limit(gas_limit)
        .with_max_fee_per_gas(max_fee)
        .with_max_priority_fee_per_gas(priority_fee);
    tx_request.set_chain_id(chain_id);

    let tx = tx_request
        .build_typed_tx()
        .map_err(|e| eyre::eyre!("Failed to build typed tx: {:?}", e))?;

    let signature = signer.sign_hash_sync(&tx.signature_hash())?;
    let signed_tx = tx.into_signed(signature);

    Ok(Bytes::from(signed_tx.encoded_2718()))
}

/// Samples a priority fee from the configuration. Returns a random value between the minimum and maximum priority fees.
fn sample_priority_fee(config: &LoadConfig) -> u128 {
    let mut rng = rand::rng();
    let min = config.priority_fee_min_wei();
    let max = config.priority_fee_max_wei();
    if min >= max { min } else { rng.random_range(min..=max) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ANVIL_ACCOUNT_0;

    #[test]
    fn test_build_da_tx() {
        let signer: PrivateKeySigner =
            PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_0.private_key).unwrap();

        let tx = build_da_tx(1000, &signer, 0, 1_000_000, 2_000_000_000, 1);
        assert!(tx.is_ok());

        let raw = tx.unwrap();
        assert!(raw.len() > 100);
    }
}
