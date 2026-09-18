//! Bounded raw RPC observations for fields not exposed by the typed providers.

use std::time::Duration;

use alloy_primitives::{Address, B256, U256};
use base_consensus_rpc::SyncStatusApiClient;
use eyre::{Result, WrapErr, ensure};
use jsonrpsee::http_client::HttpClient;
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};

use super::header::AuthenticatedHeader;

/// HTTP client with a per-request deadline, shared by the acceptance observations.
#[derive(Debug)]
pub struct Rpc {
    /// Deadline-bounded HTTP transport.
    pub http: reqwest::Client,
}

impl Rpc {
    /// Creates a transport whose individual calls cannot outlive assertion deadlines indefinitely.
    pub fn new() -> Result<Self> {
        Ok(Self { http: reqwest::Client::builder().timeout(Duration::from_secs(10)).build()? })
    }

    /// Returns the complete response, preserving structured execution errors for opcode checks.
    pub async fn response(&self, url: &str, method: &str, params: Value) -> Result<Value> {
        self.http
            .post(url)
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .wrap_err_with(|| format!("invalid JSON-RPC response for {method} at {url}"))
    }

    /// Requires a successful RPC result, not merely a responsive endpoint.
    pub async fn call(&self, url: &str, method: &str, params: Value) -> Result<Value> {
        let response = self.response(url, method, params).await?;
        ensure!(response.get("error").is_none(), "{method} at {url}: {}", response["error"]);
        response.get("result").cloned().ok_or_else(|| eyre::eyre!("missing {method} result"))
    }

    /// Fetches a beacon API response including version and optimistic-execution metadata.
    pub async fn beacon(&self, base: &str, path: &str) -> Result<Value> {
        self.http
            .get(format!("{}{path}", base.trim_end_matches('/')))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .wrap_err_with(|| format!("beacon response at {path}"))
    }

    /// Fetches and authenticates a numbered or tagged execution header.
    pub async fn header(&self, url: &str, block: Value) -> Result<AuthenticatedHeader> {
        AuthenticatedHeader::parse(
            self.call(url, "eth_getBlockByNumber", json!([block, false])).await?,
        )
    }

    /// Reads a balance at an exact canonical receipt block, never at a moving head.
    pub async fn balance(&self, url: &str, address: Address, hash: B256) -> Result<U256> {
        Ok(serde_json::from_value(
            self.call(
                url,
                "eth_getBalance",
                json!([address, {"blockHash": hash, "requireCanonical": true}]),
            )
            .await?,
        )?)
    }

    /// Waits until the sequencer has adopted at least the requested canonical L1 origin.
    pub async fn wait_for_l1_origin(
        &self,
        consensus: &HttpClient,
        l1_url: &str,
        required_number: u64,
        required_hash: B256,
        within: Duration,
    ) -> Result<()> {
        let mut last = Value::Null;
        timeout(within, async {
            loop {
                let status = consensus.sync_status().await?;
                last = json!(status);
                let origin = status.unsafe_l2.l1_origin;
                if origin.number == required_number && origin.hash == required_hash {
                    return Ok::<_, eyre::Report>(());
                }
                if origin.number > required_number {
                    let boundary = self
                        .header(l1_url, json!(format!("{required_number:#x}")))
                        .await?;
                    let canonical = self
                        .header(l1_url, json!(format!("{:#x}", origin.number)))
                        .await?;
                    ensure!(boundary.hash == required_hash, "required L1 boundary was replaced");
                    ensure!(canonical.hash == origin.hash, "sequencer L1 origin is not canonical");
                    return Ok::<_, eyre::Report>(());
                }
                sleep(Duration::from_millis(500)).await;
            }
        })
        .await
        .wrap_err_with(|| {
            format!(
                "sequencer did not adopt L1 origin {required_number} ({required_hash}); last status: {last}"
            )
        })??;
        Ok(())
    }

    /// Executes SLOTNUM followed by a 32-byte RETURN at an exact canonical block hash.
    ///
    /// A call-only code override isolates opcode activation from Amsterdam's EIP-8037
    /// contract-creation state charges. It does not deploy code or mutate chain state.
    pub async fn slotnum(&self, url: &str, hash: B256) -> Result<Value> {
        let probe = Address::repeat_byte(0x4b);
        self.response(
            url,
            "eth_call",
            json!([
                {"to": probe, "gas": "0x186a0"},
                {"blockHash": hash, "requireCanonical": true},
                {probe.to_string(): {"code": "0x4b60005260206000f3"}}
            ]),
        )
        .await
    }

    /// Accepts only an opcode execution failure, not a transport, gas, or request error.
    pub fn require_inactive_slotnum(response: &Value) -> Result<()> {
        ensure!(response.get("result").is_none(), "SLOTNUM unexpectedly executed: {response}");
        let message = response["error"]["message"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("missing opcode error: {response}"))?
            .to_ascii_lowercase();
        // Base delegates these EVM halts to Reth's RpcInvalidTransactionError (-32003).
        // Do not accept out-of-gas errors that also mention an invalid opcode operand.
        ensure!(
            response["error"]["code"] == -32003
                && matches!(
                    message.as_str(),
                    "evm error: notactivated" | "evm error: opcodenotfound"
                ),
            "SLOTNUM failed for a reason other than an inactive opcode: {response}"
        );
        Ok(())
    }

    /// Parses an Ethereum RPC quantity without accepting missing values.
    pub fn quantity(value: &Value) -> Result<u64> {
        let hex = value
            .as_str()
            .and_then(|s| s.strip_prefix("0x"))
            .ok_or_else(|| eyre::eyre!("expected hex quantity, got {value}"))?;
        u64::from_str_radix(hex, 16).wrap_err("invalid hex quantity")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::Rpc;

    #[test]
    fn opcode_rejection_does_not_accept_unrelated_rpc_failures() {
        for reason in ["EVM error: NotActivated", "EVM error: OpcodeNotFound"] {
            Rpc::require_inactive_slotnum(&json!({"error": {"code": -32003, "message": reason}}))
                .unwrap();
        }
        for response in [
            json!({"result": "0x0000"}),
            json!({"error": {"message": "insufficient funds"}}),
            json!({"error": {"message": "header not found"}}),
            json!({"error": {"message": "out of gas"}}),
            json!({"error": {"code": -32003, "message": "out of gas: invalid operand to an opcode: 100000"}}),
            json!({"error": {"code": -32602, "message": "EVM error: OpcodeNotFound"}}),
            json!({"error": {"message": "invalid params"}}),
        ] {
            assert!(Rpc::require_inactive_slotnum(&response).is_err());
        }
    }
}
