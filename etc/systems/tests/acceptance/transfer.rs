//! Successful value transfers and hash-pinned safe-chain observations.

use std::time::Duration;

use alloy_consensus::{SignableTransaction, proofs::calculate_transaction_root};
use alloy_eips::eip2718::Encodable2718;
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use base_common_rpc_types::BaseTransactionRequest;
use base_consensus_rpc::SyncStatusApiClient;
use base_protocol::L2BlockInfo;
use base_system_tests::{ANVIL_ACCOUNT_1, SystemTestProviderExt};
use eyre::{Result, WrapErr, ensure};
use jsonrpsee::http_client::HttpClient;
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};

use super::{batch::TransferTarget, header::AuthenticatedHeader, rpc::Rpc};

/// Identifiable recipient and exact value for a scenario's signed value transfer.
#[derive(Clone, Copy, Debug)]
pub struct TransferRequest {
    /// Recipient not otherwise used by the fresh fixture.
    pub recipient: Address,
    /// Exact wei value expected in the receipt block's balance delta.
    pub value: u64,
}

/// A successful transfer together with the exact block and state changes it produced.
#[derive(Debug)]
pub struct Transfer {
    /// Signed transaction bytes later recovered from an L1 batch.
    pub raw_transaction: Bytes,
    /// Hash returned by both signing and RPC submission.
    pub transaction_hash: B256,
    /// Identifiable recipient unique to this transfer in fresh chain state.
    pub recipient: Address,
    /// Exact transferred value.
    pub value: U256,
    /// Execution receipt from the sequencer.
    pub receipt: Value,
    /// Authenticated containing L2 header.
    pub block: AuthenticatedHeader,
    /// Authenticated parent L2 header used to anchor the initial balance.
    pub parent: AuthenticatedHeader,
    /// L1 origin obtained from the containing block's deposit, not its wall clock.
    pub l1_origin: AuthenticatedHeader,
    /// Recipient balance at the receipt block's parent hash.
    pub balance_before: U256,
    /// Recipient balance at the receipt block hash.
    pub balance_after: U256,
}

impl Transfer {
    /// Sends a signed value transfer through the real sequencer RPC and waits for a receipt.
    pub async fn send(
        rpc: &Rpc,
        provider: &RootProvider<Base>,
        sequencer_url: &str,
        l1_url: &str,
        rollup: &RollupConfig,
        transfer: TransferRequest,
        within: Duration,
    ) -> Result<Self> {
        let observations = async {
            let signer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_1.private_key)?;
            let request = BaseTransactionRequest::default()
                .from(signer.address())
                .to(transfer.recipient)
                .value(U256::from(transfer.value))
                .transaction_type(2)
                .with_gas_limit(21_000)
                .with_max_fee_per_gas(1_000_000_000)
                .with_max_priority_fee_per_gas(0)
                .with_chain_id(rollup.l2_chain_id.id())
                .with_nonce(provider.get_transaction_count(signer.address()).await?);
            let transaction =
                request.build_typed_tx().map_err(|_| eyre::eyre!("invalid transfer request"))?;
            let signature = signer.sign_hash_sync(&transaction.signature_hash())?;
            let signed = transaction.into_signed(signature);
            let raw_transaction: Bytes = signed.encoded_2718().into();
            let transaction_hash = *signed.hash();
            let submitted = provider.send_raw_transaction(&raw_transaction).await?;
            ensure!(*submitted.tx_hash() == transaction_hash, "submitted transfer hash mismatch");
            provider
                .wait_for_receipt(transaction_hash, Duration::from_secs(60))
                .await
                .wrap_err_with(|| {
                    format!(
                        "transfer {transaction_hash} to {} for {} wei",
                        transfer.recipient, transfer.value
                    )
                })?;
            let receipt = rpc
                .call(sequencer_url, "eth_getTransactionReceipt", json!([transaction_hash]))
                .await?;
            ensure!(
                Rpc::quantity(&receipt["status"])? == 1
                    && receipt["transactionHash"] == json!(transaction_hash),
                "transfer receipt failed or returned the wrong transaction: {receipt}"
            );
            ensure!(
                receipt["from"] == json!(signer.address())
                    && receipt["to"] == json!(transfer.recipient),
                "transfer receipt address mismatch"
            );
            let height = Rpc::quantity(&receipt["blockNumber"])?;
            let block = rpc.header(sequencer_url, json!(format!("{height:#x}"))).await?;
            ensure!(
                receipt["blockHash"] == json!(block.hash),
                "receipt is not in canonical containing block"
            );
            let parent = rpc
                .header(
                    sequencer_url,
                    json!(format!(
                        "{:#x}",
                        height
                            .checked_sub(1)
                            .ok_or_else(|| eyre::eyre!("transfer included in genesis"))?
                    )),
                )
                .await?;
            ensure!(block.header.parent_hash == parent.hash, "transfer parent hash mismatch");
            let full = provider
                .get_block_by_hash(block.hash)
                .full()
                .await?
                .ok_or_else(|| eyre::eyre!("transfer block missing"))?
                .map_header(|header| header.into_inner())
                .into_consensus()
                .map_transactions(|transaction| transaction.inner.inner);
            ensure!(
                calculate_transaction_root(&full.body.transactions)
                    == block.header.transactions_root,
                "receipt block's transaction body does not match its authenticated root"
            );
            ensure!(
                full.body
                    .transactions
                    .iter()
                    .any(|transaction| transaction.encoded_2718().as_slice()
                        == raw_transaction.as_ref()),
                "signed transfer is absent from its authenticated receipt block"
            );
            let info = L2BlockInfo::from_block_and_genesis(&full, &rollup.genesis)?;
            let l1_origin =
                rpc.header(l1_url, json!(format!("{:#x}", info.l1_origin.number))).await?;
            ensure!(info.l1_origin.hash == l1_origin.hash, "L2 origin differs from canonical L1");
            let balance_before =
                rpc.balance(sequencer_url, transfer.recipient, parent.hash).await?;
            let balance_after = rpc.balance(sequencer_url, transfer.recipient, block.hash).await?;
            ensure!(
                balance_after == balance_before + U256::from(transfer.value),
                "recipient balance did not increase by exact transfer value"
            );
            Ok(Self {
                raw_transaction,
                transaction_hash,
                recipient: transfer.recipient,
                value: U256::from(transfer.value),
                receipt,
                block,
                parent,
                l1_origin,
                balance_before,
                balance_after,
            })
        };
        timeout(within, observations).await.wrap_err_with(|| {
            format!(
                "transfer submission and receipt-block observations timed out for {} to {} after {within:?}",
                transfer.value, transfer.recipient
            )
        })?
    }

    /// The batch evidence must recover this transfer in its actual timestamp and origin.
    pub fn target(&self) -> TransferTarget {
        TransferTarget {
            raw_transaction: self.raw_transaction.clone(),
            timestamp: self.block.header.timestamp,
            l1_origin_number: self.l1_origin.header.number,
        }
    }

    /// Requires both nodes' safe heads to cover the receipt block, then checks that exact hash/state.
    pub async fn wait_until_safe(
        &self,
        rpc: &Rpc,
        sequencer: &RootProvider<Base>,
        verifier: &RootProvider<Base>,
        consensus: [&HttpClient; 2],
        verifier_url: &str,
    ) -> Result<Value> {
        let mut last = json!(null);
        let height = self.block.header.number;
        let result = timeout(Duration::from_secs(120), async {
            loop {
                let sequencer_status = consensus[0].sync_status().await?;
                let verifier_status = consensus[1].sync_status().await?;
                last = json!({"sequencer": sequencer_status, "verifier": verifier_status});
                if sequencer_status.safe_l2.block_info.number >= height && verifier_status.safe_l2.block_info.number >= height {
                    let sequencer_safe = sequencer
                        .block_hash_at(sequencer_status.safe_l2.block_info.number)
                        .await?;
                    let verifier_safe = verifier
                        .block_hash_at(verifier_status.safe_l2.block_info.number)
                        .await?;
                    ensure!(
                        sequencer_safe == Some(sequencer_status.safe_l2.block_info.hash),
                        "sequencer consensus safe head is not EL canonical"
                    );
                    ensure!(
                        verifier_safe == Some(verifier_status.safe_l2.block_info.hash),
                        "verifier consensus safe head is not EL canonical"
                    );
                    verifier.wait_for_convergence(sequencer, height, Duration::from_secs(10)).await?;
                    let sequencer_hash = sequencer.block_hash_at(height).await?;
                    let verifier_hash = verifier.block_hash_at(height).await?;
                    ensure!(sequencer_hash == Some(self.block.hash), "sequencer replaced the transfer block before safety");
                    ensure!(verifier_hash == Some(self.block.hash), "safe verifier transfer block hash mismatch");
                    let receipt = rpc.call(verifier_url, "eth_getTransactionReceipt", json!([self.transaction_hash])).await?;
                    ensure!(Rpc::quantity(&receipt["status"])? == 1 && receipt["blockHash"] == json!(self.block.hash) && Rpc::quantity(&receipt["blockNumber"])? == height, "safe verifier transfer receipt mismatch: {receipt}");
                    ensure!(rpc.balance(verifier_url, self.recipient, self.parent.hash).await? == self.balance_before, "verifier parent balance mismatch");
                    ensure!(rpc.balance(verifier_url, self.recipient, self.block.hash).await? == self.balance_after, "safe verifier recipient balance mismatch");
                    return Ok::<_, eyre::Report>(json!({"number": height, "hash": self.block.hash, "sequencer_hash": sequencer_hash, "verifier_hash": verifier_hash, "sequencer_status": sequencer_status, "verifier_status": verifier_status, "verifier_receipt": receipt, "recipient_balance": self.balance_after}));
                }
                sleep(Duration::from_millis(500)).await;
            }
        }).await;
        result.wrap_err_with(|| format!("safe derivation timed out for transfer {} in block {height} ({}); last statuses: {last}", self.transaction_hash, self.block.hash))?
    }
}

#[cfg(test)]
mod tests {
    //! A stalled HTTP peer exercises the real Alloy provider; no internal trait is faked.

    use std::{future::pending, time::Duration};

    use alloy_primitives::Address;
    use alloy_provider::RootProvider;
    use base_common_genesis::RollupConfig;
    use base_common_network::Base;
    use tokio::{io::AsyncReadExt, net::TcpListener, sync::oneshot, time::timeout};

    use super::{Rpc, Transfer, TransferRequest};

    #[tokio::test]
    async fn transfer_deadline_covers_a_stalled_typed_nonce_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (observed, received) = oneshot::channel();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0; 1024];
            loop {
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0, "client disconnected before the nonce request");
                bytes.extend_from_slice(&chunk[..count]);
                if String::from_utf8_lossy(&bytes).contains("eth_getTransactionCount") {
                    break;
                }
            }
            observed.send(()).unwrap();
            // Keep the connection alive but deliberately never return a response.
            pending::<()>().await;
            drop(stream);
        });
        let provider = RootProvider::<Base>::new_http(url.parse().unwrap());
        let result = timeout(
            Duration::from_secs(2),
            Transfer::send(
                &Rpc::new().unwrap(),
                &provider,
                &url,
                &url,
                &RollupConfig::default(),
                TransferRequest { recipient: Address::repeat_byte(0x73), value: 1_337 },
                Duration::from_millis(100),
            ),
        )
        .await;
        peer.abort();
        let error = result.expect("transfer ignored its own deadline").unwrap_err();
        timeout(Duration::from_secs(1), received).await.unwrap().unwrap();
        assert!(
            error
                .to_string()
                .contains("transfer submission and receipt-block observations timed out")
        );
    }
}
