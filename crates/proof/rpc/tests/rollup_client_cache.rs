//! Integration tests for rollup client output-at-block cache behavior.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use alloy_primitives::B256;
use base_proof_rpc::{RollupClient, RollupClientConfig, RollupProvider};
use serde_json::Value;
use url::Url;

fn repeated_hash(byte: &str) -> String {
    format!("0x{}", byte.repeat(32))
}

fn output_response(id: &Value, output_root: &str, block_number: u64) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"result":{{"outputRoot":"{output_root}","blockRef":{{"hash":"0x3333333333333333333333333333333333333333333333333333333333333333","number":{block_number},"parentHash":"0x2222222222222222222222222222222222222222222222222222222222222222","timestamp":1234567890,"l1origin":{{"hash":"0x1111111111111111111111111111111111111111111111111111111111111111","number":100}},"sequenceNumber":0}}}}}}"#
    )
}

fn read_http_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buf = [0; 1024];

    loop {
        let read = stream.read(&mut buf).expect("read request");
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..read]);

        let Some(headers_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&bytes[..headers_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(": ")?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.parse::<usize>().expect("content length"))
            })
            .unwrap_or_default();

        if bytes.len() >= headers_end + 4 + content_length {
            break;
        }
    }

    String::from_utf8(bytes).expect("request is UTF-8")
}

fn request_id(request: &str) -> Value {
    let body_start = request.find("\r\n\r\n").expect("request has body separator") + 4;
    let body: Value = serde_json::from_str(&request[body_start..]).expect("request body is JSON");

    body.get("id").cloned().expect("request has JSON-RPC id")
}

fn start_output_rpc(
    roots: Vec<String>,
    block_number: u64,
) -> (Url, Arc<AtomicUsize>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock rollup RPC");
    let addr = listener.local_addr().expect("local address");
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_thread = Arc::clone(&calls);

    let handle = thread::spawn(move || {
        for root in roots {
            let (mut stream, _) = listener.accept().expect("accept request");
            let request = read_http_request(&mut stream);
            assert!(request.contains("optimism_outputAtBlock"), "unexpected request: {request}");
            assert!(
                request.contains(&format!("0x{block_number:x}")),
                "request did not ask for block {block_number}: {request}"
            );
            let id = request_id(&request);

            calls_for_thread.fetch_add(1, Ordering::SeqCst);
            let body = output_response(&id, &root, block_number);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).expect("write response");
        }
    });

    (Url::parse(&format!("http://{addr}")).expect("mock URL"), calls, handle)
}

#[tokio::test(flavor = "multi_thread")]
async fn fresh_output_at_block_bypasses_and_updates_cache() {
    let block_number = 12_345;
    let stale_root_hex = repeated_hash("11");
    let fresh_root_hex = repeated_hash("22");
    let stale_root: B256 = stale_root_hex.parse().expect("stale root");
    let fresh_root: B256 = fresh_root_hex.parse().expect("fresh root");

    let (url, calls, server) = start_output_rpc(vec![stale_root_hex, fresh_root_hex], block_number);
    let client =
        RollupClient::new(RollupClientConfig::new(url).with_timeout(Duration::from_secs(5)))
            .expect("rollup client");

    let first = client.output_at_block(block_number).await.expect("first cached read");
    assert_eq!(first.output_root, stale_root);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let cached = client.output_at_block(block_number).await.expect("second cached read");
    assert_eq!(cached.output_root, stale_root);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let fresh =
        client.fresh_output_at_block(block_number).await.expect("fresh output-at-block read");
    assert_eq!(fresh.output_root, fresh_root);
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let updated_cached = client.output_at_block(block_number).await.expect("updated cached read");
    assert_eq!(updated_cached.output_root, fresh_root);
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    server.join().expect("mock server");
}
