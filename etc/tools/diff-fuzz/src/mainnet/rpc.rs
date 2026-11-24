//! Blocking JSON-RPC client that retries across a list of endpoints.

use std::{thread, time::Duration};

use eyre::{bail, eyre};
use serde_json::{Value, json};

/// A blocking JSON-RPC client over one or more equivalent endpoints.
///
/// Public gateways put mixed backends behind one URL: some answer, others time out or reply
/// "method not found". A call is therefore retried with exponential backoff, rotating through
/// the endpoints, until one returns a result. An endpoint that reports the requested state as
/// pruned is not asked again for that call; when every endpoint has said so, the call fails.
#[derive(Debug, Clone)]
pub struct RpcClient {
    /// Endpoint URLs, tried in rotation.
    pub urls: Vec<String>,
    /// Maximum number of requests per call, over all endpoints.
    pub max_attempts: u32,
    http: reqwest::blocking::Client,
}

impl RpcClient {
    /// Per-request HTTP timeout. Witness generation for a large block can take tens of seconds.
    pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
    /// First retry delay; doubles per attempt up to [`Self::MAX_BACKOFF`].
    pub const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
    /// Longest retry delay.
    pub const MAX_BACKOFF: Duration = Duration::from_secs(20);
    /// Default for [`Self::max_attempts`].
    pub const DEFAULT_MAX_ATTEMPTS: u32 = 40;

    /// Creates a client over `urls`.
    pub fn new(urls: Vec<String>) -> eyre::Result<Self> {
        if urls.is_empty() {
            bail!("no RPC endpoints configured");
        }
        let http = reqwest::blocking::Client::builder()
            .timeout(Self::REQUEST_TIMEOUT)
            .user_agent("base-diff-fuzz")
            .build()?;
        Ok(Self { urls, max_attempts: Self::DEFAULT_MAX_ATTEMPTS, http })
    }

    /// Calls `method` and returns its non-null `result`.
    pub fn call(&self, method: &str, params: Value) -> eyre::Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let mut pruned = vec![false; self.urls.len()];
        let mut backoff = Self::INITIAL_BACKOFF;
        let mut last_error = String::new();
        let mut url_index = 0;
        for attempt in 0..self.max_attempts {
            while pruned[url_index] {
                url_index = (url_index + 1) % self.urls.len();
            }
            let url = &self.urls[url_index];
            match self.call_once(url, &body) {
                Ok(result) => return Ok(result),
                Err(error) => {
                    last_error = format!("{url}: {error}");
                    if error.contains("pruned") {
                        pruned[url_index] = true;
                        if pruned.iter().all(|p| *p) {
                            bail!("{method}: state unavailable on every endpoint: {last_error}");
                        }
                    }
                }
            }
            url_index = (url_index + 1) % self.urls.len();
            if attempt + 1 < self.max_attempts {
                thread::sleep(backoff);
                backoff = (backoff * 2).min(Self::MAX_BACKOFF);
            }
        }
        Err(eyre!("{method}: no result after {} attempts: {last_error}", self.max_attempts))
    }

    /// Sends `calls` as one JSON-RPC batch and returns their results in order, retrying the
    /// whole batch across endpoints (with [`Self::call`]'s backoff) until every call has a
    /// non-null result.
    pub fn batch(&self, calls: &[(&str, Value)]) -> eyre::Result<Vec<Value>> {
        let body = Value::Array(
            calls
                .iter()
                .enumerate()
                .map(|(id, (method, params))| {
                    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
                })
                .collect(),
        );
        let mut backoff = Self::INITIAL_BACKOFF;
        let mut last_error = String::new();
        for attempt in 0..self.max_attempts {
            let url = &self.urls[attempt as usize % self.urls.len()];
            match self.batch_once(url, &body, calls.len()) {
                Ok(results) => return Ok(results),
                Err(error) => last_error = format!("{url}: {error}"),
            }
            thread::sleep(backoff);
            backoff = (backoff * 2).min(Self::MAX_BACKOFF);
        }
        Err(eyre!(
            "batch of {}: no result after {} attempts: {last_error}",
            calls.len(),
            self.max_attempts
        ))
    }

    fn batch_once(&self, url: &str, body: &Value, len: usize) -> Result<Vec<Value>, String> {
        let response = self
            .http
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string())
            .send()
            .map_err(|e| e.to_string())?;
        let text = response.text().map_err(|e| e.to_string())?;
        let replies: Vec<Value> = serde_json::from_str(&text).map_err(|_| {
            format!("not a batch reply: {}", text.chars().take(200).collect::<String>())
        })?;
        let mut results = vec![Value::Null; len];
        for mut reply in replies {
            let id = reply.get("id").and_then(Value::as_u64).ok_or("reply without id")? as usize;
            match reply.get_mut("result").map(Value::take) {
                Some(result) if !result.is_null() && id < len => results[id] = result,
                _ => return Err(format!("call {id}: {}", reply.get("error").unwrap_or(&reply))),
            }
        }
        if results.iter().any(Value::is_null) {
            return Err("batch reply is missing calls".to_owned());
        }
        Ok(results)
    }

    /// One request to one endpoint; the error is a short description of what came back.
    pub fn call_once(&self, url: &str, body: &Value) -> Result<Value, String> {
        let response = self
            .http
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string())
            .send()
            .map_err(|e| e.to_string())?;
        let status = response.status();
        let text = response.text().map_err(|e| e.to_string())?;
        let mut value: Value = serde_json::from_str(&text).map_err(|_| {
            let snippet: String = text.chars().take(200).collect();
            format!("HTTP {status}: {snippet}")
        })?;
        match value.get_mut("result").map(Value::take) {
            Some(result) if !result.is_null() => Ok(result),
            _ => {
                let error = value.get("error").or_else(|| value.get("message")).unwrap_or(&value);
                let message = error.get("message").unwrap_or(error);
                Err(message.to_string())
            }
        }
    }
}
