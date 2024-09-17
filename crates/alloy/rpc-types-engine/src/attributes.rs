//! Optimism-specific payload attributes.

use alloc::vec::Vec;
use alloy_primitives::Bytes;
use alloy_rpc_types_engine::PayloadAttributes;
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
