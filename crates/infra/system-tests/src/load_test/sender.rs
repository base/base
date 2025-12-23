use super::tracker::TransactionTracker;
use super::wallet::Wallet;
use crate::client::TipsRpcClient;
use crate::fixtures::create_load_test_transaction;
use alloy_network::Network;
use alloy_primitives::{Address, Bytes, keccak256};
use alloy_provider::{Provider, RootProvider};
use anyhow::{Context, Result};
use op_alloy_network::Optimism;
use rand::Rng;
use rand_chacha::ChaCha8Rng;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

const MAX_RETRIES: u32 = 3;
const INITIAL_BACKOFF_MS: u64 = 100;

pub struct SenderTask<N: Network> {
    wallet: Wallet,
    client: TipsRpcClient<N>,
    sequencer: RootProvider<Optimism>,
    rate_per_wallet: f64,
    duration: Duration,
    tracker: Arc<TransactionTracker>,
    rng: ChaCha8Rng,
}

impl<N: Network> SenderTask<N> {
    pub fn new(
        wallet: Wallet,
        client: TipsRpcClient<N>,
        sequencer: RootProvider<Optimism>,
        rate_per_wallet: f64,
        duration: Duration,
        tracker: Arc<TransactionTracker>,
        rng: ChaCha8Rng,
    ) -> Self {
        Self {
            wallet,
            client,
            sequencer,
            rate_per_wallet,
            duration,
            tracker,
            rng,
        }
    }

    pub async fn run(mut self) -> Result<()> {
        let mut nonce = self
            .sequencer
            .get_transaction_count(self.wallet.address)
            .await
            .context("Failed to get initial nonce")?;

        let interval_duration = Duration::from_secs_f64(1.0 / self.rate_per_wallet);
        let mut ticker = tokio::time::interval(interval_duration);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let deadline = Instant::now() + self.duration;

        while Instant::now() < deadline {
            ticker.tick().await;

            let recipient = self.random_address();
            let tx_bytes = self.create_transaction(recipient, nonce)?;
            let tx_hash = keccak256(&tx_bytes);

            // Retry loop with exponential backoff
            let mut retries = 0;
            let mut backoff_ms = INITIAL_BACKOFF_MS;

            loop {
                let send_time = Instant::now();

                match self.client.send_raw_transaction(tx_bytes.clone()).await {
                    Ok(_) => {
                        self.tracker.record_sent(tx_hash, send_time);
                        nonce += 1;
                        break;
                    }
                    Err(e) => {
                        retries += 1;
                        if retries > MAX_RETRIES {
                            println!(
                                "Error sending raw transaction after {MAX_RETRIES} retries: {e}"
                            );
                            self.tracker.record_send_error();
                            nonce += 1; // Move on to next nonce after max retries
                            break;
                        }
                        // Exponential backoff before retry
                        sleep(Duration::from_millis(backoff_ms)).await;
                        backoff_ms *= 2; // Double backoff each retry
                    }
                }
            }
        }

        Ok(())
    }

    fn create_transaction(&self, to: Address, nonce: u64) -> Result<Bytes> {
        create_load_test_transaction(&self.wallet.signer, to, nonce)
    }

    fn random_address(&mut self) -> Address {
        let mut bytes = [0u8; 20];
        self.rng.fill(&mut bytes);
        Address::from(bytes)
    }
}
