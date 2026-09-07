//! Minimal payload fixtures for tests of engine messages, providers, and payload scheduling.
//! Blob construction and network-specific payload building are deliberately outside this fixture.

use std::sync::Arc;

use alloy_eips::eip7685::Requests;
use alloy_primitives::{Bytes, U256};
use alloy_rpc_types_engine::{
    BlobsBundleV1, BlobsBundleV2, ExecutionData, ExecutionPayload, ExecutionPayloadEnvelopeV2,
    ExecutionPayloadEnvelopeV3, ExecutionPayloadEnvelopeV4, ExecutionPayloadEnvelopeV5,
    ExecutionPayloadEnvelopeV6, ExecutionPayloadFieldV2, ExecutionPayloadV1, ExecutionPayloadV3,
    ExecutionPayloadV4, PayloadAttributes,
};
use base_common_consensus::BaseBlock as Block;
use reth_payload_primitives::{BuiltPayload, PayloadTypes};
use reth_primitives_traits::{RecoveredBlock, SealedBlock};

use crate::{BigBlockData, EngineTypes};

/// Concrete message types for tests of generic engine infrastructure.
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
pub struct TestEngineTypes;

impl PayloadTypes for TestEngineTypes {
    type ExecutionData = ExecutionData;
    type BuiltPayload = TestBuiltPayload;
    type PayloadAttributes = PayloadAttributes;

    fn block_to_payload(block: SealedBlock<Block>, bal: Option<Bytes>) -> ExecutionData {
        let (payload, sidecar) = ExecutionPayload::from_block_unchecked_with_extras(
            block.hash(),
            &block.into_block(),
            bal,
        );
        ExecutionData { payload, sidecar }
    }
}

impl EngineTypes for TestEngineTypes {
    type ExecutionPayloadEnvelopeV1 = ExecutionPayloadV1;
    type ExecutionPayloadEnvelopeV2 = ExecutionPayloadEnvelopeV2;
    type ExecutionPayloadEnvelopeV3 = ExecutionPayloadEnvelopeV3;
    type ExecutionPayloadEnvelopeV4 = ExecutionPayloadEnvelopeV4;
    type ExecutionPayloadEnvelopeV5 = ExecutionPayloadEnvelopeV5;
    type ExecutionPayloadEnvelopeV6 = ExecutionPayloadEnvelopeV6;
}

/// Block and execution metadata supplied by test payload jobs.
#[derive(Debug, Clone)]
pub struct TestBuiltPayload {
    /// Recovered test block.
    pub block: Arc<RecoveredBlock<Block>>,
    /// Fees returned by the test job.
    pub fees: U256,
    /// Execution requests, if supplied by the test.
    pub requests: Option<Requests>,
    /// Encoded access list, if supplied by the test.
    pub block_access_list: Option<Bytes>,
}

impl TestBuiltPayload {
    /// Creates a payload fixture from an already executed test block.
    pub const fn new(
        block: Arc<RecoveredBlock<Block>>,
        fees: U256,
        requests: Option<Requests>,
        block_access_list: Option<Bytes>,
    ) -> Self {
        Self { block, fees, requests, block_access_list }
    }
}

impl BuiltPayload for TestBuiltPayload {
    fn block(&self) -> &SealedBlock<Block> {
        self.block.sealed_block()
    }
    fn fees(&self) -> U256 {
        self.fees
    }
    fn requests(&self) -> Option<Requests> {
        self.requests.clone()
    }
    fn block_access_list(&self) -> Option<&Bytes> {
        self.block_access_list.as_ref()
    }
}

impl From<TestBuiltPayload> for ExecutionPayloadV1 {
    fn from(value: TestBuiltPayload) -> Self {
        Self::from_block_unchecked(
            value.block.hash(),
            &Arc::unwrap_or_clone(value.block).into_block(),
        )
    }
}

impl From<TestBuiltPayload> for ExecutionPayloadEnvelopeV2 {
    fn from(value: TestBuiltPayload) -> Self {
        Self {
            block_value: value.fees,
            execution_payload: ExecutionPayloadFieldV2::from_block_unchecked(
                value.block.hash(),
                &Arc::unwrap_or_clone(value.block).into_block(),
            ),
        }
    }
}

impl From<TestBuiltPayload> for ExecutionPayloadEnvelopeV3 {
    fn from(value: TestBuiltPayload) -> Self {
        Self {
            block_value: value.fees,
            execution_payload: ExecutionPayloadV3::from_block_unchecked(
                value.block.hash(),
                &Arc::unwrap_or_clone(value.block).into_block(),
            ),
            blobs_bundle: BlobsBundleV1::empty(),
            should_override_builder: false,
        }
    }
}

impl From<TestBuiltPayload> for ExecutionPayloadEnvelopeV4 {
    fn from(value: TestBuiltPayload) -> Self {
        Self {
            execution_requests: value.requests.clone().unwrap_or_default(),
            envelope_inner: value.into(),
        }
    }
}

impl From<TestBuiltPayload> for ExecutionPayloadEnvelopeV5 {
    fn from(value: TestBuiltPayload) -> Self {
        Self {
            block_value: value.fees,
            execution_requests: value.requests.unwrap_or_default(),
            execution_payload: ExecutionPayloadV3::from_block_unchecked(
                value.block.hash(),
                &Arc::unwrap_or_clone(value.block).into_block(),
            ),
            blobs_bundle: BlobsBundleV2::empty(),
            should_override_builder: false,
        }
    }
}

impl TryFrom<TestBuiltPayload> for ExecutionPayloadEnvelopeV6 {
    type Error = std::io::Error;

    fn try_from(value: TestBuiltPayload) -> Result<Self, Self::Error> {
        let bal = value
            .block_access_list
            .ok_or_else(|| std::io::Error::other("test payload has no block access list"))?;
        Ok(Self {
            block_value: value.fees,
            execution_requests: value.requests.unwrap_or_default(),
            execution_payload: ExecutionPayloadV4::from_block_unchecked_with_bal(
                value.block.hash(),
                &Arc::unwrap_or_clone(value.block).into_block(),
                bal,
            ),
            blobs_bundle: BlobsBundleV2::empty(),
            should_override_builder: false,
        })
    }
}

impl From<TestBuiltPayload> for ExecutionData {
    fn from(value: TestBuiltPayload) -> Self {
        TestEngineTypes::block_to_payload(
            value.block.sealed_block().clone(),
            value.block_access_list,
        )
    }
}

impl From<TestBuiltPayload> for BigBlockData<ExecutionData> {
    fn from(_value: TestBuiltPayload) -> Self {
        unreachable!("test payload jobs do not produce big blocks")
    }
}
