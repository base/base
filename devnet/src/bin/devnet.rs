//! CLI binary for managing local L1+L2 devnet.

use std::{fs, io::Write, path::PathBuf};

use alloy_primitives::utils::format_ether;
use base_primitives::Flashblock;
use clap::Parser;
use devnet::{
    DevnetBuilder, DevnetUrls,
    cli::{Command, DevnetCli},
    docker::{
        cleanup_devnet_network, is_devnet_running, list_devnet_containers, stop_devnet_containers,
    },
    rpc::DevnetRpcClient,
};
use eyre::{Result, WrapErr};
use futures_util::StreamExt;
use tokio_tungstenite::connect_async;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = DevnetCli::parse();

    match cli.command {
        Command::Start => start_devnet().await,
        Command::Status => status_devnet().await,
        Command::Accounts => accounts_devnet().await,
        Command::Smoke => smoke_devnet().await,
        Command::Clean => clean_devnet(),
        Command::Flashblocks => flashblocks_devnet().await,
    }
}

async fn start_devnet() -> Result<()> {
    let existing_containers = list_devnet_containers()?;
    if !existing_containers.is_empty() {
        println!("Cleaning up existing devnet containers...");
        stop_devnet_containers()?;
        cleanup_devnet_network()?;
    }

    let devnet_dir =
        std::env::current_dir().wrap_err("Failed to get current directory")?.join(".devnet");

    let _ = fs::remove_dir_all(&devnet_dir);
    fs::create_dir_all(devnet_dir.join("shared"))?;

    println!("Starting devnet...");

    let devnet = DevnetBuilder::new()
        .with_output_dir(devnet_dir.clone())
        .with_stable_config()
        .build()
        .await?;

    let urls = devnet.urls().await?;
    urls.write_to_file(&devnet_dir.join("urls.json"))?;

    println!("\nDevnet started successfully!");
    println!("{urls}");
    println!("\nURLs saved to .devnet/urls.json");
    println!("\nPress Ctrl+C to stop the devnet...");

    tokio::signal::ctrl_c().await?;

    println!("\nShutting down devnet...");

    Ok(())
}

async fn smoke_devnet() -> Result<()> {
    use alloy_network::EthereumWallet;
    use alloy_provider::{Provider, ProviderBuilder};
    use alloy_rpc_types::TransactionRequest;
    use alloy_signer_local::PrivateKeySigner;
    use devnet::config::{ANVIL_ACCOUNT_1, ANVIL_ACCOUNT_2};

    if !is_devnet_running()? {
        eprintln!("Error: Devnet is not running. Start it with 'devnet start'");
        std::process::exit(1);
    }

    let urls = DevnetUrls::read_from_file(&PathBuf::from(".devnet/urls.json"))?;

    let signer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_1.private_key)
        .wrap_err("Invalid private key")?;
    let wallet = EthereumWallet::from(signer);
    let to_addr = ANVIL_ACCOUNT_2.address;

    println!("=== L1 Transaction Tests ===");

    println!("Sending L1 ETH tx...");
    let l1_provider =
        ProviderBuilder::new().wallet(wallet.clone()).connect_http(urls.l1_rpc.parse()?);

    let tx = TransactionRequest::default()
        .to(to_addr)
        .value(alloy_primitives::utils::parse_ether("0.001")?);

    let receipt = l1_provider.send_transaction(tx).await?.get_receipt().await?;
    println!(
        "ETH tx: {} block={} status={:?}",
        receipt.transaction_hash,
        receipt.block_number.unwrap_or(0),
        receipt.status()
    );

    println!();
    println!("=== L2 Transaction Tests ===");

    println!("Sending L2 tx to builder...");
    let l2_builder_provider =
        ProviderBuilder::new().wallet(wallet.clone()).connect_http(urls.l2_builder_rpc.parse()?);

    let tx = TransactionRequest::default()
        .to(to_addr)
        .value(alloy_primitives::utils::parse_ether("0.001")?);

    let receipt = l2_builder_provider.send_transaction(tx).await?.get_receipt().await?;
    println!("TX: {} block={}", receipt.transaction_hash, receipt.block_number.unwrap_or(0));

    println!("Sending L2 tx to client...");
    let l2_client_provider =
        ProviderBuilder::new().wallet(wallet).connect_http(urls.l2_client_rpc.parse()?);

    let tx = TransactionRequest::default()
        .to(to_addr)
        .value(alloy_primitives::utils::parse_ether("0.001")?);

    let receipt = l2_client_provider.send_transaction(tx).await?.get_receipt().await?;
    println!("TX: {} block={}", receipt.transaction_hash, receipt.block_number.unwrap_or(0));

    println!();
    println!("Smoke tests complete!");

    Ok(())
}

fn clean_devnet() -> Result<()> {
    let devnet_dir = PathBuf::from(".devnet");
    if devnet_dir.exists() {
        fs::remove_dir_all(&devnet_dir)?;
        println!("Removed .devnet directory");
    } else {
        println!(".devnet directory does not exist");
    }
    Ok(())
}

async fn status_devnet() -> Result<()> {
    if !is_devnet_running()? {
        eprintln!("Error: Devnet is not running. Start it with 'devnet start'");
        std::process::exit(1);
    }

    let urls = DevnetUrls::read_from_file(&PathBuf::from(".devnet/urls.json"))?;
    let client = DevnetRpcClient::new(
        &urls.l1_rpc,
        &urls.l2_builder_rpc,
        &urls.l2_client_rpc,
        &urls.l2_builder_op_rpc,
        &urls.l2_client_op_rpc,
    )?;

    let l1_block = client.l1_block_number().await?;
    let builder_status = client.l2_builder_sync_status().await?;
    let client_status = client.l2_client_sync_status().await?;

    let builder_unsafe = builder_status.unsafe_l2.block_info.number;
    let builder_safe = builder_status.safe_l2.block_info.number;
    let client_unsafe = client_status.unsafe_l2.block_info.number;
    let client_safe = client_status.safe_l2.block_info.number;

    println!();
    println!("{:<12} | {:<10} | {:<10}", "Component", "Unsafe", "Safe");
    println!("{:<12}-+-{:<10}-+-{:<10}", "------------", "----------", "----------");
    println!("{:<12} | {:<10} | {:<10}", "L1", l1_block, "-");
    println!("{:<12} | {:<10} | {:<10}", "L2 Builder", builder_unsafe, builder_safe);
    println!("{:<12} | {:<10} | {:<10}", "L2 Client", client_unsafe, client_safe);
    println!();

    Ok(())
}

async fn accounts_devnet() -> Result<()> {
    if !is_devnet_running()? {
        eprintln!("Error: Devnet is not running. Start it with 'devnet start'");
        std::process::exit(1);
    }

    let urls = DevnetUrls::read_from_file(&PathBuf::from(".devnet/urls.json"))?;
    let client = DevnetRpcClient::new(
        &urls.l1_rpc,
        &urls.l2_builder_rpc,
        &urls.l2_client_rpc,
        &urls.l2_builder_op_rpc,
        &urls.l2_client_op_rpc,
    )?;

    println!("Devnet Accounts");
    println!("===============");
    println!("{:<4} {:<44} {:>20} {:>20}", "#", "Address", "L1 Balance (ETH)", "L2 Balance (ETH)");
    println!("{}", "-".repeat(92));

    use devnet::config::{
        ANVIL_ACCOUNT_0, ANVIL_ACCOUNT_1, ANVIL_ACCOUNT_2, ANVIL_ACCOUNT_3, ANVIL_ACCOUNT_4,
        ANVIL_ACCOUNT_5, ANVIL_ACCOUNT_6, ANVIL_ACCOUNT_7, ANVIL_ACCOUNT_8, ANVIL_ACCOUNT_9,
    };
    let accounts = [
        &*ANVIL_ACCOUNT_0,
        &*ANVIL_ACCOUNT_1,
        &*ANVIL_ACCOUNT_2,
        &*ANVIL_ACCOUNT_3,
        &*ANVIL_ACCOUNT_4,
        &*ANVIL_ACCOUNT_5,
        &*ANVIL_ACCOUNT_6,
        &*ANVIL_ACCOUNT_7,
        &*ANVIL_ACCOUNT_8,
        &*ANVIL_ACCOUNT_9,
    ];

    for (i, account) in accounts.iter().enumerate() {
        let (l1_bal, l2_bal, _) = client.get_balance(account.address).await?;
        let l1_eth = format_ether(l1_bal);
        let l2_eth = format_ether(l2_bal);
        println!("{:<4} {:<44} {:>20} {:>20}", i, format!("{:?}", account.address), l1_eth, l2_eth);
    }

    Ok(())
}

async fn flashblocks_devnet() -> Result<()> {
    if !is_devnet_running()? {
        eprintln!("Error: Devnet is not running. Start it with 'devnet start'");
        std::process::exit(1);
    }

    let ws_url = "ws://localhost:7111";

    eprintln!("Connecting to flashblocks stream at {ws_url}...");

    let (ws_stream, _) =
        connect_async(ws_url).await.wrap_err("Failed to connect to flashblocks WebSocket")?;

    let (_, mut read) = ws_stream.split();

    eprintln!("Connected! Streaming flashblocks (Ctrl+C to stop)...");

    while let Some(msg) = read.next().await {
        match msg {
            Ok(msg) => {
                let data = msg.into_data();
                match Flashblock::try_decode_message(data) {
                    Ok(flashblock) => {
                        let json = serde_json::to_string_pretty(&flashblock)
                            .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"));
                        if writeln!(std::io::stdout(), "{json}").is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("Failed to decode flashblock: {e}");
                    }
                }
            }
            Err(e) => {
                eprintln!("WebSocket error: {e}");
                break;
            }
        }
    }

    Ok(())
}
