//! Engine API fixture for transport tests. It validates the shared version rules and decodes
//! payloads without constructing a node, payload builder, or network-specific consensus service.

use std::sync::Arc;

use base_common_consensus::{BaseBlock as Block, BaseTxEnvelope};
use base_common_rpc_types_engine::{BasePayloadError, ExecutionData};
use base_execution_chainspec::BaseChainSpec;
use base_execution_payload_types::{
    BasePayloadBuilderAttributes as PayloadAttributes, EngineApiMessageVersion,
    EngineObjectValidationError, NewPayloadError, PayloadOrAttributes, validate_execution_requests,
    validate_version_specific_fields,
};
use reth_primitives_traits::{Block as _, SealedBlock};

use crate::{EngineApiValidator, PayloadValidator};

/// Validator for tests of the shared Engine API transport and version handling.
#[derive(Debug, Clone)]
pub struct TestEngineValidator {
    /// Fork schedule used by the shared version checks.
    pub chain_spec: Arc<BaseChainSpec>,
}

impl TestEngineValidator {
    /// Creates a validator for the supplied test fork schedule.
    pub const fn new(chain_spec: Arc<BaseChainSpec>) -> Self {
        Self { chain_spec }
    }
}

impl PayloadValidator for TestEngineValidator {
    type Block = Block;

    fn convert_payload_to_block(
        &self,
        data: ExecutionData,
    ) -> Result<SealedBlock, NewPayloadError> {
        Ok(data
            .payload
            .try_into_block_with_sidecar(&data.sidecar)
            .map_err(|error| match error {
                BasePayloadError::Eth(error) => NewPayloadError::Eth(error),
                error => NewPayloadError::other(error),
            })?
            .seal_slow())
    }
}

impl EngineApiValidator for TestEngineValidator {
    fn validate_version_specific_fields(
        &self,
        version: EngineApiMessageVersion,
        object: PayloadOrAttributes<'_, ExecutionData, PayloadAttributes<BaseTxEnvelope>>,
    ) -> Result<(), EngineObjectValidationError> {
        object
            .execution_requests()
            .map(|requests| validate_execution_requests(requests))
            .transpose()?;
        validate_version_specific_fields(&self.chain_spec, version, object)
    }

    fn ensure_well_formed_attributes(
        &self,
        version: EngineApiMessageVersion,
        attributes: &PayloadAttributes<BaseTxEnvelope>,
    ) -> Result<(), EngineObjectValidationError> {
        validate_version_specific_fields(
            &self.chain_spec,
            version,
            PayloadOrAttributes::<ExecutionData, PayloadAttributes<BaseTxEnvelope>>::PayloadAttributes(attributes),
        )
    }
}
