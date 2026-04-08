//! Contains an online implementation of the `BeaconClient` trait.

use std::{boxed::Box, collections::HashMap, format, string::String, vec::Vec};

use alloy_eips::eip4844::{env_settings::EnvKzgSettings, kzg_to_versioned_hash};
use alloy_primitives::{B256, FixedBytes};
use alloy_rpc_types_beacon::sidecar::GetBlobsResponse;
use alloy_transport_http::reqwest::{self, Client};
use async_trait::async_trait;
use c_kzg::Blob;
use thiserror::Error;

use crate::{Metrics, blobs::BoxedBlob};

/// The config spec engine api method.
const SPEC_METHOD: &str = "eth/v1/config/spec";

/// The beacon genesis engine api method.
const GENESIS_METHOD: &str = "eth/v1/beacon/genesis";

/// The blobs engine api method prefix.
const BLOBS_METHOD_PREFIX: &str = "eth/v1/beacon/blobs";

/// A reduced genesis data.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReducedGenesisData {
    /// The genesis time.
    #[serde(rename = "genesis_time")]
    #[serde(with = "alloy_serde::quantity")]
    pub genesis_time: u64,
}

/// An API genesis response.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct APIGenesisResponse {
    /// The data.
    pub data: ReducedGenesisData,
}

/// A reduced config data.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReducedConfigData {
    /// The seconds per slot.
    #[serde(rename = "SECONDS_PER_SLOT")]
    #[serde(with = "alloy_serde::quantity")]
    pub seconds_per_slot: u64,
}

/// An API config response.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct APIConfigResponse {
    /// The data.
    pub data: ReducedConfigData,
}

impl APIConfigResponse {
    /// Creates a new API config response.
    pub const fn new(seconds_per_slot: u64) -> Self {
        Self { data: ReducedConfigData { seconds_per_slot } }
    }
}

impl APIGenesisResponse {
    /// Creates a new API genesis response.
    pub const fn new(genesis_time: u64) -> Self {
        Self { data: ReducedGenesisData { genesis_time } }
    }
}

/// The [`BeaconClient`] is a thin wrapper around the Beacon API.
#[async_trait]
pub trait BeaconClient {
    /// The error type for [`BeaconClient`] implementations.
    type Error: core::fmt::Display;

    /// Returns the slot number if this error represents a beacon slot not found (HTTP 404).
    ///
    /// Returns `None` for all other error kinds. This allows the blob provider to distinguish
    /// permanently-unavailable slots (missed/orphaned beacon blocks) from transient errors,
    /// and trigger a pipeline reset instead of retrying indefinitely.
    fn slot_not_found(err: &Self::Error) -> Option<u64>;

    /// Returns the slot interval in seconds.
    async fn slot_interval(&self) -> Result<APIConfigResponse, Self::Error>;

    /// Returns the beacon genesis time.
    async fn genesis_time(&self) -> Result<APIGenesisResponse, Self::Error>;

    /// Fetches blobs that were confirmed in the specified L1 block with the given slot.
    /// Blob data is checked for validity.
    async fn filtered_beacon_blobs(
        &self,
        slot: u64,
        blob_hashes: &[B256],
    ) -> Result<Vec<BoxedBlob>, Self::Error>;
}

const BLOB_SIZE: usize = 131072;

/// [`blob_versioned_hash`] computes the versioned hash of a blob.
fn blob_versioned_hash(blob: &FixedBytes<BLOB_SIZE>) -> Result<B256, BeaconClientError> {
    let kzg_settings = EnvKzgSettings::Default;
    let kzg_blob = Blob::new(blob.0);
    let commitment = kzg_settings.get().blob_to_kzg_commitment(&kzg_blob)?;
    Ok(kzg_to_versioned_hash(commitment.as_slice()))
}

/// An error that can occur when interacting with the beacon client.
#[derive(Error, Debug)]
pub enum BeaconClientError {
    /// HTTP request failed.
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// The beacon node returned HTTP 404 for the requested slot. This means the slot was missed
    /// or orphaned and the blobs will never be available.
    #[error("Beacon slot not found (HTTP 404) for slot {0}")]
    SlotNotFound(u64),

    /// Blob hash not found in beacon response.
    #[error("Blob hash not found in beacon response: {0}")]
    BlobNotFound(String),

    /// KZG error.
    #[error("KZG error: {0}")]
    KZG(#[from] c_kzg::Error),
}

/// An online implementation of the [`BeaconClient`] trait.
#[derive(Debug, Clone)]
pub struct OnlineBeaconClient {
    /// The base URL of the beacon API.
    pub base: String,
    /// The inner reqwest client.
    pub inner: Client,
    /// The duration in seconds of an L1 slot. This can be used to override the CL slot
    /// duration if the l1-beacon's slot configuration endpoint is not available.
    pub l1_slot_duration: Option<u64>,
}

impl OnlineBeaconClient {
    /// Creates a new [`OnlineBeaconClient`] from the provided base URL string.
    pub fn new_http(mut base: String) -> Self {
        // If base ends with a slash, remove it
        if base.ends_with('/') {
            base.remove(base.len() - 1);
        }
        Self {
            base,
            inner: Client::builder().build().expect("Failed to create beacon client"),
            l1_slot_duration: None,
        }
    }

    /// Sets the duration in seconds of an L1 slot. This can be used to override the CL slot
    /// duration if the l1-beacon's slot configuration endpoint is not available.
    pub const fn with_l1_slot_duration_override(mut self, l1_slot_duration: u64) -> Self {
        self.l1_slot_duration = Some(l1_slot_duration);
        self
    }

    /// Fetches only the blobs corresponding to the provided (versioned) blob hashes
    /// from the beacon [`BLOBS_METHOD_PREFIX`] endpoint.
    /// Blobs are validated against the supplied versioned hashes
    /// and returned in the same order as the input.
    async fn filtered_beacon_blobs(
        &self,
        slot: u64,
        blob_hashes: &[B256],
    ) -> Result<Vec<BoxedBlob>, BeaconClientError> {
        let params = blob_hashes.iter().map(|hash| hash.to_string()).collect::<Vec<_>>();
        let url = format!(
            "{}/{}/{}?versioned_hashes={}",
            self.base,
            BLOBS_METHOD_PREFIX,
            slot,
            params.join(",")
        );
        let response = self.inner.get(url).send().await?;

        // A 404 means the beacon slot was missed or orphaned. Blobs for such slots will never
        // become available, so surface this as a distinct error rather than a generic HTTP error
        // so that callers can trigger a pipeline reset instead of retrying indefinitely.
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(BeaconClientError::SlotNotFound(slot));
        }

        let response = response.error_for_status()?;
        let bundle = response.json::<GetBlobsResponse>().await?;

        let returned_blobs_mapped_by_hash = bundle
            .data
            .iter()
            .map(|data| -> Result<_, BeaconClientError> {
                let recomputed_hash = blob_versioned_hash(data)?;
                Ok((recomputed_hash, data))
            })
            .collect::<Result<HashMap<_, _>, BeaconClientError>>()?;

        // Map the input (blob_hashes) into the output,
        // finding the blob from the response
        // whose hash matches the input:
        blob_hashes
            .iter()
            .map(|blob_hash| -> Result<BoxedBlob, BeaconClientError> {
                let matching_data = returned_blobs_mapped_by_hash
                    .get(blob_hash)
                    .ok_or(BeaconClientError::BlobNotFound(blob_hash.to_string()))?;
                Ok(BoxedBlob { blob: Box::new(**matching_data) })
            })
            .collect::<Result<Vec<_>, BeaconClientError>>()
    }
}

#[async_trait]
impl BeaconClient for OnlineBeaconClient {
    type Error = BeaconClientError;

    fn slot_not_found(err: &Self::Error) -> Option<u64> {
        if let BeaconClientError::SlotNotFound(slot) = err { Some(*slot) } else { None }
    }

    async fn slot_interval(&self) -> Result<APIConfigResponse, Self::Error> {
        Metrics::beacon_requests("spec").increment(1);

        // Use the l1 slot duration if provided
        if let Some(l1_slot_duration) = self.l1_slot_duration {
            return Ok(APIConfigResponse::new(l1_slot_duration));
        }

        let result = base_metrics::time!(Metrics::request_duration("spec"), {
            async {
                let first = self.inner.get(format!("{}/{}", self.base, SPEC_METHOD)).send().await?;
                first.json::<APIConfigResponse>().await
            }
            .await
        });

        if result.is_err() {
            Metrics::beacon_errors("spec").increment(1);
        }

        Ok(result?)
    }

    async fn genesis_time(&self) -> Result<APIGenesisResponse, Self::Error> {
        Metrics::beacon_requests("genesis").increment(1);

        let result = base_metrics::time!(Metrics::request_duration("genesis"), {
            async {
                let first =
                    self.inner.get(format!("{}/{}", self.base, GENESIS_METHOD)).send().await?;
                first.json::<APIGenesisResponse>().await
            }
            .await
        });

        if result.is_err() {
            Metrics::beacon_errors("genesis").increment(1);
        }

        Ok(result?)
    }

    async fn filtered_beacon_blobs(
        &self,
        slot: u64,
        blob_hashes: &[B256],
    ) -> Result<Vec<BoxedBlob>, BeaconClientError> {
        Metrics::beacon_requests("blobs").increment(1);

        // Try to get the blobs from the blobs endpoint.
        let result = base_metrics::time!(Metrics::request_duration("blobs"), {
            self.filtered_beacon_blobs(slot, blob_hashes).await
        });

        if result.is_err() {
            Metrics::beacon_errors("blobs").increment(1);
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Blob;
    use alloy_primitives::{B256, FixedBytes, hex::FromHex};
    use httpmock::prelude::*;
    use serde_json::json;

    use super::*;

    const TEST_BLOB_HASH_HEX: &str =
        "0x016c357b8b3a6b3fd82386e7bebf77143d537cdb1c856509661c412602306a04";

    /// Computes the versioned hash for a blob using the same path as production code.
    fn versioned_hash_for(blob: &Blob) -> B256 {
        super::blob_versioned_hash(blob).unwrap()
    }

    #[test]
    fn test_blob_versioned_hash() {
        let input: Blob = FixedBytes::repeat_byte(1);
        let test_blob_hash: FixedBytes<32> = FixedBytes::from_hex(TEST_BLOB_HASH_HEX).unwrap();
        assert_eq!(test_blob_hash, blob_versioned_hash(&input).unwrap());
    }

    /// Verifies that `filtered_beacon_blobs` returns blobs in request order, not server order.
    ///
    /// The server returns `[blob_a, blob_b]` but the client requests `[hash_b, hash_a]`.
    /// The result must follow the request order: `[blob_b, blob_a]`.
    #[tokio::test]
    async fn test_filtered_beacon_blobs_distinct_blobs() {
        let blob_a: Blob = FixedBytes::repeat_byte(1);
        let blob_b: Blob = FixedBytes::repeat_byte(2);
        let hash_a = versioned_hash_for(&blob_a);
        let hash_b = versioned_hash_for(&blob_b);

        let slot = 987654321u64;
        // Request in reverse order relative to what the server will return.
        let requested_hashes: Vec<B256> = vec![hash_b, hash_a];
        let required_query_param = format!("{hash_b},{hash_a}");

        let server = MockServer::start();
        let blobs_mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/eth/v1/beacon/blobs/{slot}"))
                .query_param("versioned_hashes", required_query_param);
            then.status(200).json_body(json!({
                "execution_optimistic": false,
                "finalized": false,
                "data": [blob_a, blob_b]  // server returns natural order
            }));
        });

        let client = OnlineBeaconClient::new_http(server.base_url());
        let result = client.filtered_beacon_blobs(slot, &requested_hashes).await.unwrap();
        blobs_mock.assert();

        // Output must follow request order (hash_b first, then hash_a).
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], BoxedBlob { blob: Box::new(blob_b) }, "first result must be blob_b");
        assert_eq!(result[1], BoxedBlob { blob: Box::new(blob_a) }, "second result must be blob_a");
    }

    /// Verifies that requesting a hash that does not match any returned blob yields
    /// `BeaconClientError::BlobNotFound`.
    ///
    /// The server returns `blob_b` (which hashes to `hash_b`), but the client requests
    /// `hash_a`. Since no returned blob hashes to `hash_a`, the lookup must fail.
    #[tokio::test]
    async fn test_filtered_beacon_blobs_hash_mismatch_returns_not_found() {
        let blob_a: Blob = FixedBytes::repeat_byte(1);
        let blob_b: Blob = FixedBytes::repeat_byte(2);
        let hash_a = versioned_hash_for(&blob_a);

        let slot = 5678u64;
        let server = MockServer::start();
        let blobs_mock = server.mock(|when, then| {
            when.method(GET).path(format!("/eth/v1/beacon/blobs/{slot}"));
            then.status(200).json_body(json!({
                "execution_optimistic": false,
                "finalized": false,
                "data": [blob_b]  // server returns blob_b, but client wants hash_a
            }));
        });

        let client = OnlineBeaconClient::new_http(server.base_url());
        let result = client.filtered_beacon_blobs(slot, &[hash_a]).await;
        blobs_mock.assert();

        assert!(
            matches!(result, Err(BeaconClientError::BlobNotFound(_))),
            "expected BlobNotFound when returned blob does not match requested hash, got {result:?}"
        );
    }

    /// Regression test: a beacon node HTTP 404 for a given slot must return
    /// `BeaconClientError::SlotNotFound` rather than a generic `Http` error.
    /// This allows the blob provider layer to map it to `BlobProviderError::BlobNotFound`
    /// and the pipeline to issue a reset rather than retrying indefinitely.
    #[tokio::test]
    async fn test_filtered_beacon_blobs_404_returns_slot_not_found() {
        let slot = 13779552u64;
        let test_blob_hash: FixedBytes<32> = FixedBytes::from_hex(TEST_BLOB_HASH_HEX).unwrap();
        let requested_blob_hashes: Vec<B256> = vec![test_blob_hash];

        let server = MockServer::start();
        let blobs_mock = server.mock(|when, then| {
            when.method(GET).path(format!("/eth/v1/beacon/blobs/{slot}"));
            then.status(404).body(r#"{"code":404,"message":"Block not found"}"#);
        });

        let client = OnlineBeaconClient::new_http(server.base_url());
        let response = client.filtered_beacon_blobs(slot, &requested_blob_hashes).await;
        blobs_mock.assert();

        assert!(
            matches!(response, Err(BeaconClientError::SlotNotFound(s)) if s == slot),
            "expected SlotNotFound({slot}), got {response:?}"
        );
    }
}
