//! Blocking JSON-RPC and Engine API client for one running node.

use std::{
    collections::BTreeMap,
    io::Read,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_primitives::hex;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use eyre::{WrapErr, bail, eyre};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// What came back for one JSON-RPC request body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// The parsed response body: one envelope, or an array of them for a batch.
    Body(Value),
    /// No JSON body: a connection failure, a timeout, or an HTTP error with a non-JSON body.
    /// The text is free-form and never compared.
    Transport(String),
}

/// Edge counters a coverage node reported for one request (see `fuzz_coverage` in the README).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoverageReading {
    /// Size of the node's counter map.
    pub counters: u64,
    /// `(edge index, 8-bit hit count)` for every edge the request hit.
    pub hits: Vec<(u64, u64)>,
}

/// One node under test: a JSON-RPC endpoint and its authenticated Engine API endpoint.
#[derive(Debug, Clone)]
pub struct NodeClient {
    /// Short client name used in reports, such as `base` or `nethermind`.
    pub name: String,
    /// JSON-RPC URL.
    pub http_url: String,
    /// Engine API URL.
    pub auth_url: String,
    secret: [u8; 32],
    http: reqwest::blocking::Client,
    /// Set for a coverage node: held from sending a request until its coverage is read.
    coverage: Option<Arc<Mutex<()>>>,
    /// Set by [`Self::enable_coverage_sink`]: edge hit counts summed over every reading that is
    /// not returned by [`Self::send_covered`].
    sink: Option<Arc<Mutex<BTreeMap<u64, u64>>>>,
}

impl NodeClient {
    /// Per-request timeout. Traces of large blocks are the slowest requests.
    pub const TIMEOUT: Duration = Duration::from_secs(60);
    /// Largest response body read. Every reply is parsed into a JSON value per node and
    /// normalized, so a multi-gigabyte reply (an uncapped opcode trace) would exhaust the
    /// harness's memory; a larger reply is a transport failure and so a saved finding.
    pub const MAX_RESPONSE: u64 = 64 << 20;

    /// Parses `name=http_url,auth_url` and reads the shared JWT secret (hex) from `jwt_path`.
    pub fn parse(spec: &str, jwt_path: &std::path::Path) -> eyre::Result<Self> {
        let (name, urls) = spec
            .split_once('=')
            .ok_or_else(|| eyre!("node spec must be name=http,auth: {spec}"))?;
        let (http_url, auth_url) = urls
            .split_once(',')
            .ok_or_else(|| eyre!("node spec must be name=http,auth: {spec}"))?;
        let text = std::fs::read_to_string(jwt_path)
            .wrap_err_with(|| format!("reading JWT secret {}", jwt_path.display()))?;
        let bytes = hex::decode(text.trim().trim_start_matches("0x"))?;
        let secret: [u8; 32] =
            bytes.try_into().map_err(|_| eyre!("JWT secret must be 32 bytes of hex"))?;
        let http = reqwest::blocking::Client::builder().timeout(Self::TIMEOUT).build()?;
        Ok(Self {
            name: name.to_owned(),
            http_url: http_url.to_owned(),
            auth_url: auth_url.to_owned(),
            secret,
            http,
            coverage: None,
            sink: None,
        })
    }

    /// The JSON-RPC method that returns and clears a coverage node's edge counters.
    pub const COVERAGE_METHOD: &'static str = "fuzz_coverage";

    /// Makes this a coverage node: from now on every request, Engine API calls included, is
    /// sent one at a time and followed by a coverage reading, so each reading belongs to one
    /// request. [`Self::send_covered`] returns the reading; every other request discards it.
    pub fn enable_coverage(&mut self) {
        self.coverage = Some(Arc::new(Mutex::new(())));
    }

    /// Makes this a coverage node whose readings are summed instead of discarded, so the edges
    /// of a sequence of requests (Engine API calls and state reads alike) can be taken at once
    /// with [`Self::take_coverage`]. Clones share the sum.
    pub fn enable_coverage_sink(&mut self) {
        self.enable_coverage();
        self.sink = Some(Arc::new(Mutex::new(BTreeMap::new())));
    }

    /// Reads the node's pending coverage into the sink, then returns and clears the sum. `None`
    /// when [`Self::enable_coverage_sink`] was not called.
    pub fn take_coverage(&self) -> Option<BTreeMap<u64, u64>> {
        let sink = self.sink.as_ref()?;
        let lock = self.coverage.as_ref()?;
        let _held = lock.lock().expect("coverage lock");
        self.drain_coverage();
        Some(std::mem::take(&mut *sink.lock().expect("coverage sink")))
    }

    /// Clears the sum and the node's pending coverage, such as after a restart's warm-up.
    pub fn reset_coverage(&self) {
        if let Some(sink) = &self.sink {
            let _ = self.take_coverage();
            sink.lock().expect("coverage sink").clear();
        }
    }

    /// Reads coverage into the sink (or discards it without one). The caller holds the lock.
    fn drain_coverage(&self) {
        let reading = self.read_coverage();
        if let (Some(sink), Some(reading)) = (&self.sink, reading) {
            let mut sink = sink.lock().expect("coverage sink");
            for (edge, count) in reading.hits {
                let sum = sink.entry(edge).or_default();
                *sum = sum.saturating_add(count);
            }
        }
    }

    /// Whether [`Self::enable_coverage`] was called.
    pub const fn is_coverage_node(&self) -> bool {
        self.coverage.is_some()
    }

    /// Sends a raw body like [`Self::send`] and returns the coverage it produced, `None` when
    /// this is not a coverage node or the reading failed.
    pub fn send_covered(&self, body: &str) -> (Reply, Option<CoverageReading>) {
        let Some(lock) = &self.coverage else {
            return (self.send(body), None);
        };
        let _held = lock.lock().expect("coverage lock");
        let reply = self.post_raw(&self.http_url, body, None);
        (reply, self.read_coverage())
    }

    /// Reads and clears the node's coverage. The caller holds the coverage lock.
    fn read_coverage(&self) -> Option<CoverageReading> {
        let body =
            json!({ "jsonrpc": "2.0", "id": 1, "method": Self::COVERAGE_METHOD, "params": [] });
        let Reply::Body(body) = self.post_raw(&self.http_url, &body.to_string(), None) else {
            return None;
        };
        let result = body.get("result")?;
        let hits = result
            .get("hits")?
            .as_array()?
            .iter()
            .filter_map(|pair| Some((pair.get(0)?.as_u64()?, pair.get(1)?.as_u64()?)))
            .collect();
        Some(CoverageReading { counters: result.get("counters")?.as_u64()?, hits })
    }

    /// Posts a raw body to the JSON-RPC endpoint. The body need not be valid JSON.
    pub fn send(&self, body: &str) -> Reply {
        self.post(&self.http_url, body, None)
    }

    /// Calls `method` and returns the response envelope.
    pub fn call(&self, method: &str, params: &Value) -> Reply {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        self.send(&body.to_string())
    }

    /// Calls `method` and returns its `result`, failing on an error response.
    pub fn result(&self, method: &str, params: &Value) -> eyre::Result<Value> {
        Self::unwrap(&self.name, method, self.call(method, params))
    }

    /// Calls an Engine API method and returns its `result`.
    pub fn engine(&self, method: &str, params: &Value) -> eyre::Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let token = self.jwt();
        Self::unwrap(&self.name, method, self.post(&self.auth_url, &body.to_string(), Some(&token)))
    }

    fn unwrap(name: &str, method: &str, reply: Reply) -> eyre::Result<Value> {
        match reply {
            Reply::Body(mut body) => {
                if let Some(error) = body.get("error") {
                    bail!("{name} {method}: {error}");
                }
                body.get_mut("result")
                    .map(Value::take)
                    .ok_or_else(|| eyre!("{name} {method}: no result in {body}"))
            }
            Reply::Transport(error) => bail!("{name} {method}: {error}"),
        }
    }

    fn post(&self, url: &str, body: &str, token: Option<&str>) -> Reply {
        let Some(lock) = &self.coverage else {
            return self.post_raw(url, body, token);
        };
        let _held = lock.lock().expect("coverage lock");
        let reply = self.post_raw(url, body, token);
        self.drain_coverage();
        reply
    }

    fn post_raw(&self, url: &str, body: &str, token: Option<&str>) -> Reply {
        let mut request = self
            .http
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_owned());
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = match request.send() {
            Ok(response) => response,
            Err(error) => return Reply::Transport(error.to_string()),
        };
        let status = response.status();
        let mut text = String::new();
        if let Err(error) = response.take(Self::MAX_RESPONSE + 1).read_to_string(&mut text) {
            return Reply::Transport(error.to_string());
        }
        if text.len() as u64 > Self::MAX_RESPONSE {
            return Reply::Transport(format!("response over {} MiB", Self::MAX_RESPONSE >> 20));
        }
        Self::parse_body(&text).map_or_else(
            |_| {
                Reply::Transport(format!(
                    "HTTP {status}: {}",
                    text.chars().take(200).collect::<String>()
                ))
            },
            Reply::Body,
        )
    }

    /// Parses a response body without `serde_json`'s nesting limit: call traces of deep call
    /// stacks nest hundreds of levels.
    pub fn parse_body(text: &str) -> serde_json::Result<Value> {
        let mut deserializer = serde_json::Deserializer::from_str(text);
        deserializer.disable_recursion_limit();
        let value = serde::Deserialize::deserialize(&mut deserializer)?;
        deserializer.end()?;
        Ok(value)
    }

    /// An HS256 Engine API token issued now.
    fn jwt(&self) -> String {
        let iat = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let claims = URL_SAFE_NO_PAD.encode(format!(r#"{{"iat":{iat}}}"#));
        let signing_input = format!("{header}.{claims}");
        let signature =
            URL_SAFE_NO_PAD.encode(Self::hmac_sha256(&self.secret, signing_input.as_bytes()));
        format!("{signing_input}.{signature}")
    }

    /// HMAC-SHA256 (RFC 2104) for a key shorter than the block size.
    fn hmac_sha256(key: &[u8; 32], message: &[u8]) -> [u8; 32] {
        let mut ipad = [0x36u8; 64];
        let mut opad = [0x5cu8; 64];
        for (i, byte) in key.iter().enumerate() {
            ipad[i] ^= byte;
            opad[i] ^= byte;
        }
        let inner = Sha256::new().chain_update(ipad).chain_update(message).finalize();
        Sha256::new().chain_update(opad).chain_update(inner).finalize().into()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::hex;

    use super::NodeClient;

    #[test]
    fn hmac_matches_rfc_4231_case_1() {
        // HMAC zero-pads keys to the block size, so the RFC's 20-byte key padded to 32 bytes
        // gives the RFC's MAC.
        let mut key = [0u8; 32];
        key[..20].fill(0x0b);
        let mac = NodeClient::hmac_sha256(&key, b"Hi There");
        assert_eq!(
            hex::encode(mac),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }
}
