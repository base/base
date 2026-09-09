//! L1 Beacon blobs endpoint response.

use alloy_eips::eip4844::{Blob, deserialize_blobs};
use serde::{Deserialize, Serialize};

/// Response from `eth/v1/beacon/blobs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetBlobsResponse {
    /// True if the response references an unverified execution payload. Optimistic information may
    /// be invalidated at a later time. If the field is not present, assume the False value.
    #[serde(default)]
    pub execution_optimistic: bool,
    /// True if the response references the finalized history of the chain, as determined by fork
    /// choice. If the field is not present, additional calls are necessary to compare the epoch of
    /// the requested information with the finalized checkpoint.
    #[serde(default)]
    pub finalized: bool,
    /// Vec of individual blobs
    #[serde(deserialize_with = "deserialize_blobs")]
    pub data: Vec<Blob>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omitted_metadata_defaults_to_false() {
        let response: GetBlobsResponse = serde_json::from_str(r#"{"data":[]}"#).unwrap();
        assert!(!response.execution_optimistic);
        assert!(!response.finalized);
        assert!(response.data.is_empty());
    }

    #[test]
    fn rejects_truncated_blob() {
        assert!(serde_json::from_str::<GetBlobsResponse>(r#"{"data":["0x00"]}"#).is_err());
    }
}
