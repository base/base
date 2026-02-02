//! CLI binary for managing local L1+L2 devnet.

use std::{fs, io::Write, path::PathBuf};

use alloy_primitives::U256;
use base_flashtypes::Flashblock;
use clap::Parser;
use eyre::{Result, WrapErr};
use futures_util::StreamExt;
use system_tests::{
    DevnetBuilder,
    cli::{Command, DevnetCli},
    docker::{
        cleanup_devnet_network, is_devnet_running, list_devnet_containers, stop_devnet_containers,
    },
    rpc::DevnetRpcClient,
};
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
        .with_l1_chain_id(1337)
        .with_l2_chain_id(84538453)
        .with_output_dir(devnet_dir.clone())
        .with_stable_config()
        .build()
        .await?;

    let l1_rpc = devnet.l1_rpc_url().await?;
    let l2_builder_rpc = devnet.l2_rpc_url()?;
    let l2_client_rpc = devnet.l2_client_rpc_url()?;
    let l2_builder_op_rpc = devnet.l2_stack().op_node().rpc_url().await?;
    let l2_client_op_rpc = "http://localhost:8549".to_string();

    let urls = DevnetUrls {
        l1_rpc: l1_rpc.to_string(),
        l2_builder_rpc: l2_builder_rpc.to_string(),
        l2_client_rpc: l2_client_rpc.to_string(),
        l2_builder_op_rpc: l2_builder_op_rpc.to_string(),
        l2_client_op_rpc,
    };
    fs::write(devnet_dir.join("urls.json"), serde_json::to_string_pretty(&urls)?)?;

    println!("\nDevnet started successfully!");
    println!("\nRPC URLs:");
    println!("  L1:          {l1_rpc}");
    println!("  L2 Builder:  {l2_builder_rpc}");
    println!("  L2 Client:   {l2_client_rpc}");
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
    use system_tests::config::{ANVIL_ACCOUNT_1, ANVIL_ACCOUNT_2};

    if !is_devnet_running()? {
        eprintln!("Error: Devnet is not running. Start it with 'devnet start'");
        std::process::exit(1);
    }

    let urls = read_urls()?;

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

#[derive(serde::Deserialize, serde::Serialize)]
struct DevnetUrls {
    l1_rpc: String,
    l2_builder_rpc: String,
    l2_client_rpc: String,
    l2_builder_op_rpc: String,
    l2_client_op_rpc: String,
}

fn read_urls() -> Result<DevnetUrls> {
    let path = PathBuf::from(".devnet/urls.json");
    let content = fs::read_to_string(&path)
        .wrap_err("Failed to read .devnet/urls.json - is devnet running?")?;
    serde_json::from_str(&content).wrap_err("Failed to parse urls.json")
}

async fn status_devnet() -> Result<()> {
    if !is_devnet_running()? {
        eprintln!("Error: Devnet is not running. Start it with 'devnet start'");
        std::process::exit(1);
    }

    let urls = read_urls()?;
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

    let builder_unsafe =
        builder_status.unsafe_l2.map_or_else(|| "N/A".to_string(), |b| b.number.to_string());
    let builder_safe =
        builder_status.safe_l2.map_or_else(|| "N/A".to_string(), |b| b.number.to_string());
    let client_unsafe =
        client_status.unsafe_l2.map_or_else(|| "N/A".to_string(), |b| b.number.to_string());
    let client_safe =
        client_status.safe_l2.map_or_else(|| "N/A".to_string(), |b| b.number.to_string());

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

    let urls = read_urls()?;
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

    use system_tests::config::{
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

fn format_ether(wei: U256) -> String {
    let wei_u128: u128 = wei.try_into().unwrap_or(u128::MAX);
    let eth = wei_u128 as f64 / 1e18;
    format!("{eth:.4}")
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
