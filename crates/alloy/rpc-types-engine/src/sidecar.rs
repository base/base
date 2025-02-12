use alloy_consensus::{Block, BlockHeader, Transaction};
use alloy_primitives::B256;
use alloy_rpc_types_engine::{CancunPayloadFields, MaybeCancunPayloadFields};
use derive_more::Into;

/// Container type for all available additional `newPayload` request parameters that are not present
/// in the [`ExecutionPayload`](alloy_rpc_types_engine::ExecutionPayload) object itself.
///
/// Default is equivalent to pre-canyon, payloads v1 and v2.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct OpExecutionPayloadSidecar {
    /// Canyon request params, inherited from Cancun, introduced in `engine_newPayloadV3` that are
    /// not present in the [`ExecutionPayload`](alloy_rpc_types_engine::ExecutionPayload).
    canyon: MaybeCancunPayloadFields,
}

impl OpExecutionPayloadSidecar {
    /// Extracts the [`OpExecutionPayloadSidecar`] from the given [`Block`].
    ///
    /// Returns `OpExecutionPayloadSidecar::default` if the block does not contain any sidecar
    /// fields (pre-canyon).
    pub fn from_block<T, H>(block: &Block<T, H>) -> Self
    where
        T: Transaction,
        H: BlockHeader,
    {
        let canyon =
            block.parent_beacon_block_root().map(|parent_beacon_block_root| CancunPayloadFields {
                parent_beacon_block_root,
                versioned_hashes: block.body.blob_versioned_hashes_iter().copied().collect(),
            });

        canyon.map_or_else(Self::default, Self::v4)
    }

    /// Creates a new instance for canyon with the canyon fields for `engine_newPayloadV3`
    pub fn v3(canyon: CancunPayloadFields) -> Self {
        Self { canyon: canyon.into() }
    }

    /// Creates a new instance post prague for `engine_newPayloadV4`
    pub fn v4(canyon: CancunPayloadFields) -> Self {
        Self { canyon: canyon.into() }
    }

    /// Returns a reference to the [`CancunPayloadFields`].
    pub const fn canyon(&self) -> Option<&CancunPayloadFields> {
        self.canyon.as_ref()
    }

    /// Consumes the type and returns the [`CancunPayloadFields`]
    pub fn into_canyon(self) -> Option<CancunPayloadFields> {
        self.canyon.into_inner()
    }

    /// Returns the parent beacon block root, if any.
    pub fn parent_beacon_block_root(&self) -> Option<B256> {
        self.canyon.parent_beacon_block_root()
    }
}
