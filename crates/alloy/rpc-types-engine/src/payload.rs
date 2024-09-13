use alloy_primitives::{Bytes, B256, U256};
use alloy_rpc_types_engine::{
    BlobsBundleV1, ExecutionPayloadV3, ExecutionPayloadV4, PayloadAttributes,
};
use op_alloy_protocol::L2BlockInfo;

/// Optimism Payload Attributes
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct OptimismPayloadAttributes {
    /// The payload attributes
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub payload_attributes: PayloadAttributes,
    /// Transactions is a field for rollups: the transactions list is forced into the block
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub transactions: Option<Vec<Bytes>>,
    /// If true, the no transactions are taken out of the tx-pool, only transactions from the above
    /// Transactions list will be included.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub no_tx_pool: Option<bool>,
    /// If set, this sets the exact gas limit the block produced with.
    #[cfg_attr(
        feature = "serde",
        serde(skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")
    )]
    pub gas_limit: Option<u64>,
}

/// Optimism Payload Attributes with parent block reference.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct OptimismAttributesWithParent {
    /// The payload attributes.
    pub attributes: OptimismPayloadAttributes,
    /// The parent block reference.
    pub parent: L2BlockInfo,
    /// Whether the current batch is the last in its span.
    pub is_last_in_span: bool,
}

impl OptimismAttributesWithParent {
    /// Create a new [OptimismAttributesWithParent] instance.
    pub const fn new(
        attributes: OptimismPayloadAttributes,
        parent: L2BlockInfo,
        is_last_in_span: bool,
    ) -> Self {
        Self { attributes, parent, is_last_in_span }
    }

    /// Returns the payload attributes.
    pub const fn attributes(&self) -> &OptimismPayloadAttributes {
        &self.attributes
    }

    /// Returns the parent block reference.
    pub const fn parent(&self) -> &L2BlockInfo {
        &self.parent
    }

    /// Returns whether the current batch is the last in its span.
    pub const fn is_last_in_span(&self) -> bool {
        self.is_last_in_span
    }
}

/// This structure maps for the return value of `engine_getPayload` of the beacon chain spec, for
/// V3.
///
/// See also:
/// [Optimism execution payload envelope v3] <https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/exec-engine.md#engine_getpayloadv3>
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct OptimismExecutionPayloadEnvelopeV3 {
    /// Execution payload V3
    pub execution_payload: ExecutionPayloadV3,
    /// The expected value to be received by the feeRecipient in wei
    pub block_value: U256,
    /// The blobs, commitments, and proofs associated with the executed payload.
    pub blobs_bundle: BlobsBundleV1,
    /// Introduced in V3, this represents a suggestion from the execution layer if the payload
    /// should be used instead of an externally provided one.
    pub should_override_builder: bool,
    /// Ecotone parent beacon block root
    pub parent_beacon_block_root: B256,
}

/// This structure maps for the return value of `engine_getPayload` of the beacon chain spec, for
/// V4.
///
/// See also:
/// [Optimism execution payload envelope v4] <https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/exec-engine.md#engine_getpayloadv4>
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct OptimismExecutionPayloadEnvelopeV4 {
    /// Execution payload V4
    pub execution_payload: ExecutionPayloadV4,
    /// The expected value to be received by the feeRecipient in wei
    pub block_value: U256,
    /// The blobs, commitments, and proofs associated with the executed payload.
    pub blobs_bundle: BlobsBundleV1,
    /// Introduced in V3, this represents a suggestion from the execution layer if the payload
    /// should be used instead of an externally provided one.
    pub should_override_builder: bool,
    /// Ecotone parent beacon block root
    pub parent_beacon_block_root: B256,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(feature = "serde")]
    fn serde_roundtrip_execution_payload_envelope_v3() {
        // pulled from a geth response getPayloadV3 in hive tests, modified to add a mock parent
        // beacon block root.
        let response = r#"{"executionPayload":{"parentHash":"0xe927a1448525fb5d32cb50ee1408461a945ba6c39bd5cf5621407d500ecc8de9","feeRecipient":"0x0000000000000000000000000000000000000000","stateRoot":"0x10f8a0830000e8edef6d00cc727ff833f064b1950afd591ae41357f97e543119","receiptsRoot":"0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421","logsBloom":"0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000","prevRandao":"0xe0d8b4521a7da1582a713244ffb6a86aa1726932087386e2dc7973f43fc6cb24","blockNumber":"0x1","gasLimit":"0x2ffbd2","gasUsed":"0x0","timestamp":"0x1235","extraData":"0xd883010d00846765746888676f312e32312e30856c696e7578","baseFeePerGas":"0x342770c0","blockHash":"0x44d0fa5f2f73a938ebb96a2a21679eb8dea3e7b7dd8fd9f35aa756dda8bf0a8a","transactions":[],"withdrawals":[],"blobGasUsed":"0x0","excessBlobGas":"0x0"},"blockValue":"0x0","blobsBundle":{"commitments":[],"proofs":[],"blobs":[]},"shouldOverrideBuilder":false,"parentBeaconBlockRoot":"0xdead00000000000000000000000000000000000000000000000000000000beef"}"#;
        let envelope: OptimismExecutionPayloadEnvelopeV3 = serde_json::from_str(response).unwrap();
        assert_eq!(serde_json::to_string(&envelope).unwrap(), response);
    }
}
