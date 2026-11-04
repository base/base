//! Public-send nonce isolation across preparation failures. A scripted HTTP provider
//! records signed envelopes and controls receipts while sibling requests are live.
mod common;

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use alloy_consensus::{Transaction, TxEnvelope};
use alloy_eips::Decodable2718;
use alloy_network::EthereumWallet;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::RootProvider;
use alloy_signer_local::PrivateKeySigner;
use base_tx_manager::{
    NoopTxMetrics, SimpleTxManager, TxCandidate, TxManager, TxManagerConfig, TxManagerError,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[derive(Default)]
struct Script {
    accepted: Mutex<Vec<(u64, B256)>>,
    conflicts: AtomicBool,
    fail: AtomicBool,
    mined: AtomicBool,
}
impl Script {
    fn reply(&self, request: &Value) -> Value {
        let method = request["method"].as_str().unwrap();
        let result = match method {
            "eth_chainId" => json!("0x1"),
            "eth_getTransactionCount" => json!("0x0"),
            "eth_maxPriorityFeePerGas" => json!("0x3b9aca00"),
            "eth_getBlockByNumber" => {
                json!({"hash": B256::with_last_byte(8), "number":"0x1", "parentHash": B256::ZERO,
                "sha3Uncles":B256::ZERO,"miner":Address::ZERO,"stateRoot":B256::ZERO,"transactionsRoot":B256::ZERO,
                "receiptsRoot":B256::ZERO,"logsBloom":format!("0x{}", "00".repeat(256)),"difficulty":"0x0",
                "gasLimit":"0x1c9c380","gasUsed":"0x0","timestamp":"0x1","extraData":"0x","mixHash":B256::ZERO,
                "nonce":"0x0000000000000000","baseFeePerGas":"0x3b9aca00","transactions":[],"uncles":[]})
            }
            "eth_estimateGas" if self.fail.load(Ordering::SeqCst) => {
                return json!({"jsonrpc":"2.0", "id":request["id"],
                "error":{"code":3,"message":"execution reverted"}});
            }
            "eth_estimateGas" => json!("0x5208"),
            "eth_sendRawTransaction" => {
                let raw: Bytes = serde_json::from_value(request["params"][0].clone()).unwrap();
                let tx = TxEnvelope::decode_2718(&mut raw.as_ref()).unwrap();
                let mut accepted = self.accepted.lock().unwrap();
                if accepted
                    .iter()
                    .any(|(nonce, hash)| *nonce == tx.nonce() && *hash != *tx.tx_hash())
                {
                    self.conflicts.store(true, Ordering::SeqCst);
                    return json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":"replacement transaction underpriced"}});
                }
                accepted.push((tx.nonce(), *tx.tx_hash()));
                json!(tx.tx_hash())
            }
            "eth_blockNumber" => json!("0x2"),
            "eth_getTransactionReceipt" if self.mined.load(Ordering::SeqCst) => json!({
                "transactionHash":request["params"][0],"transactionIndex":"0x0","blockHash":B256::with_last_byte(8),
                "blockNumber":"0x1","from":Address::ZERO,"to":Address::with_last_byte(42),"cumulativeGasUsed":"0x5208",
                "gasUsed":"0x5208","effectiveGasPrice":"0x3b9aca00","contractAddress":null,"logs":[],
                "logsBloom":format!("0x{}", "00".repeat(256)),"status":"0x1","type":"0x2"}),
            "eth_getTransactionReceipt" => Value::Null,
            _ => panic!("unexpected method {method}"),
        };
        json!({"jsonrpc":"2.0","id":request["id"],"result":result})
    }
}
async fn server(script: Arc<Script>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let script = Arc::clone(&script);
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let (start, len) = loop {
                    let mut buf = [0; 4096];
                    let n = stream.read(&mut buf).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                        let len: usize = headers
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length: "))
                            .unwrap()
                            .parse()
                            .unwrap();
                        if bytes.len() >= end + 4 + len {
                            break (end + 4, len);
                        }
                    }
                };
                let req: Value = serde_json::from_slice(&bytes[start..start + len]).unwrap();
                let body = script.reply(&req).to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            });
        }
    });
    (url, task)
}
async fn wait_count(script: &Script, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while script.accepted.lock().unwrap().len() < count {
            assert!(
                !script.conflicts.load(Ordering::SeqCst),
                "live sibling nonce reused by distinct signed candidates"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("publication barrier");
}
async fn schedule(first_async: bool, next_async: bool, fail: bool, recycled: bool) {
    let script = Arc::new(Script::default());
    let (url, server) = server(Arc::clone(&script)).await;
    let manager = Arc::new(
        SimpleTxManager::from_wallet(
            RootProvider::new_http(url.parse().unwrap()),
            EthereumWallet::from(PrivateKeySigner::random()),
            TxManagerConfig {
                receipt_query_interval: Duration::from_millis(20),
                resubmission_timeout: Duration::from_secs(60),
                tx_send_timeout: Duration::from_secs(15),
                num_confirmations: 1,
                ..Default::default()
            },
            1,
            Arc::new(NoopTxMetrics),
        )
        .await
        .unwrap(),
    );
    if recycled {
        let nonce = manager.nonce_manager().reserve_nonce().await.unwrap();
        manager.nonce_manager().return_reserved_nonce(nonce).await;
    }
    let candidate = TxCandidate { to: Some(Address::with_last_byte(42)), ..Default::default() };
    let a = {
        let m = Arc::clone(&manager);
        let c = candidate.clone();
        tokio::spawn(async move {
            if first_async { m.send_async(c).await.await } else { m.send(c).await }
        })
    };
    wait_count(&script, 1).await;
    script.fail.store(fail, Ordering::SeqCst);
    if fail {
        let error = if next_async {
            manager.send_async(candidate.clone()).await.await
        } else {
            manager.send(candidate.clone()).await
        };
        assert!(matches!(error, Err(TxManagerError::ExecutionReverted { .. })), "{error:?}");
    }
    script.fail.store(false, Ordering::SeqCst);
    let candidate = TxCandidate { value: U256::from(2), ..candidate };
    let c = {
        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            if next_async { m.send_async(candidate).await.await } else { m.send(candidate).await }
        })
    };
    wait_count(&script, 2).await;
    let accepted = script.accepted.lock().unwrap().clone();
    assert_ne!(accepted[0].1, accepted[1].1, "candidates must differ");
    script.mined.store(true, Ordering::SeqCst);
    let ar = a.await.unwrap().expect("original confirms");
    let cr = c.await.unwrap().expect("third confirms");
    assert_eq!(ar.transaction_hash, accepted[0].1);
    assert_eq!(cr.transaction_hash, accepted[1].1);
    server.abort();
    assert_ne!(accepted[0].0, accepted[1].0, "live sibling nonce reused");
}
#[tokio::test]
async fn send_failure() {
    schedule(false, false, true, false).await;
}
#[tokio::test]
async fn send_control() {
    schedule(false, false, false, false).await;
}
#[tokio::test]
async fn async_failure() {
    schedule(true, true, true, false).await;
}
#[tokio::test]
async fn async_control() {
    schedule(true, true, false, false).await;
}

#[tokio::test]
async fn ordinary_then_async_failure() {
    schedule(false, true, true, false).await;
}
#[tokio::test]
async fn async_then_ordinary_failure() {
    schedule(true, false, true, false).await;
}

#[tokio::test]
async fn recycled_ordinary_nonce_survives_reset() {
    schedule(false, false, true, true).await;
}

#[tokio::test]
async fn failed_signing_keeps_nonce_reusable_after_reset() {
    let script = Arc::new(Script::default());
    let (url, server) = server(Arc::clone(&script)).await;
    let wallet =
        EthereumWallet::from(common::FailingSigner { address: Address::with_last_byte(1) });
    let manager = SimpleTxManager::from_wallet(
        RootProvider::new_http(url.parse().unwrap()),
        wallet,
        TxManagerConfig::default(),
        1,
        Arc::new(NoopTxMetrics),
    )
    .await
    .unwrap();
    for recycled in [false, true] {
        if recycled {
            let nonce = manager.nonce_manager().reserve_nonce().await.unwrap();
            manager.nonce_manager().return_reserved_nonce(nonce).await;
        }
        let error = manager.send(common::simple_tx_candidate()).await.unwrap_err();
        assert!(matches!(error, TxManagerError::Sign(_)));
        manager.nonce_manager().reset().await;
        let guard = manager.nonce_manager().next_nonce().await.unwrap();
        assert_eq!(guard.nonce(), 0, "failed signing must not consume the nonce");
        guard.rollback();
    }
    assert!(script.accepted.lock().unwrap().is_empty());
    server.abort();
}
