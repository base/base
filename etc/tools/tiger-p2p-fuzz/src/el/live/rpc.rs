//! A minimal JSON-RPC client over HTTP/1.1, with the Engine API's HS256 JWT when a secret is set.

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::Engine as _;
use hmac::Mac;
use serde_json::{Value, json};

/// One node's JSON-RPC endpoint.
#[derive(Debug, Clone)]
pub(crate) struct Rpc {
    /// host:port.
    pub(crate) addr: SocketAddr,
    /// The Engine API JWT secret, for the auth port.
    pub(crate) jwt: Option<[u8; 32]>,
}

fn jwt_token(secret: &[u8; 32]) -> String {
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let iat = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let header = b64.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let claims = b64.encode(format!(r#"{{"iat":{iat}}}"#));
    let signing = format!("{header}.{claims}");
    let mut mac = <hmac::Hmac<sha2::Sha256> as Mac>::new_from_slice(secret).unwrap();
    mac.update(signing.as_bytes());
    format!("{signing}.{}", b64.encode(mac.finalize().into_bytes()))
}

impl Rpc {
    /// Calls `method` and returns its `result`, or the error object as text.
    pub(crate) fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let body =
            json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
        let mut stream = TcpStream::connect_timeout(&self.addr, Duration::from_secs(5))
            .map_err(|e| format!("{}: {e}", self.addr))?;
        stream.set_read_timeout(Some(Duration::from_secs(60))).ok();
        let auth = self
            .jwt
            .map(|s| format!("Authorization: Bearer {}\r\n", jwt_token(&s)))
            .unwrap_or_default();
        let request = format!(
            "POST / HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n{auth}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.addr,
            body.len()
        );
        stream.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&response);
        let (head, body) = text.split_once("\r\n\r\n").ok_or("no HTTP body")?;
        let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
            dechunk(body)
        } else {
            body.to_string()
        };
        let value: Value = serde_json::from_str(&body).map_err(|e| format!("{e}: {body}"))?;
        if let Some(error) = value.get("error") {
            return Err(error.to_string());
        }
        Ok(value["result"].clone())
    }
}

fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some((size, after)) = rest.split_once("\r\n") {
        let Ok(n) = usize::from_str_radix(size.trim(), 16) else { break };
        if n == 0 || after.len() < n {
            break;
        }
        out.push_str(&after[..n]);
        rest = after[n..].trim_start_matches("\r\n");
    }
    out
}
