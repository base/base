//! Engine response types used by the Base test harness.

use alloy_rpc_types_engine::{ExecutionPayloadEnvelopeV2, ExecutionPayloadV1};
use base_common_rpc_types_engine::{
    BaseExecutionPayloadEnvelopeV3, BaseExecutionPayloadEnvelopeV4, BaseExecutionPayloadEnvelopeV5,
};

use crate::EngineTypes;

/// Concrete Engine API response types for Base test nodes.
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
pub struct TestEngineTypes;

impl EngineTypes for TestEngineTypes {
    type ExecutionPayloadEnvelopeV1 = ExecutionPayloadV1;
    type ExecutionPayloadEnvelopeV2 = ExecutionPayloadEnvelopeV2;
    type ExecutionPayloadEnvelopeV3 = BaseExecutionPayloadEnvelopeV3;
    type ExecutionPayloadEnvelopeV4 = BaseExecutionPayloadEnvelopeV4;
    type ExecutionPayloadEnvelopeV5 = BaseExecutionPayloadEnvelopeV5;
    type ExecutionPayloadEnvelopeV6 = BaseExecutionPayloadEnvelopeV5;
}
