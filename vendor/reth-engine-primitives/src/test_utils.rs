//! Payload decoding fixture for execution tests.

use std::sync::Arc;

use base_common_consensus::BaseBlock as Block;
use base_common_rpc_types_engine::{BasePayloadError, ExecutionData};
use base_execution_chainspec::BaseChainSpec;
use base_execution_payload_types::NewPayloadError;
use reth_primitives_traits::{Block as _, SealedBlock};

use crate::PayloadValidator;

/// Validator for tests of the payload decoding.
#[derive(Debug, Clone)]
pub struct TestEngineValidator {
    /// Chain configuration for test callers.
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
