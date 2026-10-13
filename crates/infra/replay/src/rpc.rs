use std::time::Duration;

use async_trait::async_trait;
use eyre::{Result, bail, eyre};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// JSON-RPC boundary shared by online capture and offline response replay.
#[async_trait]
pub trait Rpc: Send {
    /// Returns a successful result, or fails without fabricating missing data.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value>;
}

/// A reusable response keyed by method and exact JSON parameters, not request ID.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RpcExchange {
    /// JSON-RPC method.
    pub method: String,
    /// Positional parameters.
    pub params: Value,
    /// Successful response payload.
    pub result: Value,
}

/// Read-only HTTP capture client. Credentials and endpoint URLs are never recorded.
#[derive(Debug)]
pub struct HttpRpc {
    client: reqwest::Client,
    url: reqwest::Url,
    /// Captured successful responses, suitable for [`RecordedRpc`].
    pub exchanges: Vec<RpcExchange>,
}

impl HttpRpc {
    /// Creates a client with a bounded timeout for each capture request.
    pub fn new(url: &str) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder().timeout(Duration::from_secs(60)).build()?,
            url: url.parse().map_err(|_| eyre!("invalid RPC URL"))?,
            exchanges: Vec::new(),
        })
    }
}

#[async_trait]
impl Rpc for HttpRpc {
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        // This tool must never forward writes, even when called as a library.
        if !matches!(
            method,
            "eth_chainId"
                | "eth_getBlockByNumber"
                | "eth_getBlockByHash"
                | "eth_getProof"
                | "eth_getCode"
        ) {
            bail!("unsupported capture method: {method}");
        }
        let mut retries = 0;
        let response = loop {
            let response = self
                .client
                .post(self.url.clone())
                .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
                .send()
                .await
                .map_err(|e| e.without_url())?;
            if retries < 5
                && (response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || response.status().is_server_error())
            {
                tokio::time::sleep(Duration::from_secs(1 << retries)).await;
                retries += 1;
                continue;
            }
            break response;
        };
        let response: Value = response
            .error_for_status()
            .map_err(|e| e.without_url())?
            .json()
            .await
            .map_err(|e| e.without_url())?;
        if let Some(error) = response.get("error") {
            bail!("{method}: {error}");
        }
        let result = response
            .get("result")
            .filter(|v| !v.is_null())
            .ok_or_else(|| eyre!("{method}: missing result"))?
            .clone();
        self.exchanges.push(RpcExchange { method: method.into(), params, result: result.clone() });
        Ok(result)
    }
}

/// Offline RPC substitute. Unknown requests fail; there is no network fallback.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RecordedRpc {
    /// Response tape exported by capture.
    pub exchanges: Vec<RpcExchange>,
}

#[async_trait]
impl Rpc for RecordedRpc {
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.exchanges
            .iter()
            .find(|e| e.method == method && e.params == params)
            .map(|e| e.result.clone())
            .ok_or_else(|| eyre!("unrecorded RPC request: {method} {params}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replay_is_repeatable_and_fails_closed() {
        let mut rpc = RecordedRpc {
            exchanges: vec![RpcExchange {
                method: "eth_getBlockByNumber".into(),
                params: json!(["0x42", false]),
                result: json!({"number": "0x42"}),
            }],
        };
        for _ in 0..2 {
            assert_eq!(
                rpc.request("eth_getBlockByNumber", json!(["0x42", false])).await.unwrap(),
                json!({"number": "0x42"})
            );
        }
        assert!(rpc.request("eth_getBlockByNumber", json!(["0x43", false])).await.is_err());
        assert!(rpc.request("eth_getBlockByHash", json!(["0x42", false])).await.is_err());
    }
}
