//! Output Types

use alloy_primitives::B256;
use base_common_types_chain::{L2BlockInfo, OutputRoot};

use crate::RollupSyncStatus;

/// An [output response][or] for Base Rollup.
///
/// [or]: https://github.com/ethereum-optimism/optimism/blob/f20b92d3eb379355c876502c4f28e72a91ab902f/op-service/eth/output.go#L10-L17
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct OutputResponse {
    /// The output version.
    pub version: B256,
    /// The output root hash.
    pub output_root: B256,
    /// A reference to the L2 block.
    pub block_ref: L2BlockInfo,
    /// The withdrawal storage root.
    pub withdrawal_storage_root: B256,
    /// The state root.
    pub state_root: B256,
    /// The status of the node sync.
    pub sync_status: RollupSyncStatus,
}

impl OutputResponse {
    /// Builds an [`OutputResponse`] from its parts.
    pub fn from_v0(v0: OutputRoot, sync_status: RollupSyncStatus, block_ref: L2BlockInfo) -> Self {
        Self {
            version: v0.version(),
            output_root: v0.hash(),
            block_ref,
            withdrawal_storage_root: v0.bridge_storage_root,
            state_root: v0.state_root,
            sync_status,
        }
    }
}
