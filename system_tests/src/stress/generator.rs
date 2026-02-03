//! Transaction generator for stress testing.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use alloy_consensus::SignableTransaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_network::Ethereum;
use alloy_primitives::{Address, Bytes};
use alloy_provider::{Provider, RootProvider};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use eyre::{Result, WrapErr};
use op_alloy_network::TransactionBuilder;
use op_alloy_rpc_types::OpTransactionRequest;
use tokio::sync::Semaphore;
use tokio::time::{interval, sleep};
use tokio_util::sync::CancellationToken;

use super::config::StressConfig;
use super::simulator::{build_simulator_config, encode_run_call};
use super::stats::Stats;

/// Generates stress test transactions at a configurable rate.
#[derive(Debug)]
pub struct Generator {
    provider: RootProvider<Ethereum>,
    signer: PrivateKeySigner,
    config: StressConfig,
    contract_address: Address,
    chain_id: u64,
    nonce: AtomicU64,
    stats: Arc<Stats>,
}

impl Generator {
    /// Creates a new generator with the given configuration.
    pub async fn new(
        rpc_url: &str,
        signer: PrivateKeySigner,
        config: StressConfig,
        contract_address: Address,
        chain_id: u64,
    ) -> Result<Self> {
        let client = alloy_rpc_client::RpcClient::builder().http(rpc_url.parse()?);
        let provider = RootProvider::<Ethereum>::new(client);

        let initial_nonce = provider
            .get_transaction_count(signer.address())
            .await
            .wrap_err("Failed to get initial nonce")?;

        Ok(Self {
            provider,
            signer,
            config,
            contract_address,
            chain_id,
            nonce: AtomicU64::new(initial_nonce),
            stats: Arc::new(Stats::new()),
        })
    }

    /// Returns a shared handle to the statistics collector.
    pub fn stats(&self) -> Arc<Stats> {
        self.stats.clone()
    }

    /// Runs the generator until duration elapses or shutdown is signaled.
    pub async fn run(&self, shutdown: CancellationToken) -> Result<()> {
        let semaphore = Arc::new(Semaphore::new(self.config.parallel));
        let tx_interval = Duration::from_secs_f64(1.0 / self.config.tx_rate);
        let mut interval_timer = interval(tx_interval);

        let gas_price = self.provider.get_gas_price().await.unwrap_or(1_000_000_000);
        let max_fee = gas_price * 2;

        tracing::info!(
            tx_rate = self.config.tx_rate,
            parallel = self.config.parallel,
            duration_secs = self.config.duration_secs,
            "Starting stress generator"
        );

        let start = std::time::Instant::now();
        let duration = Duration::from_secs(self.config.duration_secs);

        loop {
            if start.elapsed() >= duration {
                tracing::info!("Duration elapsed, stopping generator");
                break;
            }

            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::info!("Shutdown signal received");
                    break;
                }
                _ = interval_timer.tick() => {
                    let permit = match semaphore.clone().try_acquire_owned() {
                        Ok(p) => p,
                        Err(_) => {
                            tracing::debug!("At max parallelism, skipping tick");
                            continue;
                        }
                    };

                    let tx_nonce = self.nonce.fetch_add(1, Ordering::SeqCst);
                    let priority_fee = self.sample_priority_fee();

                    let tx_result = self.build_and_send_tx(tx_nonce, priority_fee, max_fee).await;

                    match tx_result {
                        Ok(tx_hash) => {
                            self.stats.record_submitted();
                            tracing::debug!(%tx_hash, nonce = tx_nonce, "Transaction sent");
                        }
                        Err(e) => {
                            self.stats.record_failed();
                            tracing::warn!(error = %e, nonce = tx_nonce, "Transaction failed");
                        }
                    }

                    drop(permit);
                }
            }
        }

        let _ = semaphore
            .acquire_many(self.config.parallel as u32)
            .await;
        tracing::info!("All in-flight transactions completed");

        Ok(())
    }

    fn sample_priority_fee(&self) -> u128 {
        use rand::Rng;
        let mut rng = rand::rng();
        let min = self.config.priority_fee_min_wei();
        let max = self.config.priority_fee_max_wei();
        if min >= max {
            min
        } else {
            rng.random_range(min..=max)
        }
    }

    async fn build_and_send_tx(
        &self,
        nonce: u64,
        priority_fee: u128,
        max_fee: u128,
    ) -> Result<alloy_primitives::B256> {
        let sim_config = build_simulator_config(
            self.config.create_storage,
            self.config.create_accounts,
        );
        let calldata = encode_run_call(&sim_config);

        let gas_limit = 1_000_000u64;

        let mut tx_request = OpTransactionRequest::default()
            .from(self.signer.address())
            .to(self.contract_address)
            .input(calldata.into())
            .with_nonce(nonce)
            .with_gas_limit(gas_limit)
            .with_max_fee_per_gas(max_fee)
            .with_max_priority_fee_per_gas(priority_fee);
        tx_request.set_chain_id(self.chain_id);

        let tx = tx_request
            .build_typed_tx()
            .map_err(|e| eyre::eyre!("Failed to build typed tx: {:?}", e))?;

        let signature = self.signer.sign_hash_sync(&tx.signature_hash())?;
        let signed_tx = tx.into_signed(signature);
        let raw_tx: Bytes = signed_tx.encoded_2718().into();
        let tx_hash = *signed_tx.hash();

        let _ = self.provider
            .send_raw_transaction(&raw_tx)
            .await
            .wrap_err("Failed to send raw transaction")?;

        Ok(tx_hash)
    }

    /// Waits for pending transaction receipts up to the given timeout.
    pub async fn wait_for_receipts(&self, timeout_secs: u64) -> Result<()> {
        let submitted = self.stats.submitted();
        if submitted == 0 {
            return Ok(());
        }

        tracing::info!(
            submitted,
            "Waiting for transaction receipts (up to {}s)",
            timeout_secs
        );

        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(timeout_secs);

        while start.elapsed() < timeout {
            let block_number = self.provider.get_block_number().await?;
            tracing::debug!(block_number, "Current block");

            if start.elapsed() > Duration::from_secs(5) {
                break;
            }
            sleep(Duration::from_secs(1)).await;
        }

        Ok(())
    }
}
