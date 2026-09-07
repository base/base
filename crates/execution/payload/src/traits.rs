use alloy_primitives::B256;
use alloy_rpc_types_engine::PayloadId;
use base_common_rpc_types_engine::BasePayloadAttributes;
use reth_payload_primitives::PayloadAttributes;
use reth_primitives_traits::{SignedTransaction, WithEncoded};

use crate::BasePayloadBuilderAttributes;

/// Attributes for the payload builder.
pub trait Attributes: PayloadAttributes {
    /// Primitive transaction type.
    type Transaction: SignedTransaction;
    /// RPC payload attributes type accepted by the builder.
    type RpcPayloadAttributes;

    /// Creates builder attributes for the given parent and RPC payload attributes.
    fn try_new(
        parent: B256,
        attributes: Self::RpcPayloadAttributes,
        version: u8,
    ) -> Result<Self, alloy_rlp::Error>
    where
        Self: Sized;

    /// Returns the precomputed payload job ID.
    fn payload_job_id(&self) -> PayloadId;

    /// Whether to use the transaction pool for the payload.
    fn no_tx_pool(&self) -> bool;

    /// Sequencer transactions to include in the payload.
    fn sequencer_transactions(&self) -> &[WithEncoded<Self::Transaction>];
}

impl<T: SignedTransaction> Attributes for BasePayloadBuilderAttributes<T> {
    type Transaction = T;
    type RpcPayloadAttributes = BasePayloadAttributes;

    fn try_new(
        parent: B256,
        attributes: Self::RpcPayloadAttributes,
        version: u8,
    ) -> Result<Self, alloy_rlp::Error> {
        Self::try_new(parent, attributes, version)
    }

    fn payload_job_id(&self) -> PayloadId {
        self.payload_attributes.id
    }

    fn no_tx_pool(&self) -> bool {
        self.no_tx_pool
    }

    fn sequencer_transactions(&self) -> &[WithEncoded<Self::Transaction>] {
        &self.transactions
    }
}
