//! Smoke tests for the full `SystemTestStack` stack.

use std::{net::TcpListener, process::Command, time::Duration};

use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer::SignerSync;
use base_common_consensus::SignableTransaction;
use base_common_genesis::RollupConfig;
use base_common_network::{Base, Ethereum, PrivateKeySigner, ReceiptResponse, TransactionBuilder};
use base_common_rpc_types::BaseTransactionRequest;
use base_system_tests::{ANVIL_ACCOUNT_1, SEQUENCER, SetupImage, SystemTestStackBuilder};
use eyre::{Result, WrapErr};
use tokio::time::{sleep, timeout};

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const BLOCK_PRODUCTION_TIMEOUT: Duration = Duration::from_secs(30);
const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);
const TX_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);

static SMOKE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn denim_and_zenith_activation_matches_el_and_cl_configs() -> Result<()> {
    const AZUL_ACTIVATION_BLOCK: u64 = 20;
    const DENIM_ACTIVATION_BLOCK: u64 = 25;
    const ZENITH_ACTIVATION_BLOCK: u64 = 100;

    let _guard = SMOKE_TEST_LOCK.lock().await;
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_base_azul_activation_block(AZUL_ACTIVATION_BLOCK)
        .with_base_denim_activation_block(DENIM_ACTIVATION_BLOCK)
        .with_base_zenith_activation_block(ZENITH_ACTIVATION_BLOCK)
        .build()
        .await?;

    let genesis: serde_json::Value = serde_json::from_str(&system.l2_deployment().read_genesis()?)?;
    let rollup_json = system.l2_deployment().read_rollup_config()?;
    let rollup: serde_json::Value = serde_json::from_str(&rollup_json)?;
    let rollup_config: RollupConfig = serde_json::from_str(&rollup_json)?;
    let expected_azul =
        rollup_config.genesis.l2_time + rollup_config.block_time * AZUL_ACTIVATION_BLOCK;
    let expected_denim = rollup_config.l2_block_timestamp(DENIM_ACTIVATION_BLOCK);
    let expected_zenith = rollup_config.l2_block_timestamp(ZENITH_ACTIVATION_BLOCK);
    assert_eq!(rollup_config.l2_block_timestamp_parts(ZENITH_ACTIVATION_BLOCK).1, 0);

    assert_eq!(rollup["base"]["azul"].as_u64(), Some(expected_azul));
    assert_eq!(genesis["config"]["base"]["azul"].as_u64(), Some(expected_azul));
    assert_eq!(rollup["base"]["denim"].as_u64(), Some(expected_denim));
    assert_eq!(genesis["config"]["base"]["denim"].as_u64(), Some(expected_denim));

    // Zenith is the genesis-only gate for future hardfork feature testing; the devnet setup
    // must be able to schedule it through genesis config.
    assert_eq!(rollup["base"]["zenith"].as_u64(), Some(expected_zenith));
    assert_eq!(genesis["config"]["base"]["zenith"].as_u64(), Some(expected_zenith));

    Ok(())
}

#[test]
fn rejects_post_denim_block_without_whole_second_timestamp() {
    SetupImage::ensure_built().unwrap();
    let output = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--network",
            "none",
            "--entrypoint",
            "op-deployer",
            "-e",
            "SEQUENCER_ADDR=0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc",
            "devnet-setup:local-v2",
            "--denim-block",
            "25",
            "--zenith-block",
            "26",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("zenith must align to a whole second after Denim")
    );
}

#[tokio::test]
async fn smoke_test_system_block_production_and_transactions() -> Result<()> {
    let _guard = SMOKE_TEST_LOCK.lock().await;
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .build()
        .await?;

    let l1_provider = system.l1_provider().await?;
    let l2_builder_provider = system.l2_builder_provider()?;
    let l2_client_provider = system.l2_client_provider()?;

    verify_l1_block_production(&l1_provider).await?;
    verify_l2_block_production(&l2_builder_provider).await?;
    send_l2_transaction_via_client(&l2_client_provider, &l2_builder_provider).await?;

    Ok(())
}

async fn verify_l1_block_production(provider: &RootProvider<Ethereum>) -> Result<()> {
    let initial_block = provider.get_block_number().await?;

    let result = timeout(BLOCK_PRODUCTION_TIMEOUT, async {
        loop {
            sleep(BLOCK_POLL_INTERVAL).await;
            let current_block = provider.get_block_number().await?;
            if current_block > initial_block {
                return Ok::<_, eyre::Error>(current_block);
            }
        }
    })
    .await
    .wrap_err("L1 block production timed out")??;

    assert!(result > initial_block, "L1 should produce new blocks");
    println!("L1 block height: {initial_block} -> {result}");
    Ok(())
}

async fn verify_l2_block_production(provider: &RootProvider<Base>) -> Result<()> {
    let initial_block = provider.get_block_number().await?;

    let result = timeout(BLOCK_PRODUCTION_TIMEOUT, async {
        loop {
            sleep(BLOCK_POLL_INTERVAL).await;
            let current_block = provider.get_block_number().await?;
            if current_block > initial_block {
                return Ok::<_, eyre::Error>(current_block);
            }
        }
    })
    .await
    .wrap_err("L2 block production timed out")??;

    assert!(result > initial_block, "L2 should produce new blocks");
    println!("L2 block height: {initial_block} -> {result}");
    Ok(())
}

async fn send_l2_transaction_via_client(
    client_provider: &RootProvider<Base>,
    builder_provider: &RootProvider<Base>,
) -> Result<()> {
    let private_key_hex = format!("0x{}", hex::encode(ANVIL_ACCOUNT_1.private_key.as_slice()));
    let signer: PrivateKeySigner = private_key_hex.parse()?;
    let sender_address = signer.address();

    let builder_balance = builder_provider.get_balance(sender_address).await?;
    assert!(builder_balance > U256::ZERO, "Sender should have balance on builder");

    timeout(Duration::from_secs(30), async {
        loop {
            let client_balance = client_provider.get_balance(sender_address).await?;
            if client_balance > U256::ZERO {
                return Ok::<_, eyre::Error>(());
            }
            sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .wrap_err("Timed out waiting for client to sync balance")??;

    let nonce = client_provider.get_transaction_count(sender_address).await?;

    let recipient: Address = "0x000000000000000000000000000000000000dEaD".parse()?;
    let tx_request = BaseTransactionRequest::default()
        .from(sender_address)
        .to(recipient)
        .value(U256::from(1_000_000_000u64))
        .transaction_type(2)
        .with_gas_limit(21000)
        .with_max_fee_per_gas(1_000_000_000)
        .with_max_priority_fee_per_gas(0)
        .with_chain_id(L2_CHAIN_ID)
        .with_nonce(nonce);

    let tx = tx_request.build_typed_tx().map_err(|_| eyre::eyre!("invalid transaction request"))?;
    let signature = signer.sign_hash_sync(&tx.signature_hash())?;
    let signed_tx = tx.into_signed(signature);
    let raw_tx: alloy_primitives::Bytes = signed_tx.encoded_2718().into();
    let expected_tx_hash = *signed_tx.hash();

    let pending_tx = client_provider
        .send_raw_transaction(&raw_tx)
        .await
        .wrap_err("Failed to send transaction")?;
    let tx_hash = *pending_tx.tx_hash();
    assert_eq!(tx_hash, expected_tx_hash, "Transaction hash mismatch");

    let receipt = timeout(TX_RECEIPT_TIMEOUT, async {
        loop {
            if let Some(receipt) = builder_provider.get_transaction_receipt(tx_hash).await? {
                return Ok::<_, eyre::Error>(receipt);
            }
            sleep(Duration::from_secs(2)).await;
        }
    })
    .await
    .wrap_err("Transaction receipt timed out on builder")?
    .wrap_err("Failed to get transaction receipt")?;

    assert_eq!(receipt.inner.transaction_hash, tx_hash);
    assert!(receipt.inner.block_number.is_some(), "Receipt should have block number");
    assert_eq!(receipt.inner.from, sender_address);
    assert_eq!(receipt.inner.to, Some(recipient));
    assert!(receipt.status(), "transfer must execute successfully");
    let client_receipt = timeout(TX_RECEIPT_TIMEOUT, async {
        loop {
            if let Some(receipt) = client_provider.get_transaction_receipt(tx_hash).await? {
                return Ok::<_, eyre::Error>(receipt);
            }
            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await
    .wrap_err("client did not import the transaction block")??;
    assert_eq!(client_receipt.inner.block_hash, receipt.inner.block_hash);
    assert_eq!(client_receipt.inner.block_number, receipt.inner.block_number);
    println!(
        "Transfer {tx_hash} included on builder and client in block {:?} ({:?})",
        receipt.inner.block_number, receipt.inner.block_hash
    );

    Ok(())
}

#[tokio::test]
async fn smoke_test_builder_and_client_block_sync() -> Result<()> {
    let _guard = SMOKE_TEST_LOCK.lock().await;
    base_node_runner::test_utils::init_silenced_tracing();
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .build()
        .await?;

    let builder_provider = system.l2_builder_provider()?;
    let client_provider = system.l2_client_provider()?;

    timeout(BLOCK_PRODUCTION_TIMEOUT, async {
        loop {
            let block = builder_provider.get_block_number().await?;
            if block >= 3 {
                return Ok::<_, eyre::Error>(block);
            }
            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await
    .wrap_err("Builder block production timed out")??;

    let client_block = timeout(Duration::from_secs(60), async {
        loop {
            let client_block = client_provider.get_block_number().await?;
            if client_block > 0 {
                return Ok::<_, eyre::Error>(client_block);
            }
            sleep(Duration::from_secs(2)).await;
        }
    })
    .await
    .wrap_err("Client block sync timed out - client stayed at block 0")??;

    assert!(client_block > 0, "Client should have synced at least one block");

    Ok(())
}

/// Runs the shipped executable, including integrated proofs follow mode.
#[tokio::test]
#[ignore = "requires BASE_BINARY pointing to a built base executable and Docker"]
pub async fn smoke_test_unified_binary_produces_and_follows_blocks() -> Result<()> {
    let binary = std::fs::canonicalize(std::env::var("BASE_BINARY")?)?;
    let _guard = SMOKE_TEST_LOCK.lock().await;
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .build()
        .await?;
    let data = tempfile::tempdir()?;
    let mut nodes = Vec::new();
    let mut providers: Vec<String> = Vec::new();
    for role in ["sequencer", "rpc"] {
        let node_dir = data.path().join(role);
        std::fs::create_dir(&node_dir)?;
        let rpc_port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
        let rpc_url = format!("http://127.0.0.1:{rpc_port}");
        if role == "rpc" {
            for command in [vec!["reth", "init"], vec!["proofs", "init"]] {
                let mut init = Command::new(&binary);
                init.current_dir(&node_dir)
                    .args(&command)
                    .arg("--chain")
                    .arg(system.l2_deployment().genesis_path())
                    .arg("--datadir")
                    .arg(&node_dir);
                if command[0] == "proofs" {
                    init.arg("--proofs-history.storage-path").arg(node_dir.join("proofs"));
                }
                eyre::ensure!(init.status()?.success(), "proofs initialization failed");
            }
        }
        let mut command = tokio::process::Command::new(&binary);
        command
            .current_dir(&node_dir)
            .kill_on_drop(true)
            .args(["--chain", "dev", role])
            .arg("--execution-chain")
            .arg(system.l2_deployment().genesis_path())
            .arg("--datadir")
            .arg(&node_dir)
            .args([
                "--http",
                "--http.addr=127.0.0.1",
                "--port=0",
                "--disable-discovery",
                "--rpc.port=0",
                "--p2p.listen.tcp=0",
                "--p2p.listen.udp=0",
                "--l1-slot-duration-override=1",
            ])
            .arg(format!("--http.port={rpc_port}"))
            .arg("--l1-eth-rpc")
            .arg(system.l1_rpc_url().await?.as_str())
            .arg("--l1-beacon")
            .arg(system.l1_stack().beacon_url().await?)
            .arg("--l2-config-file")
            .arg(system.l2_deployment().rollup_config_path())
            .arg("--l1-config-file")
            .arg(system.l1_genesis().el_genesis_path().with_file_name("chain-config.json"));
        if role == "sequencer" {
            command
                .arg("--p2p.sequencer.key")
                .arg(SEQUENCER.private_key.to_string())
                .arg("--sequencer.l1-confs=0");
        } else {
            command
                .arg("--source-l2-rpc")
                .arg(&providers[0])
                .args(["--follow.proofs", "--proofs-history"])
                .arg("--proofs-history.storage-path")
                .arg(node_dir.join("proofs"));
        }
        nodes.push(command.spawn()?);
        let provider = RootProvider::<Base>::new_http(rpc_url.parse()?);
        timeout(Duration::from_secs(90), async {
            loop {
                if provider.get_block_number().await.is_ok_and(|number| number > 0) {
                    return Ok::<_, eyre::Error>(());
                }
                for node in &mut nodes {
                    eyre::ensure!(node.try_wait()?.is_none(), "base process exited during startup");
                }
                sleep(BLOCK_POLL_INTERVAL).await;
            }
        })
        .await
        .wrap_err("unified node did not advance")??;
        providers.push(rpc_url);
    }
    let sequencer = RootProvider::<Base>::new_http(providers[0].parse()?);
    let follower = RootProvider::<Base>::new_http(providers[1].parse()?);
    verify_l1_block_production(&system.l1_provider().await?).await?;
    verify_l2_block_production(&sequencer).await?;
    send_l2_transaction_via_client(&sequencer, &sequencer).await?;
    let height = sequencer.get_block_number().await?;
    timeout(Duration::from_secs(60), async {
        while follower.get_block_number().await? < height {
            sleep(BLOCK_POLL_INTERVAL).await;
        }
        Ok::<_, eyre::Error>(())
    })
    .await??;
    let expected = sequencer.get_block_by_number(height.into()).await?.unwrap();
    let actual = follower.get_block_by_number(height.into()).await?.unwrap();
    assert_eq!(expected.header.hash, actual.header.hash);
    println!(
        "Unified sequencer and proofs follower agree at block {height}: {}",
        actual.header.hash
    );
    for mut node in nodes {
        node.kill().await?;
    }
    Ok(())
}
