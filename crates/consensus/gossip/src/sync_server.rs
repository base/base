//! Sync request/response server trait for the OP Stack gossip driver.
//!
//! The response wire format is:
//! - Success: `[0x00][version_u32_le][snappy_ssz_bytes...]`
//! - Not found: `[0x01][0x00]`

use std::fmt;

use async_trait::async_trait;

/// Provider for execution payloads by block number.
///
/// Implementations fetch the L2 execution payload at the given block number, encode it per the
/// OP Stack req/resp protocol spec, and return the protocol version alongside the snappy-
/// compressed SSZ bytes.
///
/// The returned bytes are passed directly to the peer as the `<payload>` portion of the response:
/// `<res><version><payload>` where `res = 0x00` (success).
///
/// <https://specs.optimism.io/protocol/rollup-node-p2p.html#payload_by_number>
#[async_trait]
pub trait BlockPayloadProvider: Send + Sync + fmt::Debug + 'static {
    /// Returns `(version: u32, snappy_ssz_bytes: Vec<u8>)` or `None` if the payload is
    /// unavailable for the given block number.
    ///
    /// Version semantics (per the OP Stack req/resp spec):
    /// - `0`: pre-Ecotone — snappy-compressed SSZ of `ExecutionPayloadV1`
    /// - `1`: Ecotone — snappy-compressed SSZ of `ExecutionPayloadV2` with appended
    ///   `parent_beacon_block_root`
    async fn get_payload(&self, block_number: u64) -> Option<(u32, Vec<u8>)>;
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A mock [`BlockPayloadProvider`] for testing.
    #[derive(Debug)]
    pub struct MockPayloadProvider {
        /// The payload to return, keyed by block number. Returns `None` if absent.
        pub payload: Option<(u32, Vec<u8>)>,
    }

    #[async_trait]
    impl BlockPayloadProvider for MockPayloadProvider {
        async fn get_payload(&self, _block_number: u64) -> Option<(u32, Vec<u8>)> {
            self.payload.clone()
        }
    }

    #[tokio::test]
    async fn test_mock_provider_returns_payload() {
        let provider = MockPayloadProvider { payload: Some((1, vec![0xde, 0xad, 0xbe, 0xef])) };
        let result = provider.get_payload(42).await;
        assert_eq!(result, Some((1, vec![0xde, 0xad, 0xbe, 0xef])));
    }

    #[tokio::test]
    async fn test_mock_provider_returns_none() {
        let provider = MockPayloadProvider { payload: None };
        let result = provider.get_payload(42).await;
        assert!(result.is_none());
    }

    /// Verifies the response byte layout for a version-1 success response.
    ///
    /// A success response for version 1 must start with `[0x00, 0x01, 0x00, 0x00, 0x00, ...]`.
    #[tokio::test]
    async fn test_success_response_byte_layout_v1() {
        let payload_bytes = vec![0xaa, 0xbb];
        let version: u32 = 1;
        let provider = MockPayloadProvider { payload: Some((version, payload_bytes.clone())) };
        let (v, bytes) = provider.get_payload(10).await.unwrap();

        // Build the wire response the same way the handler does.
        let mut response = Vec::new();
        response.push(0x00); // success
        response.extend_from_slice(&v.to_le_bytes());
        response.extend_from_slice(&bytes);

        assert_eq!(response[0], 0x00, "status must be 0x00 (success)");
        assert_eq!(&response[1..5], &[0x01, 0x00, 0x00, 0x00], "version must be 1 LE");
        assert_eq!(&response[5..], &payload_bytes);
    }
}
