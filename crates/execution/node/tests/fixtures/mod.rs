//! Base payload attributes used by migrated engine, networking, and storage scenarios.

use base_common_rpc_types_engine::PayloadAttributes;
use base_execution_payload_builder::BasePayloadBuilderAttributes;

/// Creates attributes for the Base execution implementation.
#[derive(Debug)]
pub struct BaseTestPayload;

impl BaseTestPayload {
    /// Preserves shared Engine API fields and supplies Base's explicit gas limit.
    pub fn attributes(attributes: PayloadAttributes) -> BasePayloadBuilderAttributes {
        BasePayloadBuilderAttributes { gas_limit: Some(30_000_000), ..attributes.into() }
    }
}
