//! HTTP client for the payload witness cache.

use std::sync::OnceLock;

use alloy_rpc_types::debug::ExecutionWitness;

use crate::WitnessKey;

/// Raw `debug_executePayload` responses are about 15 megabytes. This leaves room for a larger
/// block without accepting an unbounded body.
const MAX_WITNESS_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// Reads one payload witness from a sidecar.
#[derive(Debug)]
pub struct WitnessCacheClient;

impl WitnessCacheClient {
    /// Fetches the witness for `key`.
    ///
    /// A missing witness is `Ok(None)`. Transport and non-404 HTTP failures are errors so the
    /// caller can fall back to the proof node.
    pub async fn get(
        base_url: &str,
        key: WitnessKey,
    ) -> Result<Option<ExecutionWitness>, WitnessCacheError> {
        let url = format!(
            "{}/witness/{}/{}",
            base_url.trim_end_matches('/'),
            key.parent_hash,
            key.attributes_digest
        );
        let response = http_client().get(url).send().await?;
        if response.status().as_u16() == 404 {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(WitnessCacheError::Status { status: response.status().as_u16() });
        }
        let body = read_body(response, MAX_WITNESS_RESPONSE_BYTES).await?;
        Ok(Some(serde_json::from_slice(&body)?))
    }
}

async fn read_body(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, WitnessCacheError> {
    if response.content_length().is_some_and(|len| len > max_bytes as u64) {
        return Err(WitnessCacheError::TooLarge { max_bytes });
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(WitnessCacheError::TooLarge { max_bytes });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(2))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("witness cache HTTP client")
    })
}

/// Failure talking to the witness cache. A cache miss is not an error.
#[derive(Debug, thiserror::Error)]
pub enum WitnessCacheError {
    /// The HTTP request failed before a response body was read.
    #[error("witness cache request failed: {0}")]
    Request(#[from] reqwest::Error),
    /// The cache responded with a status other than 200 or 404.
    #[error("witness cache returned HTTP {status}")]
    Status {
        /// HTTP status code.
        status: u16,
    },
    /// The response body was larger than the witness size cap.
    #[error("witness cache response exceeded {max_bytes} bytes")]
    TooLarge {
        /// Maximum accepted body size, in bytes.
        max_bytes: usize,
    },
    /// The response body was not a payload witness.
    #[error("witness cache response was not a payload witness: {0}")]
    Decode(#[from] serde_json::Error),
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use std::sync::Arc;

    use alloy_primitives::B256;
    use alloy_rpc_types::debug::ExecutionWitness;
    use tokio::net::TcpListener;

    use super::*;
    use crate::{WitnessCache, WitnessServer};

    #[tokio::test]
    async fn round_trips_a_stored_witness_and_reports_a_miss() {
        let cache = Arc::new(WitnessCache::new(8));
        let key = WitnessKey {
            parent_hash: B256::repeat_byte(0x11),
            attributes_digest: B256::repeat_byte(0x22),
        };
        let witness: ExecutionWitness =
            serde_json::from_str(r#"{"state":["0x01"],"codes":[],"keys":[],"headers":[]}"#)
                .unwrap();
        cache.insert(key, witness);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = WitnessServer::serve(cache, listener).await;
        });

        let base_url = format!("http://{address}");
        let fetched = wait_for_witness(&base_url, key).await;
        assert_eq!(fetched.state.len(), 1);

        let missing = WitnessKey {
            parent_hash: B256::repeat_byte(0x33),
            attributes_digest: B256::repeat_byte(0x44),
        };
        assert!(WitnessCacheClient::get(&base_url, missing).await.unwrap().is_none());
    }

    async fn wait_for_witness(base_url: &str, key: WitnessKey) -> ExecutionWitness {
        for _ in 0..50 {
            if let Ok(Some(witness)) = WitnessCacheClient::get(base_url, key).await {
                return witness;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("witness cache did not start serving");
    }
}
