//! Engine API fixture for transport tests. It validates the shared version rules and decodes
//! payloads without constructing a node, payload builder, or network-specific consensus service.

use std::sync::Arc;

use alloy_rpc_types_engine::{ExecutionData, PayloadAttributes};
use reth_chainspec::{ChainSpec, EthChainSpec, EthereumHardforks};
use reth_ethereum_primitives::Block;
use reth_payload_primitives::{
    EngineApiMessageVersion, EngineObjectValidationError, NewPayloadError, PayloadOrAttributes,
    PayloadTypes, validate_execution_requests, validate_version_specific_fields,
};
use reth_primitives_traits::{Block as _, SealedBlock};

use crate::{EngineApiValidator, PayloadValidator};

/// Validator for tests of the shared Engine API transport and version handling.
#[derive(Debug, Clone)]
pub struct TestEngineValidator<C = ChainSpec> {
    /// Fork schedule used by the shared version checks.
    pub chain_spec: Arc<C>,
}

impl<C> TestEngineValidator<C> {
    /// Creates a validator for the supplied test fork schedule.
    pub const fn new(chain_spec: Arc<C>) -> Self {
        Self { chain_spec }
    }
}

impl<C, T> PayloadValidator<T> for TestEngineValidator<C>
where
    C: EthChainSpec + EthereumHardforks + 'static,
    T: PayloadTypes<ExecutionData = ExecutionData>,
{
    type Block = Block;

    fn convert_payload_to_block(
        &self,
        data: ExecutionData,
    ) -> Result<SealedBlock<Block>, NewPayloadError> {
        Ok(data.payload.try_into_block_with_sidecar(&data.sidecar)?.seal_slow())
    }
}

impl<C, T> EngineApiValidator<T> for TestEngineValidator<C>
where
    C: EthChainSpec + EthereumHardforks + 'static,
    T: PayloadTypes<ExecutionData = ExecutionData, PayloadAttributes = PayloadAttributes>,
{
    fn validate_version_specific_fields(
        &self,
        version: EngineApiMessageVersion,
        object: PayloadOrAttributes<'_, ExecutionData, PayloadAttributes>,
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
        attributes: &PayloadAttributes,
    ) -> Result<(), EngineObjectValidationError> {
        validate_version_specific_fields(
            &self.chain_spec,
            version,
            PayloadOrAttributes::<ExecutionData, PayloadAttributes>::PayloadAttributes(attributes),
        )
    }
}
