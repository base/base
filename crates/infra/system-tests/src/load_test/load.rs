use super::config::LoadArgs;
use super::metrics::{TestConfig, calculate_results};
use super::output::{print_results, save_results};
use super::poller::ReceiptPoller;
use super::sender::SenderTask;
use super::tracker::TransactionTracker;
use super::wallet::load_wallets;
use crate::client::TipsRpcClient;
use crate::fixtures::create_optimism_provider;
use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use std::sync::Arc;
use std::time::Duration;

pub async fn run(args: LoadArgs) -> Result<()> {
    let wallets = load_wallets(&args.wallets).context("Failed to load wallets")?;

    if wallets.is_empty() {
        anyhow::bail!("No wallets found in file. Run 'setup' command first.");
    }

    let num_wallets = wallets.len();

    let sequencer = create_optimism_provider(&args.sequencer)?;

    let tips_provider = create_optimism_provider(&args.target)?;
    let tips_client = TipsRpcClient::new(tips_provider);

    let tracker = TransactionTracker::new(Duration::from_secs(args.duration));

    let rate_per_wallet = args.rate as f64 / num_wallets as f64;

    let pb = ProgressBar::new(args.duration);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len}s | Sent: {msg}")
            .unwrap()
            .progress_chars("##-"),
    );

    let poller = ReceiptPoller::new(
        sequencer.clone(),
        Arc::clone(&tracker),
        Duration::from_secs(args.tx_timeout),
    );
    let poller_handle = tokio::spawn(async move { poller.run().await });

    let mut sender_handles = Vec::new();

    for (i, wallet) in wallets.into_iter().enumerate() {
        let rng = match args.seed {
            Some(seed) => ChaCha8Rng::seed_from_u64(seed + i as u64),
            None => ChaCha8Rng::from_entropy(),
        };

        let sender = SenderTask::new(
            wallet,
            tips_client.clone(),
            sequencer.clone(),
            rate_per_wallet,
            Duration::from_secs(args.duration),
            Arc::clone(&tracker),
            rng,
        );

        let handle = tokio::spawn(async move { sender.run().await });

        sender_handles.push(handle);
    }

    let pb_tracker = Arc::clone(&tracker);
    let pb_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            let elapsed = pb_tracker.elapsed().as_secs();
            let sent = pb_tracker.total_sent();
            pb.set_position(elapsed);
            pb.set_message(format!("{sent}"));

            if pb_tracker.is_test_completed() {
                break;
            }
        }
        pb.finish_with_message("Complete");
    });

    for handle in sender_handles {
        handle.await??;
    }

    tracker.mark_test_completed();

    pb_handle.await?;

    let grace_period = Duration::from_secs(args.tx_timeout + 10);
    match tokio::time::timeout(grace_period, poller_handle).await {
        Ok(Ok(Ok(()))) => {
            println!("✅ All transactions resolved");
        }
        Ok(Ok(Err(e))) => {
            println!("⚠️  Poller error: {e}");
        }
        Ok(Err(e)) => {
            println!("⚠️  Poller panicked: {e}");
        }
        Err(_) => {
            println!("⏱️  Grace period expired, some transactions may still be pending");
        }
    }

    let config = TestConfig {
        target: args.target.clone(),
        sequencer: args.sequencer.clone(),
        wallets: num_wallets,
        target_rate: args.rate,
        duration_secs: args.duration,
        tx_timeout_secs: args.tx_timeout,
        seed: args.seed,
    };

    let results = calculate_results(&tracker, config);
    print_results(&results);

    // Save results if output file specified
    if let Some(output_path) = args.output.as_ref() {
        save_results(&results, output_path)?;
    }

    Ok(())
}
