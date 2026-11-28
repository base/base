//! The methods tiger answers through the sequencer, as base-reth does with
//! `--rollup.sequencer`: `eth_sendRawTransaction` (tiger has no transaction pool, so it only
//! forwards) and `base_transactionStatus`. The client is a minimal blocking JSON-RPC client
//! over HTTP/1.1.

use std::{
    io::{BufReader, Write},
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};

use alloy_consensus::transaction::SignerRecoverable;
use alloy_eips::{Typed2718, eip2718::Decodable2718};
use alloy_primitives::{B256, Bytes};
use base_common_consensus::{BASE_TX_TYPE_ID, BaseTxEnvelope, DEPOSIT_TX_TYPE_ID};
use base_common_genesis::BaseFeature;
use serde_json::{Value, json};

use super::param;
use crate::{Node, RpcError};

/// How long a sequencer request may take to connect, and then to answer.
const SEQUENCER_TIMEOUT: Duration = Duration::from_secs(10);
/// The largest sequencer response read.
const SEQUENCER_RESPONSE_BYTES_MAX: usize = 4 * 1024 * 1024;

impl Node {
    /// Answers the pool methods; `None` for other methods.
    pub(super) fn rpc_sequencer(&self, method: &str, p: &Value) -> Option<Result<Value, RpcError>> {
        Some(match method {
            "eth_sendRawTransaction" => param::<Bytes>(p, 0).and_then(|raw| self.send_raw_transaction(&raw)),
            "base_transactionStatus" => param::<B256>(p, 0).and_then(|hash| match &self.rpc_config.sequencer {
                // Without a sequencer base-reth asks only its pool; tiger has none.
                None => Ok(json!({ "status": "Unknown" })),
                Some(url) => Self::sequencer_request(url, "base_transactionStatus", json!([hash])).map_err(|error| {
                    RpcError::internal(format!("failed to fetch transaction status: {}", error.message))
                }),
            }),
            _ => return None,
        })
    }

    /// `eth_sendRawTransaction`: decoded and recovered as reth's pool type, the Everest gate,
    /// then forwarded; the sequencer's hash or error is the answer.
    fn send_raw_transaction(&self, raw: &Bytes) -> Result<Value, RpcError> {
        if raw.is_empty() {
            return Err(RpcError::invalid_params("Empty transaction data".into()));
        }
        let mut bytes = raw.as_ref();
        let decoded = BaseTxEnvelope::decode_2718(&mut bytes)
            .ok()
            .filter(|tx| bytes.is_empty() && tx.ty() != DEPOSIT_TX_TYPE_ID)
            .ok_or_else(|| RpcError::invalid_params("failed to decode signed transaction".into()))?;
        decoded.recover_signer().map_err(|_| RpcError::invalid_params("invalid transaction signature".into()))?;
        if decoded.ty() == BASE_TX_TYPE_ID {
            let head = self.chain.read().expect("chain lock").head();
            if !self.spec.is_active(BaseFeature::TxBase, head.header.timestamp) {
                return Err(RpcError::new(
                    -32003,
                    "Base transactions are not accepted before the Everest hard fork",
                ));
            }
        }
        let Some(url) = &self.rpc_config.sequencer else {
            return Err(RpcError::internal("tiger has no transaction pool; set --rollup.sequencer-http".into()));
        };
        Self::sequencer_request(url, "eth_sendRawTransaction", json!([raw]))
    }

    /// One JSON-RPC call to the sequencer at `url`: its result, or its error passed through
    /// unchanged; a transport failure is `-32603`.
    fn sequencer_request(url: &str, method: &str, params: Value) -> Result<Value, RpcError> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string();
        let response = Self::http_post(url, &body).map_err(RpcError::internal)?;
        let mut response: Value = serde_json::from_slice(&response)
            .map_err(|error| RpcError::internal(format!("sequencer response: {error}")))?;
        if let Some(error) = response.get("error") {
            return Err(RpcError {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(-32603),
                message: error.get("message").and_then(Value::as_str).unwrap_or_default().to_owned(),
                data: error.get("data").cloned(),
            });
        }
        response
            .get_mut("result")
            .map(Value::take)
            .ok_or_else(|| RpcError::internal("sequencer response has no result".into()))
    }

    /// POSTs `body` as JSON to an `http://` URL and returns the response body.
    fn http_post(url: &str, body: &str) -> Result<Vec<u8>, String> {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| format!("sequencer {url}: tiger forwards to http:// endpoints only"))?;
        let (authority, path) = rest.find('/').map_or((rest, "/"), |at| (&rest[..at], &rest[at..]));
        let address = if authority.contains(':') { authority.to_owned() } else { format!("{authority}:80") };
        let socket = address
            .to_socket_addrs()
            .map_err(|error| format!("sequencer {authority}: {error}"))?
            .next()
            .ok_or_else(|| format!("sequencer {authority}: no address"))?;
        let mut stream = TcpStream::connect_timeout(&socket, SEQUENCER_TIMEOUT).map_err(|error| error.to_string())?;
        stream.set_read_timeout(Some(SEQUENCER_TIMEOUT)).map_err(|error| error.to_string())?;
        stream.set_write_timeout(Some(SEQUENCER_TIMEOUT)).map_err(|error| error.to_string())?;
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).map_err(|error| error.to_string())?;
        let (status, body) = crate::upstream::read_response(&mut BufReader::new(stream), SEQUENCER_RESPONSE_BYTES_MAX)
            .map_err(|error| format!("sequencer: {error}"))?;
        let status = status.split_whitespace().nth(1).unwrap_or_default();
        // A JSON-RPC error may come with a non-200 status; only a body that is not JSON-RPC fails.
        if !status.starts_with('2') && serde_json::from_slice::<Value>(&body).is_err() {
            return Err(format!("sequencer answered HTTP {status}"));
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };

    use alloy_consensus::{SignableTransaction, TxLegacy};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Bytes, Signature, TxKind, U256};
    use base_common_consensus::BaseTxEnvelope;
    use serde_json::{Value, json};

    use super::super::test_utils::{node, reply};

    /// A sequencer that answers one request with `response` and hands back what it was sent.
    fn sequencer(response: Value) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            while !String::from_utf8_lossy(&request).contains("\"params\"") || !request.ends_with(b"}") {
                let read = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..read]);
            }
            let body = response.to_string();
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(body.as_bytes()).unwrap();
            String::from_utf8(request).unwrap()
        });
        (url, handle)
    }

    fn signed_transfer() -> Bytes {
        let tx = TxLegacy {
            chain_id: Some(8453),
            nonce: 0,
            gas_price: 1,
            gas_limit: 21_000,
            to: TxKind::Call(alloy_primitives::Address::repeat_byte(2)),
            value: U256::from(1),
            input: Bytes::new(),
        };
        let secp = secp256k1::Secp256k1::new();
        let key = secp256k1::SecretKey::from_byte_array([7; 32]).unwrap();
        let message = secp256k1::Message::from_digest(tx.signature_hash().0);
        let (recovery, bytes) = secp.sign_ecdsa_recoverable(message, &key).serialize_compact();
        let signature = Signature::from_bytes_and_parity(&bytes, i32::from(recovery) == 1);
        Bytes::from(BaseTxEnvelope::Legacy(tx.into_signed(signature)).encoded_2718())
    }

    fn call(node: &crate::Node, method: &str, params: Value) -> Value {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        reply(node, &body.to_string()).expect("a request is answered")
    }

    #[test]
    fn raw_transactions_go_to_the_sequencer_and_its_errors_come_back_unchanged() {
        let mut node = node();
        let raw = signed_transfer();
        // Bytes that are not a pooled transaction are rejected before forwarding.
        assert_eq!(call(&node, "eth_sendRawTransaction", json!(["0x"]))["error"]["code"], json!(-32602));
        assert_eq!(call(&node, "eth_sendRawTransaction", json!(["0x7e00"]))["error"]["code"], json!(-32602));
        assert_eq!(call(&node, "base_transactionStatus", json!([format!("0x{}", "11".repeat(32))]))["result"], json!({ "status": "Unknown" }));

        let error = json!({ "code": -32000, "message": "nonce too low", "data": "0x01" });
        let (url, sent) = sequencer(json!({ "jsonrpc": "2.0", "id": 1, "error": error }));
        node.rpc_config.sequencer = Some(url);
        assert_eq!(call(&node, "eth_sendRawTransaction", json!([raw]))["error"], error);
        assert!(sent.join().unwrap().contains(&raw.to_string()), "the raw bytes are forwarded");

        let hash = format!("0x{}", "ab".repeat(32));
        let (url, _) = sequencer(json!({ "jsonrpc": "2.0", "id": 1, "result": hash }));
        node.rpc_config.sequencer = Some(url);
        assert_eq!(call(&node, "eth_sendRawTransaction", json!([raw]))["result"], json!(hash));
    }
}
