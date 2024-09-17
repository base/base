//! Optimism execution payload envelope V3.

use alloy_primitives::{B256, U256};
use alloy_rpc_types_engine::{BlobsBundleV1, ExecutionPayloadV1, ExecutionPayloadV4};

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

impl crate::AsInnerPayload for OptimismExecutionPayloadEnvelopeV4 {
    /// Returns the inner [ExecutionPayloadV1] from the envelope.
    fn as_v1_payload(&self) -> alloc::borrow::Cow<'_, ExecutionPayloadV1> {
        alloc::borrow::Cow::Borrowed(
            &self.execution_payload.payload_inner.payload_inner.payload_inner,
        )
    }
}
