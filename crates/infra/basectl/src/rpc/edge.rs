//! HTTP client and wire types for an Axiom Edge proving manager.

use std::{collections::BTreeMap, fmt, time::Duration};

use anyhow::{Context, Result, bail};
use reqwest::{
    Response,
    multipart::{Form, Part},
};
use serde::{Deserialize, Serialize};
use url::Url;

/// Covers a multi-megabyte input upload over the mesh.
const EDGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Program in the manager's loadout (`EDGE_PROGRAMS`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeProgram {
    /// Loadout name, also the `programs/<name>/` artifact directory.
    pub name: String,
    /// Loadout version, also the `programs/<name>/<version>/` artifact directory.
    pub version: u32,
}

/// Final artifact the manager produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeProofType {
    /// Stops at the final internal recursion proof.
    Stark,
    /// Wraps the STARK through root and halo2 into an on-chain verifiable proof.
    Evm,
}

/// `POST /start_proof` body for a manager-staged input.
#[derive(Debug, Clone, Serialize)]
pub struct EdgeProofRequest {
    /// Caller-chosen id: ASCII alphanumerics, `_`, and `-`.
    pub proof_uuid: String,
    /// Program to prove.
    pub program: EdgeProgram,
    /// Requested final artifact.
    pub proof_type: EdgeProofType,
    /// Per-proof watchdog, overriding the manager's `[proof] timeout_secs`.
    pub timeout_secs: u64,
    /// Segment trace-memory budget in bytes, overriding the workers'
    /// `default_segment_memory` and `OpenVM`'s default. Does not change keys.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub segment_memory: Option<u64>,
}

/// `POST /start_proof` success body.
#[derive(Debug, Clone, Deserialize)]
pub struct EdgeProofStarted {
    /// Workers assigned to the proof.
    pub num_workers: usize,
}

/// Proof status. The manager reports most states as a bare label and some
/// terminal states as a single-key object carrying a reason.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum EdgeProofStatus {
    /// Bare label such as `in_progress` or `completed`.
    Label(String),
    /// `{ "<label>": <detail> }`.
    Detailed(BTreeMap<String, serde_json::Value>),
}

impl EdgeProofStatus {
    /// The status label.
    pub fn label(&self) -> &str {
        match self {
            Self::Label(label) => label,
            Self::Detailed(detail) => detail.keys().next().map_or("unknown", String::as_str),
        }
    }
}

impl fmt::Display for EdgeProofStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self::Detailed(detail) = self else { return f.write_str(self.label()) };
        match detail.values().next() {
            Some(serde_json::Value::String(reason)) => write!(f, "{}: {reason}", self.label()),
            Some(reason) => write!(f, "{}: {reason}", self.label()),
            None => f.write_str(self.label()),
        }
    }
}

/// `GET /proof_state/{proof_uuid}` body. Counters and timings fill in as the
/// pipeline advances.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EdgeProofState {
    /// Current status.
    pub status: EdgeProofStatus,
    /// Continuation segments, known once execution finishes.
    pub num_segments: Option<u64>,
    /// Executed guest instructions, known once execution finishes.
    pub num_instructions: Option<u64>,
    /// App (per-segment) proofs received.
    #[serde(default)]
    pub app_proofs_count: u64,
    /// Leaf aggregation proofs received.
    #[serde(default)]
    pub leaf_proofs_count: u64,
    /// Internal recursion proofs received.
    #[serde(default)]
    pub internal_proofs_count: u64,
    /// Final internal compression time; non-zero once the STARK is done.
    pub compression_time_ms: Option<u64>,
    /// Root prove time, reported when the EVM tail completes.
    pub total_root_prove_ms: Option<u64>,
    /// Halo2 prove time, reported when the EVM tail completes.
    pub total_halo2_prove_ms: Option<u64>,
    /// End-to-end latency, reported on completion.
    pub e2e_latency_ms: Option<u64>,
}

impl EdgeProofState {
    /// Whether the manager will not update this proof again.
    pub fn is_terminal(&self) -> bool {
        matches!(self.status.label(), "completed" | "failed" | "canceled")
    }

    /// Whether the proof finished successfully.
    pub fn is_completed(&self) -> bool {
        self.status.label() == "completed"
    }

    /// Whether the STARK recursion is done, so any remaining work is the EVM tail.
    pub fn stark_done(&self) -> bool {
        self.compression_time_ms.is_some_and(|ms| ms > 0) || self.is_completed()
    }
}

/// HTTP client for an Axiom Edge manager.
#[derive(Debug, Clone)]
pub struct EdgeClient {
    base_url: Url,
    http: reqwest::Client,
}

impl EdgeClient {
    /// Creates a client for the manager at `base_url`.
    pub fn new(base_url: Url) -> Result<Self> {
        let http = reqwest::Client::builder().timeout(EDGE_REQUEST_TIMEOUT).build()?;
        Ok(Self { base_url, http })
    }

    /// Stages a bincode `StdIn` on the manager for `proof_uuid`.
    pub async fn upload_input(&self, proof_uuid: &str, stdin: Vec<u8>) -> Result<()> {
        let form = Form::new().part("input", Part::bytes(stdin).file_name("input.bin"));
        let url = self.endpoint(&format!("upload_input/{proof_uuid}"))?;
        Self::check(self.http.post(url).multipart(form).send().await?).await?;
        Ok(())
    }

    /// Starts a proof over the input staged for `request.proof_uuid`.
    pub async fn start_proof(&self, request: &EdgeProofRequest) -> Result<EdgeProofStarted> {
        let url = self.endpoint("start_proof")?;
        let response = Self::check(self.http.post(url).json(request).send().await?).await?;
        Ok(response.json().await?)
    }

    /// Fetches the current state of `proof_uuid`.
    pub async fn proof_state(&self, proof_uuid: &str) -> Result<EdgeProofState> {
        let url = self.endpoint(&format!("proof_state/{proof_uuid}"))?;
        let response = Self::check(self.http.get(url).send().await?).await?;
        Ok(response.json().await?)
    }

    /// Asks the manager to stop `proof_uuid`.
    pub async fn cancel_proof(&self, proof_uuid: &str) -> Result<()> {
        let url = self.endpoint("cancel_proof")?;
        let body = serde_json::json!({ "proof_uuid": proof_uuid });
        Self::check(self.http.post(url).json(&body).send().await?).await?;
        Ok(())
    }

    fn endpoint(&self, path: &str) -> Result<Url> {
        self.base_url.join(path).with_context(|| format!("invalid Edge manager path {path}"))
    }

    async fn check(response: Response) -> Result<Response> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response.text().await.unwrap_or_default();
        bail!("Edge manager returned {status}: {}", body.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_completed_evm_state() {
        let state: EdgeProofState = serde_json::from_str(
            r#"{"app_proofs_count":14,"compression_time_ms":184,"e2e_latency_ms":36161,
            "internal_proofs_count":3,"leaf_proofs_count":4,"num_instructions":88935431,
            "num_segments":14,"program":{"name":"range","version":1},"proof_uuid":"p",
            "status":"completed","total_halo2_prove_ms":18932,"total_root_prove_ms":2639}"#,
        )
        .unwrap();
        assert!(state.is_terminal() && state.is_completed() && state.stark_done());
        assert_eq!(state.num_segments, Some(14));
        assert_eq!(state.total_halo2_prove_ms, Some(18932));
    }

    #[test]
    fn early_state_is_not_stark_done_and_detailed_failure_is_terminal() {
        let early: EdgeProofState = serde_json::from_str(
            r#"{"app_proofs_count":3,"compression_time_ms":0,"status":"in_progress"}"#,
        )
        .unwrap();
        assert!(!early.is_terminal() && !early.stark_done());

        let failed: EdgeProofState =
            serde_json::from_str(r#"{"status":{"failed":"worker 2 crashed"}}"#).unwrap();
        assert_eq!(failed.status.to_string(), "failed: worker 2 crashed");
        assert!(failed.is_terminal() && !failed.is_completed());
    }

    #[test]
    fn request_serializes_wire_names() {
        let request = EdgeProofRequest {
            proof_uuid: "p".to_string(),
            program: EdgeProgram { name: "range".to_string(), version: 1 },
            proof_type: EdgeProofType::Evm,
            timeout_secs: 1800,
            segment_memory: Some(8 << 30),
        };
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"proof_uuid":"p","program":{"name":"range","version":1},"proof_type":"evm","timeout_secs":1800,"segment_memory":8589934592}"#
        );
    }
}
