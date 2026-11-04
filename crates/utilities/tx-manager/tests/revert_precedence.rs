//! Public preparation regressions using scripted JSON-RPC estimation failures.

use std::sync::{Arc, Mutex};

use alloy_json_rpc::{ErrorPayload, RpcError};
use alloy_network::EthereumWallet;
use alloy_primitives::{Address, Bytes};
use alloy_provider::RootProvider;
use alloy_rpc_types_eth::Block;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolError, SolType, sol_data::String as SolString};
use alloy_transport::TransportError;
use base_tx_manager::{
    NoopTxMetrics, RpcErrorClassifier, SimpleTxManager, TxCandidate, TxManagerConfig,
    TxManagerError,
};
use rstest::rstest;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const COLLISIONS: [&str; 6] = [
    "replacement transaction underpriced",
    "transaction underpriced",
    "nonce too low",
    "nonce too high",
    "insufficient funds",
    "intrinsic gas too low",
];

fn revert_data(reason: &str) -> Bytes {
    let mut data = alloy_sol_types::Revert::SELECTOR.to_vec();
    data.extend(SolString::abi_encode(&reason.to_string()));
    data.into()
}

#[rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
async fn preparation_preserves_terminal_reverts(#[case] structured: bool) {
    for reason in COLLISIONS.into_iter().chain(["ordinary contract failure"]) {
        let data = revert_data(reason);
        let mut error = json!({"code": 3, "message": format!("execution reverted: {reason}")});
        if structured {
            error["data"] = json!(data);
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let server_calls = calls.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let (body_start, length) = loop {
                    let mut chunk = [0; 4096];
                    let read = socket.read(&mut chunk).await.unwrap();
                    assert!(read > 0);
                    request.extend_from_slice(&chunk[..read]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = std::str::from_utf8(&request[..end]).unwrap();
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap();
                        if request.len() >= end + 4 + length {
                            break (end + 4, length);
                        }
                    }
                };
                let request: Value =
                    serde_json::from_slice(&request[body_start..body_start + length]).unwrap();
                let method = request["method"].as_str().unwrap();
                server_calls.lock().unwrap().push(method.to_string());
                let mut response = json!({"jsonrpc": "2.0", "id": request["id"]});
                match method {
                    "eth_chainId" => response["result"] = json!("0x1"),
                    "eth_maxPriorityFeePerGas" => response["result"] = json!("0x3b9aca00"),
                    "eth_getBlockByNumber" => {
                        let block: Block = Block::default();
                        let mut block = serde_json::to_value(block).unwrap();
                        block["baseFeePerGas"] = json!("0x3b9aca00");
                        response["result"] = block;
                    }
                    "eth_estimateGas" => response["error"] = error.clone(),
                    other => panic!("unexpected RPC method: {other}"),
                }
                let body = response.to_string();
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        let wallet = EthereumWallet::from(PrivateKeySigner::random());
        let manager = SimpleTxManager::from_wallet(
            RootProvider::new_http(endpoint.parse().unwrap()),
            wallet,
            TxManagerConfig::default(),
            1,
            Arc::new(NoopTxMetrics),
        )
        .await
        .unwrap();
        let error = manager
            .prepare(
                &TxCandidate { to: Some(Address::with_last_byte(1)), ..Default::default() },
                None,
            )
            .await
            .unwrap_err();
        server.abort();
        let expected_reason =
            if structured { format!("revert: {reason}") } else { reason.to_string() };
        assert_eq!(
            error,
            TxManagerError::ExecutionReverted {
                reason: Some(expected_reason),
                data: structured.then_some(data),
            }
        );
        let calls = calls.lock().unwrap();
        assert_eq!(calls.iter().filter(|m| *m == "eth_estimateGas").count(), 1);
        assert!(!calls.iter().any(|m| m == "eth_sendRawTransaction"));
    }
}

#[test]
fn bare_provider_errors_keep_their_identity_even_with_data() {
    let expected = [
        TxManagerError::ReplacementUnderpriced,
        TxManagerError::Underpriced,
        TxManagerError::NonceTooLow,
        TxManagerError::NonceTooHigh,
        TxManagerError::InsufficientFunds,
        TxManagerError::IntrinsicGasTooLow,
    ];
    for (message, expected) in COLLISIONS.into_iter().zip(expected).chain([(
        "unknown provider failure",
        TxManagerError::Rpc("unknown provider failure".into()),
    )]) {
        let error: TransportError = RpcError::ErrorResp(ErrorPayload {
            code: -32000,
            message: message.to_string().into(),
            data: Some(
                serde_json::value::to_raw_value(&json!({"detail": "provider data"})).unwrap(),
            ),
        });
        assert_eq!(RpcErrorClassifier::classify_rpc_error(&error), expected);
    }
}
