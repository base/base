use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_primitives::U256;
use alloy_provider::Provider;
use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use op_alloy_network::{TxSignerSync, eip2718::Encodable2718};

use super::{
    config::SetupArgs,
    wallet::{Wallet, generate_wallets, save_wallets},
};
use crate::fixtures::create_optimism_provider;

const CHAIN_ID: u64 = 13; // builder-playground local chain ID

pub async fn run(args: SetupArgs) -> Result<()> {
    let master_wallet = Wallet::from_private_key(&args.master_key)
        .context("Failed to parse master wallet private key")?;

    let provider = create_optimism_provider(&args.sequencer)?;

    let master_balance = provider
        .get_balance(master_wallet.address)
        .await
        .context("Failed to get master wallet balance")?;

    let required_balance =
        U256::from((args.fund_amount * 1e18) as u64) * U256::from(args.num_wallets);

    if master_balance < required_balance {
        anyhow::bail!(
            "Insufficient master wallet balance. Need {} ETH, have {} ETH",
            required_balance.to::<u128>() as f64 / 1e18,
            master_balance.to::<u128>() as f64 / 1e18
        );
    }

    let wallets = generate_wallets(args.num_wallets, None);

    let pb = ProgressBar::new(args.num_wallets as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("##-"),
    );

    let mut nonce = provider
        .get_transaction_count(master_wallet.address)
        .await
        .context("Failed to get master wallet nonce")?;

    let fund_amount_wei = U256::from((args.fund_amount * 1e18) as u64);

    // Send all funding transactions
    let mut pending_txs = Vec::new();
    for (i, wallet) in wallets.iter().enumerate() {
        let mut tx = TxEip1559 {
            chain_id: CHAIN_ID,
            nonce,
            gas_limit: 21000,
            max_fee_per_gas: 1_000_000_000,        // 1 gwei
            max_priority_fee_per_gas: 100_000_000, // 0.1 gwei
            to: wallet.address.into(),
            value: fund_amount_wei,
            access_list: Default::default(),
            input: Default::default(),
        };

        let signature = master_wallet.signer.sign_transaction_sync(&mut tx)?;
        let envelope = op_alloy_consensus::OpTxEnvelope::Eip1559(tx.into_signed(signature));

        let mut buf = Vec::new();
        envelope.encode_2718(&mut buf);
        let pending = provider
            .send_raw_transaction(buf.as_ref())
            .await
            .with_context(|| format!("Failed to send funding tx for wallet {i}"))?;

        pending_txs.push(pending);
        nonce += 1;
        pb.set_message(format!("Sent funding tx {}", i + 1));
        pb.inc(1);
    }

    pb.finish_with_message("All funding transactions sent!");

    // Save wallets to file
    save_wallets(&wallets, args.fund_amount, &args.output)?;

    Ok(())
}
