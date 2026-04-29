//! Tests that EIP-7702 delegated accounts can have multiple inflight transactions in the txpool.

use std::time::Duration;

use alloy_consensus::{SignableTransaction, TxEip7702};
use alloy_eips::{eip2718::Encodable2718, eip7702::Authorization};
use alloy_network::{ReceiptResponse, TransactionBuilder};
use alloy_primitives::{Address, U256};
use alloy_provider::Provider;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_rpc_types::BaseTransactionRequest;
use devnet::{DevnetBuilder, config::ANVIL_ACCOUNT_1};
use eyre::Result;
use tokio::time::{sleep, timeout};

const L2_CHAIN_ID: u64 = 84538453;
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
        .with_max_inflight_delegated_slots(4)
        .build()
        .await?;

    let builder_provider = devnet.l2_builder_provider()?;
    let client_provider = devnet.l2_client_provider()?;

    let private_key_hex = format!("0x{}", hex::encode(ANVIL_ACCOUNT_1.private_key.as_slice()));
    let signer: PrivateKeySigner = private_key_hex.parse()?;
    let sender = signer.address();

    // Wait for the builder to have the sender's balance before submitting anything.
    timeout(Duration::from_secs(30), async {
        loop {
            if builder_provider.get_balance(sender).await? > U256::ZERO {
                return Ok::<_, eyre::Error>(());
            }
            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await
    .map_err(|_| eyre::eyre!("sender {sender} had no balance on builder after 30s"))??;

    // --- Step 1: delegate the EOA via EIP-7702 ---
    // Delegate to a non-zero address — delegating to Address::ZERO is a reset
    // and produces empty code, not the 0xef0100 prefix. Any non-zero target
    // (even one with no deployed code) sets the prefix the txpool checks for.
    let delegation_target: Address = "0x0000000000000000000000000000000000000001".parse()?;
    // Get nonce from builder since we're submitting the delegation tx there.
    let nonce = builder_provider.get_transaction_count(sender).await?;
    // When sender == authority (self-delegation), the sender's nonce is incremented
    // by deduct_caller before apply_eip7702_auth_list runs. So the authorization
    // nonce must be nonce+1, not nonce.
    let auth_nonce = nonce + 1;
    let auth = Authorization {
        chain_id: U256::from(L2_CHAIN_ID),
        address: delegation_target,
        nonce: auth_nonce,
    };
    let auth_sig_hash = auth.signature_hash();
    let auth = auth.into_signed(signer.sign_hash_sync(&auth_sig_hash)?);

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
    // Send to builder directly so it's included without needing tx forwarding.
    let delegation_pending = builder_provider.send_raw_transaction(&setup_raw).await?;
    let delegation_hash = *delegation_pending.tx_hash();

    // Wait for the delegation tx receipt, then verify the 0xef0100 code.
    let delegation_receipt = timeout(Duration::from_secs(60), async {
        loop {
            if let Some(receipt) = builder_provider.get_transaction_receipt(delegation_hash).await?
            {
                return Ok::<_, eyre::Error>(receipt);
            }
            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await
    .map_err(|_| eyre::eyre!("delegation tx {} not included after 60s", delegation_hash))??;

    if !delegation_receipt.status() {
        return Err(eyre::eyre!("delegation tx failed: {:?}", delegation_receipt));
    }

    let code = builder_provider.get_code_at(sender).await?;
    if !code.starts_with(&[0xef, 0x01, 0x00]) {
        return Err(eyre::eyre!("expected 0xef0100 prefix, got: {:?}", code));
    }

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
    .await
    .map_err(|_| eyre::eyre!("client did not see 0xef0100 delegation code after 30s"))??;

    // --- Step 2: send 4 inflight transactions from the now-delegated account ---
    // Pre-sign all transactions before submitting any. Signing takes ~1ms per tx;
    // if done inline the 2s block timer could fire between sends and include an
    // earlier tx, reducing the simultaneous inflight count below 4 and letting the
    // test pass even with a limit of 1.
    let base_nonce = client_provider.get_transaction_count(sender).await?;
    let recipient: Address = "0x000000000000000000000000000000000000dEaD".parse()?;

    let raw_txs: Vec<alloy_primitives::Bytes> = (0..4u64)
        .map(|i| -> Result<alloy_primitives::Bytes> {
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
            let tx = tx_request
                .build_typed_tx()
                .map_err(|e| eyre::eyre!("invalid tx request: {e:?}"))?;
            let sig = signer.sign_hash_sync(&tx.signature_hash())?;
            Ok(tx.into_signed(sig).encoded_2718().into())
        })
        .collect::<Result<_>>()?;

    // Submit all 4 concurrently so they arrive at the pool at the same time.
    // This prevents the scenario where sequential sends allow a block to be
    // produced between requests, including tx N before tx N+1 is submitted and
    // making a limit-of-1 pool appear to accept all four.
    tokio::try_join!(
        async {
            client_provider
                .send_raw_transaction(&raw_txs[0])
                .await
                .map(drop)
                .map_err(|e| eyre::eyre!("tx nonce={} rejected: {}", base_nonce, e))
        },
        async {
            client_provider
                .send_raw_transaction(&raw_txs[1])
                .await
                .map(drop)
                .map_err(|e| eyre::eyre!("tx nonce={} rejected: {}", base_nonce + 1, e))
        },
        async {
            client_provider
                .send_raw_transaction(&raw_txs[2])
                .await
                .map(drop)
                .map_err(|e| eyre::eyre!("tx nonce={} rejected: {}", base_nonce + 2, e))
        },
        async {
            client_provider
                .send_raw_transaction(&raw_txs[3])
                .await
                .map(drop)
                .map_err(|e| eyre::eyre!("tx nonce={} rejected: {}", base_nonce + 3, e))
        },
    )?;

    Ok(())
}
