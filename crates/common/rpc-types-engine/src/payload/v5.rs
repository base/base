//! Execution payload envelope V5.

use alloc::vec::Vec;

use alloy_primitives::{Bytes, U256};
use alloy_rpc_types_engine::BlobsBundleV2;

use super::v4::BaseExecutionPayloadV4;

/// This structure maps for the return value of `engine_getPayload` of the beacon chain spec, for
/// V5.
///
/// The OP variant follows the same pattern as V4: replaces `ExecutionPayloadV3` with
/// [`BaseExecutionPayloadV4`] (which adds `withdrawalsRoot`), and keeps all other fields identical
/// to the mainnet [`ExecutionPayloadEnvelopeV5`](alloy_rpc_types_engine::ExecutionPayloadEnvelopeV5).
///
/// See also:
/// [execution payload envelope v5] <https://specs.base.org/upgrades/azul/exec-engine#engine-api-usage>
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct BaseExecutionPayloadEnvelopeV5 {
    /// Execution payload V4
    pub execution_payload: BaseExecutionPayloadV4,
    /// The expected value to be received by the feeRecipient in wei
    pub block_value: U256,
    /// The blobs, commitments, and proofs associated with the executed payload.
    pub blobs_bundle: BlobsBundleV2,
    /// Introduced in V3, this represents a suggestion from the execution layer if the payload
    /// should be used instead of an externally provided one.
    pub should_override_builder: bool,
    /// EIP-7685 execution layer requests.
    pub execution_requests: Vec<Bytes>,
}

#[cfg(test)]
#[cfg(feature = "serde")]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_execution_payload_envelope_v5() {
        let response = r#"{"executionPayload":{"parentHash":"0xe927a1448525fb5d32cb50ee1408461a945ba6c39bd5cf5621407d500ecc8de9","feeRecipient":"0x0000000000000000000000000000000000000000","stateRoot":"0x10f8a0830000e8edef6d00cc727ff833f064b1950afd591ae41357f97e543119","receiptsRoot":"0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421","logsBloom":"0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000","prevRandao":"0xe0d8b4521a7da1582a713244ffb6a86aa1726932087386e2dc7973f43fc6cb24","blockNumber":"0x1","gasLimit":"0x2ffbd2","gasUsed":"0x0","timestamp":"0x1235","extraData":"0xd883010d00846765746888676f312e32312e30856c696e7578","baseFeePerGas":"0x342770c0","blockHash":"0x44d0fa5f2f73a938ebb96a2a21679eb8dea3e7b7dd8fd9f35aa756dda8bf0a8a","transactions":[],"withdrawals":[],"blobGasUsed":"0x0","excessBlobGas":"0x0","withdrawalsRoot":"0x123400000000000000000000000000000000000000000000000000000000babe"},"blockValue":"0x0","blobsBundle":{"commitments":[],"proofs":[],"blobs":[]},"shouldOverrideBuilder":false,"executionRequests":[]}"#;
        let envelope: BaseExecutionPayloadEnvelopeV5 = serde_json::from_str(response).unwrap();
        assert_eq!(serde_json::to_string(&envelope).unwrap(), response);
    }
}
