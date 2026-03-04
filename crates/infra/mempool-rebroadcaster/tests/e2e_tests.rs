//! End-to-end tests for the mempool rebroadcaster.

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use alloy_rpc_types::txpool::TxpoolContent;
use mempool_rebroadcaster::Rebroadcaster;
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};

const GENESIS_BLOCK_JSON: &str = r#"{"hash":"0x102de6ffb001480cc9b8b548fd05c34cd4f46ae4aa91759393db90ea0409887d","parentHash":"0x0000000000000000000000000000000000000000000000000000000000000000","sha3Uncles":"0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347","miner":"0x4200000000000000000000000000000000000011","stateRoot":"0x06787a17a3ed87c339a39dbbeeb311578a0c83ed29daa2db95da62b28efce8a9","transactionsRoot":"0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421","receiptsRoot":"0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421","logsBloom":"0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000","difficulty":"0x0","number":"0x0","gasLimit":"0x1c9c380","gasUsed":"0x0","timestamp":"0x64d6dbac","extraData":"0x424544524f434b","mixHash":"0x0000000000000000000000000000000000000000000000000000000000000000","nonce":"0x0000000000000000","baseFeePerGas":"0x0","size":"0x209","uncles":[],"transactions":[]}"#;

#[derive(Clone, Copy)]
enum SendRawBehavior {
    RpcNonceTooLow,
    CloseConnection,
    OkHash,
}

#[derive(Clone)]
struct RpcServerConfig {
    txpool_content: Value,
    send_raw_behavior: SendRawBehavior,
    send_raw_calls: Arc<AtomicUsize>,
}

struct RpcServerHandle {
    endpoint: String,
    send_raw_calls: Arc<AtomicUsize>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl RpcServerHandle {
    fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    fn send_raw_calls(&self) -> usize {
        self.send_raw_calls.load(Ordering::SeqCst)
    }
}

impl Drop for RpcServerHandle {
    fn drop(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
        self.task.abort();
    }
}

async fn spawn_rpc_server(config: RpcServerConfig) -> RpcServerHandle {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("failed to bind rpc mock server");
    let addr = listener.local_addr().expect("failed to get rpc mock server address");
    let endpoint = format!("http://{addr}");
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let shared_cfg = Arc::new(config);
    let send_raw_calls = Arc::clone(&shared_cfg.send_raw_calls);
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accept_result = listener.accept() => {
                    let (stream, _) = match accept_result {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let cfg = Arc::clone(&shared_cfg);
                    tokio::spawn(async move {
                        let _ = handle_rpc_connection(stream, cfg).await;
                    });
                }
            }
        }
    });

    RpcServerHandle { endpoint, send_raw_calls, shutdown_tx: Some(shutdown_tx), task }
}

async fn handle_rpc_connection(
    mut stream: TcpStream,
    config: Arc<RpcServerConfig>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let request_body = read_http_json_body(&mut stream).await?;
    let request: Value = serde_json::from_str(&request_body)?;
    let method = request.get("method").and_then(Value::as_str).unwrap_or_default();
    let id = request.get("id").cloned().unwrap_or(Value::Null);

    let response = match method {
        "eth_getBlockByNumber" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": serde_json::from_str::<Value>(GENESIS_BLOCK_JSON)?,
        }),
        "eth_gasPrice" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": "0x0",
        }),
        "txpool_content" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": config.txpool_content.clone(),
        }),
        "eth_sendRawTransaction" => {
            config.send_raw_calls.fetch_add(1, Ordering::SeqCst);
            match config.send_raw_behavior {
                SendRawBehavior::RpcNonceTooLow => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32000,
                        "message": "nonce too low",
                    },
                }),
                SendRawBehavior::CloseConnection => {
                    // Simulate transport-level failure by closing without any HTTP response.
                    return Ok(());
                }
                SendRawBehavior::OkHash => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                }),
            }
        }
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": "method not found",
            },
        }),
    };

    write_http_json_response(&mut stream, &response).await?;
    Ok(())
}

async fn read_http_json_body(
    stream: &mut TcpStream,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let mut buffer = Vec::with_capacity(4096);
    let mut chunk = [0_u8; 1024];
    let mut header_end = None;

    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        header_end = find_header_end(&buffer);
        if header_end.is_some() {
            break;
        }
    }

    let header_end = header_end.ok_or("failed to read http headers")?;
    let header_bytes = &buffer[..header_end];
    let header = std::str::from_utf8(header_bytes)?;
    let content_length = header
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            if key.eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .ok_or("missing content-length header")?;

    while buffer.len() < header_end + content_length {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }

    if buffer.len() < header_end + content_length {
        return Err("incomplete http request body".into());
    }

    Ok(std::str::from_utf8(&buffer[header_end..header_end + content_length])?.to_string())
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n").map(|pos| pos + 4)
}

async fn write_http_json_response(
    stream: &mut TcpStream,
    payload: &Value,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let body = serde_json::to_string(payload)?;
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

fn single_tx_geth_mempool_json() -> Value {
    let mut mempool: Value = serde_json::from_str(include_str!("../testdata/geth_mempool.json"))
        .expect("failed to parse geth mempool json");
    let pending =
        mempool.get("pending").and_then(Value::as_object).expect("pending section must be present");
    let (account, nonce_map) = pending
        .iter()
        .next()
        .map(|(account, nonce_map)| (account.clone(), nonce_map.clone()))
        .expect("pending section must have at least one account");
    let nonce_map =
        nonce_map.as_object().expect("account entry in pending section must be an object");
    let (nonce, tx) = nonce_map
        .iter()
        .next()
        .map(|(nonce, tx)| (nonce.clone(), tx.clone()))
        .expect("pending account must have at least one transaction");

    let mut single_nonce_map = Map::new();
    single_nonce_map.insert(nonce, tx);

    let mut single_pending = Map::new();
    single_pending.insert(account, Value::Object(single_nonce_map));

    let mut empty_queued = Map::new();
    if let Some(existing_queued) = mempool.get("queued").and_then(Value::as_object) {
        for account in existing_queued.keys() {
            empty_queued.insert(account.clone(), Value::Object(Map::new()));
        }
    }

    if let Some(obj) = mempool.as_object_mut() {
        obj.insert("pending".to_string(), Value::Object(single_pending));
        obj.insert("queued".to_string(), Value::Object(empty_queued));
    }

    mempool
}

fn empty_mempool_json() -> Value {
    serde_json::from_str(include_str!("../testdata/reth_mempool.json"))
        .expect("failed to parse empty mempool json")
}

fn load_static_mempool_content<P: AsRef<Path>>(
    filepath: P,
) -> Result<TxpoolContent, Box<dyn std::error::Error>> {
    let data = std::fs::read_to_string(filepath)?;
    let content: TxpoolContent = serde_json::from_str(&data)?;
    Ok(content)
}

#[tokio::test]
async fn test_e2e_static_data() {
    // Load static test data
    let geth_mempool = load_static_mempool_content("testdata/geth_mempool.json")
        .expect("Failed to load geth mempool data");

    let reth_mempool = load_static_mempool_content("testdata/reth_mempool.json")
        .expect("Failed to load reth mempool data");

    // Use constant network fees for testing (same as Go version)
    let base_fee = 0x2601ff_u128; // 0x2601ff
    let gas_price = 0x36daa7_u128; // 0x36daa7

    // Create a rebroadcaster instance for testing (endpoints don't matter for this test)
    let rebroadcaster = Rebroadcaster::new(
        "http://localhost:8545".to_string(),
        "http://localhost:8546".to_string(),
    );

    // Apply filtering logic (same as production)
    let filtered_geth_mempool =
        rebroadcaster.filter_underpriced_txns(&geth_mempool, base_fee, gas_price);
    let filtered_reth_mempool =
        rebroadcaster.filter_underpriced_txns(&reth_mempool, base_fee, gas_price);

    // Compute diff (same as production)
    let diff = rebroadcaster.compute_diff(&filtered_geth_mempool, &filtered_reth_mempool);

    // Expected results: Since reth mempool is empty and geth has 5 transactions,
    // all 5 should be in in_geth_not_in_reth (sorted by nonce)
    let expected_missing_hashes = [
        "0x2d4bce6f850ef7ef164585400506893595460fac2fa8f5631de42667896a8b9a", // nonce 97226
        "0x2ddc43a753e327f21b8feab2f83db4a9add29b353519f7c28c217aa677b7671f", // nonce 97227
        "0x30e74220b9769c0eb68f95908d7f9370728a9369e6fa67e734546254c8ebe5ef", // nonce 97228
        "0x872447406779378c9650847a63ffcc2e29e870aa38f2c9fdb30834212d78f860", // nonce 97229
        "0x4c8a6e278cc52d2b8a53e69e372918fe6980f2d27caa9595ac2434ba0b28cbec", // nonce 97230
    ];

    // Assert transaction count
    assert_eq!(
        expected_missing_hashes.len(),
        diff.in_geth_not_in_reth.len(),
        "in_geth_not_in_reth count should match expected"
    );

    // Assert no transactions in reth but not in geth (since reth is empty)
    assert_eq!(
        0,
        diff.in_reth_not_in_geth.len(),
        "in_reth_not_in_geth should be empty since reth mempool is empty"
    );

    // Assert transactions are sorted by nonce and match expected values
    for (i, tx) in diff.in_geth_not_in_reth.iter().enumerate() {
        let tx_hash = format!("{:#x}", tx.as_recovered().hash());
        assert_eq!(
            expected_missing_hashes[i], tx_hash,
            "Transaction {i} hash should match expected"
        );
    }
}

#[tokio::test]
async fn test_e2e_filtering_logic() {
    // Test that underpriced transactions are properly filtered
    // Load the same data but with higher base fees to test filtering
    let geth_mempool = load_static_mempool_content("testdata/geth_mempool.json")
        .expect("Failed to load geth mempool data");

    // Set very high base fee that should filter out our transactions
    let very_high_base_fee = u128::MAX;
    let very_high_gas_price = u128::MAX;

    // Create a rebroadcaster instance for testing
    let rebroadcaster = Rebroadcaster::new(
        "http://localhost:8545".to_string(),
        "http://localhost:8546".to_string(),
    );

    // Apply filtering with very high fees
    let filtered_geth_mempool = rebroadcaster.filter_underpriced_txns(
        &geth_mempool,
        very_high_base_fee,
        very_high_gas_price,
    );

    // Assert all transactions are filtered out due to high base fee
    let total_pending =
        filtered_geth_mempool.pending.values().map(|nonce_txs| nonce_txs.len()).sum::<usize>();
    let total_queued =
        filtered_geth_mempool.queued.values().map(|nonce_txs| nonce_txs.len()).sum::<usize>();

    assert_eq!(
        0,
        total_pending + total_queued,
        "All transactions should be filtered out with very high base fee"
    );
}

#[tokio::test]
async fn test_run_ignores_nonce_too_low_rpc_errors() {
    let geth_server = spawn_rpc_server(RpcServerConfig {
        txpool_content: single_tx_geth_mempool_json(),
        send_raw_behavior: SendRawBehavior::OkHash,
        send_raw_calls: Arc::new(AtomicUsize::new(0)),
    })
    .await;

    let reth_server = spawn_rpc_server(RpcServerConfig {
        txpool_content: empty_mempool_json(),
        send_raw_behavior: SendRawBehavior::RpcNonceTooLow,
        send_raw_calls: Arc::new(AtomicUsize::new(0)),
    })
    .await;

    let rebroadcaster = Rebroadcaster::new(geth_server.endpoint(), reth_server.endpoint());
    let result = rebroadcaster.run().await.expect("run should complete");

    assert_eq!(1, reth_server.send_raw_calls(), "expected one rebroadcast attempt");
    assert_eq!(0, result.success_geth_to_reth);
    assert_eq!(0, result.unexpected_failed_geth_to_reth);
    assert_eq!(0, result.success_reth_to_geth);
    assert_eq!(0, result.unexpected_failed_reth_to_geth);
}

#[tokio::test]
async fn test_run_counts_transport_errors_as_unexpected_failures() {
    let geth_server = spawn_rpc_server(RpcServerConfig {
        txpool_content: single_tx_geth_mempool_json(),
        send_raw_behavior: SendRawBehavior::OkHash,
        send_raw_calls: Arc::new(AtomicUsize::new(0)),
    })
    .await;

    let reth_server = spawn_rpc_server(RpcServerConfig {
        txpool_content: empty_mempool_json(),
        send_raw_behavior: SendRawBehavior::CloseConnection,
        send_raw_calls: Arc::new(AtomicUsize::new(0)),
    })
    .await;

    let rebroadcaster = Rebroadcaster::new(geth_server.endpoint(), reth_server.endpoint());
    let result = rebroadcaster.run().await.expect("run should complete");

    assert_eq!(1, reth_server.send_raw_calls(), "expected one rebroadcast attempt");
    assert_eq!(0, result.success_geth_to_reth);
    assert_eq!(1, result.unexpected_failed_geth_to_reth);
    assert_eq!(0, result.success_reth_to_geth);
    assert_eq!(0, result.unexpected_failed_reth_to_geth);
}
