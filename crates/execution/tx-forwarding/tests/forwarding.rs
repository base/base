//! Public-boundary integration test for forwarding pool transactions to independent builders.

use std::{sync::Arc, time::Duration};

use alloy_consensus::SignableTransaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_network::TransactionBuilder;
use alloy_primitives::Bytes;
use alloy_provider::Provider;
use alloy_signer::SignerSync;
use base_common_rpc_types::BaseTransactionRequest;
use base_execution_chainspec::BaseChainSpec;
use base_execution_txpool::{
    DEFAULT_MAX_VALIDITY_PREDICATES, TransactionValidity, ValidatedTransaction,
};
use base_node_runner::test_utils::TestHarness;
use base_test_utils::{Account, DEVNET_CHAIN_ID, build_test_genesis, build_test_genesis_cobalt};
use base_tx_forwarding::{TxForwardingConfig, TxForwardingExtension};
use base_txpool_rpc::{SendRawTransactionValidityExtension, SendRawTransactionValidityRequest};
use eyre::{Result, WrapErr};
use jsonrpsee::{
    RpcModule,
    server::{ServerBuilder, ServerHandle},
};
use tokio::{
    sync::{Barrier, Notify, mpsc},
    time::timeout,
};
use url::Url;

const WAIT_TIMEOUT: Duration = Duration::from_secs(10);
const ISOLATION_TIMEOUT: Duration = Duration::from_millis(500);

struct MockBuilder {
    url: Url,
    handle: ServerHandle,
}

impl MockBuilder {
    async fn spawn(
        received: mpsc::UnboundedSender<ValidatedTransaction<TransactionValidity>>,
        entered: Option<Arc<Barrier>>,
        release: Option<Arc<Notify>>,
    ) -> Result<Self> {
        let server = ServerBuilder::default().build("127.0.0.1:0").await?;
        let address = server.local_addr()?;
        let mut module = RpcModule::new((received, entered, release));
        module.register_async_method(
            "base_insertValidatedTransaction",
            |params, context, _| async move {
                let transaction: ValidatedTransaction<TransactionValidity> = params.one()?;
                context.0.send(transaction).expect("test receiver should remain open");

                if let (Some(entered), Some(release)) = (&context.1, &context.2) {
                    entered.wait().await;
                    release.notified().await;
                }

                Ok::<(), jsonrpsee::types::ErrorObjectOwned>(())
            },
        )?;
        let handle = server.start(module);
        let url = Url::parse(&format!("http://{address}"))?;

        Ok(Self { url, handle })
    }

    async fn shutdown(self) -> Result<()> {
        self.handle.stop().map_err(|error| eyre::eyre!("failed to stop mock server: {error:?}"))?;
        self.handle.stopped().await;
        Ok(())
    }
}

fn signed_eip1559_transaction() -> Bytes {
    let account = Account::Alice;
    let request = BaseTransactionRequest::default()
        .from(account.address())
        .transaction_type(2u8)
        .with_gas_limit(21_000)
        .with_max_fee_per_gas(1_000_000_000)
        .with_max_priority_fee_per_gas(0)
        .with_chain_id(DEVNET_CHAIN_ID)
        .to(Account::Bob.address())
        .with_nonce(0);
    let transaction = request.build_typed_tx().expect("valid transaction request");
    let signature = account
        .signer()
        .sign_hash_sync(&transaction.signature_hash())
        .expect("test account should sign transaction");

    transaction.into_signed(signature).encoded_2718().into()
}

#[tokio::test]
async fn forwards_to_healthy_destination_while_another_destination_is_blocked() -> Result<()> {
    let (healthy_tx, mut healthy_rx) = mpsc::unbounded_channel();
    let healthy = MockBuilder::spawn(healthy_tx, None, None).await?;

    let slow_entered = Arc::new(Barrier::new(2));
    let slow_release = Arc::new(Notify::new());
    let (slow_tx, mut slow_rx) = mpsc::unbounded_channel();
    let slow = MockBuilder::spawn(
        slow_tx,
        Some(Arc::clone(&slow_entered)),
        Some(Arc::clone(&slow_release)),
    )
    .await?;

    // Put the blocked destination first so serialized forwarding cannot satisfy the assertion
    // below before its one-second RPC timeout expires.
    let config = TxForwardingConfig::new(vec![slow.url.clone(), healthy.url.clone()]);
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis()));
    let harness = TestHarness::builder()
        .with_ext::<TxForwardingExtension>(config)
        .with_chain_spec(chain_spec)
        .build()
        .await?;
    let raw = signed_eip1559_transaction();
    let _pending = harness.provider().send_raw_transaction(&raw).await?;

    timeout(WAIT_TIMEOUT, slow_entered.wait()).await.wrap_err("slow destination was not called")?;
    let healthy_forwarded = timeout(ISOLATION_TIMEOUT, healthy_rx.recv())
        .await
        .wrap_err("healthy destination was blocked by the slow destination")?
        .ok_or_else(|| eyre::eyre!("healthy destination channel closed"))?;
    assert_eq!(healthy_forwarded.sender, Account::Alice.address());
    assert_eq!(healthy_forwarded.raw, raw);
    assert!(healthy_forwarded.extensions.validity.is_empty());

    slow_release.notify_one();
    let slow_forwarded = timeout(WAIT_TIMEOUT, slow_rx.recv())
        .await
        .wrap_err("slow destination did not receive the transaction after release")?
        .ok_or_else(|| eyre::eyre!("slow destination channel closed"))?;
    assert_eq!(slow_forwarded.sender, Account::Alice.address());
    assert_eq!(slow_forwarded.raw, raw);
    assert!(slow_forwarded.extensions.validity.is_empty());

    assert!(timeout(Duration::from_millis(100), healthy_rx.recv()).await.is_err());
    assert!(timeout(Duration::from_millis(100), slow_rx.recv()).await.is_err());

    drop(harness);
    healthy.shutdown().await?;
    slow.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn forwards_validity_to_every_builder() -> Result<()> {
    let (first_tx, mut first_rx) = mpsc::unbounded_channel();
    let first = MockBuilder::spawn(first_tx, None, None).await?;
    let (second_tx, mut second_rx) = mpsc::unbounded_channel();
    let second = MockBuilder::spawn(second_tx, None, None).await?;
    let config = TxForwardingConfig::new(vec![first.url.clone(), second.url.clone()]);
    // Validity transactions are fork-gated on Cobalt, so the RPC method only accepts them once the
    // fork is active at the latest block.
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis_cobalt()));
    let harness = TestHarness::builder()
        .with_ext::<SendRawTransactionValidityExtension>(DEFAULT_MAX_VALIDITY_PREDICATES)
        .with_ext::<TxForwardingExtension>(config)
        .with_chain_spec(chain_spec)
        .build()
        .await?;
    let raw = signed_eip1559_transaction();
    let validity = serde_json::from_value(serde_json::json!({
        "type": "storage",
        "params": {
            "address": Account::Bob.address(),
            "slot": "0x1",
            "op": "=",
            "value": "0x2"
        }
    }))?;
    let expected = vec![validity];
    let client = harness.rpc_client()?;
    let _: alloy_primitives::TxHash = client
        .request(
            "base_sendRawTransactionValidity",
            (SendRawTransactionValidityRequest { tx: raw.clone(), validity: expected.clone() },),
        )
        .await?;

    let first_forwarded = timeout(WAIT_TIMEOUT, first_rx.recv())
        .await
        .wrap_err("first builder was not called")?
        .ok_or_else(|| eyre::eyre!("first builder channel closed"))?;
    let second_forwarded = timeout(WAIT_TIMEOUT, second_rx.recv())
        .await
        .wrap_err("second builder was not called")?
        .ok_or_else(|| eyre::eyre!("second builder channel closed"))?;
    for forwarded in [&first_forwarded, &second_forwarded] {
        assert_eq!(forwarded.sender, Account::Alice.address());
        assert_eq!(forwarded.raw, raw);
        assert_eq!(forwarded.extensions.validity, expected);
    }
    assert_eq!(first_forwarded.sender, second_forwarded.sender);
    assert_eq!(first_forwarded.raw, second_forwarded.raw);
    assert!(timeout(Duration::from_millis(100), first_rx.recv()).await.is_err());
    assert!(timeout(Duration::from_millis(100), second_rx.recv()).await.is_err());

    drop(harness);
    first.shutdown().await?;
    second.shutdown().await?;
    Ok(())
}
