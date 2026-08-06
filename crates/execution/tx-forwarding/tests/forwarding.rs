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
use base_execution_txpool::ValidatedTransaction;
use base_node_runner::test_utils::TestHarness;
use base_test_utils::{Account, DEVNET_CHAIN_ID, build_test_genesis};
use base_tx_forwarding::{TxForwardingConfig, TxForwardingExtension};
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
        received: mpsc::UnboundedSender<ValidatedTransaction>,
        entered: Option<Arc<Barrier>>,
        release: Option<Arc<Notify>>,
    ) -> Result<Self> {
        let server = ServerBuilder::default().build("127.0.0.1:0").await?;
        let address = server.local_addr()?;
        let mut module = RpcModule::new((received, entered, release));
        module.register_async_method(
            "base_insertValidatedTransaction",
            |params, context, _| async move {
                let transaction: ValidatedTransaction = params.one()?;
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

    slow_release.notify_one();
    let slow_forwarded = timeout(WAIT_TIMEOUT, slow_rx.recv())
        .await
        .wrap_err("slow destination did not receive the transaction after release")?
        .ok_or_else(|| eyre::eyre!("slow destination channel closed"))?;
    assert_eq!(slow_forwarded.sender, Account::Alice.address());
    assert_eq!(slow_forwarded.raw, raw);

    assert!(timeout(Duration::from_millis(100), healthy_rx.recv()).await.is_err());
    assert!(timeout(Duration::from_millis(100), slow_rx.recv()).await.is_err());

    drop(harness);
    healthy.shutdown().await?;
    slow.shutdown().await?;
    Ok(())
}
