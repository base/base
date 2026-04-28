//! Tests that EIP-7702 delegated accounts can have multiple inflight transactions in the txpool.

use std::time::Duration;

use alloy_consensus::{SignableTransaction, TxEip7702};
use alloy_eips::{eip2718::Encodable2718, eip7702::Authorization};
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, U256};
use alloy_provider::Provider;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_rpc_types::BaseTransactionRequest;
use devnet::{DevnetBuilder, config::ANVIL_ACCOUNT_1};
use eyre::Result;
use tokio::time::{sleep, timeout};

const L2_CHAIN_ID: u64 = 84538453;
// The devnet activates Base Azul (Prague/EIP-7702) at block 20.
const BASE_AZUL_BLOCK: u64 = 20;
const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Verifies that a delegated (EIP-7702) account can have 4 inflight transactions
/// simultaneously in the client mempool.
///
/// Without --rollup.txpool-max-inflight-delegated-slots=4 the second through
/// fourth sends would fail with "in-flight transaction limit reached for
/// delegated accounts".
#[tokio::test]
async fn test_delegated_account_multiple_inflight_txs() -> Result<()> {
    let devnet = DevnetBuilder::new()
        .with_l1_chain_id(1337)
        .with_l2_chain_id(L2_CHAIN_ID)
        .build()
        .await?;

    let builder_provider = devnet.l2_builder_provider()?;
    let client_provider = devnet.l2_client_provider()?;

    // Wait until EIP-7702 is active.
    timeout(Duration::from_secs(120), async {
        loop {
            if builder_provider.get_block_number().await? >= BASE_AZUL_BLOCK {
                return Ok::<_, eyre::Error>(());
            }
            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await??;

    let private_key_hex = format!("0x{}", hex::encode(ANVIL_ACCOUNT_1.private_key.as_slice()));
    let signer: PrivateKeySigner = private_key_hex.parse()?;
    let sender = signer.address();

    // Wait for the client to sync the sender's balance before submitting anything.
    timeout(Duration::from_secs(30), async {
        loop {
            if client_provider.get_balance(sender).await? > U256::ZERO {
                return Ok::<_, eyre::Error>(());
            }
            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await??;

    // --- Step 1: delegate the EOA via EIP-7702 ---
    // Delegate to Address::ZERO — we only need the 0xef0100 code prefix, not a
    // real contract. The txpool checks that prefix to identify delegated senders.
    let nonce = client_provider.get_transaction_count(sender).await?;
    let auth =
        Authorization { chain_id: U256::from(L2_CHAIN_ID), address: Address::ZERO, nonce }
            .into_signed(signer.sign_hash_sync(&Authorization {
                chain_id: U256::from(L2_CHAIN_ID),
                address: Address::ZERO,
                nonce,
            }
            .signature_hash())?);

    let setup_tx = TxEip7702 {
        chain_id: L2_CHAIN_ID,
        nonce,
        gas_limit: 50_000,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: 0,
        to: sender,
        value: U256::ZERO,
        authorization_list: vec![auth],
        ..Default::default()
    };
    let setup_sig = signer.sign_hash_sync(&setup_tx.signature_hash())?;
    let setup_signed = setup_tx.into_signed(setup_sig);
    let setup_raw: alloy_primitives::Bytes = setup_signed.encoded_2718().into();
    let _ = client_provider.send_raw_transaction(&setup_raw).await?;

    // Wait for the delegation tx to be included so the account has 0xef0100 code.
    timeout(Duration::from_secs(60), async {
        loop {
            let code = builder_provider.get_code_at(sender).await?;
            if code.starts_with(&[0xef, 0x01, 0x00]) {
                return Ok::<_, eyre::Error>(());
            }
            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await??;

    // Also wait for the client to see the updated code.
    timeout(Duration::from_secs(30), async {
        loop {
            let code = client_provider.get_code_at(sender).await?;
            if code.starts_with(&[0xef, 0x01, 0x00]) {
                return Ok::<_, eyre::Error>(());
            }
            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await??;

    // --- Step 2: send 4 inflight transactions from the now-delegated account ---
    let base_nonce = client_provider.get_transaction_count(sender).await?;

    let recipient: Address = "0x000000000000000000000000000000000000dEaD".parse()?;
    let mut tx_hashes = Vec::new();
    for i in 0..4u64 {
        let tx_request = BaseTransactionRequest::default()
            .from(sender)
            .to(recipient)
            .value(U256::from(1u64))
            .transaction_type(2)
            .with_gas_limit(21_000)
            .with_max_fee_per_gas(1_000_000_000)
            .with_max_priority_fee_per_gas(0)
            .with_chain_id(L2_CHAIN_ID)
            .with_nonce(base_nonce + i);

        let tx =
            tx_request.build_typed_tx().map_err(|_| eyre::eyre!("invalid tx request"))?;
        let sig = signer.sign_hash_sync(&tx.signature_hash())?;
        let signed = tx.into_signed(sig);
        let hash = *signed.hash();
        let raw: alloy_primitives::Bytes = signed.encoded_2718().into();

        let _ = client_provider
            .send_raw_transaction(&raw)
            .await
            .map_err(|e| eyre::eyre!("tx nonce={} rejected: {}", base_nonce + i, e))?;

        tx_hashes.push(hash);
    }

    assert_eq!(tx_hashes.len(), 4, "all 4 transactions must be accepted by the mempool");
    Ok(())
}
