use alloc::vec::Vec;

use alloy_consensus::{Block, BlockHeader, Transaction};
use alloy_primitives::B256;
use alloy_rpc_types_engine::{
    CancunPayloadFields, MaybeCancunPayloadFields, MaybePraguePayloadFields, PraguePayloadFields,
};

/// Container type for all available additional `newPayload` request parameters that are not present
/// in the [`ExecutionPayload`](alloy_rpc_types_engine::ExecutionPayload) object itself.
///
/// Default is equivalent to pre-ecotone, payloads v1 and v2.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BaseExecutionPayloadSidecar {
    /// Ecotone request params, inherited from Cancun, introduced in `engine_newPayloadV3` that are
    /// not present in the [`ExecutionPayloadV3`](alloy_rpc_types_engine::ExecutionPayloadV3).
    ///
    /// NOTE: Blob versioned hashes should always be empty. See <https://specs.base.org/protocol/execution#engine_newpayloadv3>.
    ecotone: MaybeCancunPayloadFields,
    /// Isthmus request params, inherited from Prague, introduced in `engine_newPayloadV4` that are
    /// not present in the [`BaseExecutionPayloadV4`].
    ///
    /// NOTE: These fields, i.e. the EL request hashes, should always be empty. See <https://specs.base.org/protocol/execution#engine_newpayloadv4>.
    isthmus: MaybePraguePayloadFields,
}

impl BaseExecutionPayloadSidecar {
    /// Extracts the [`BaseExecutionPayloadSidecar`] from the given [`Block`].
    ///
    /// Returns `BaseExecutionPayloadSidecar::default` if the block does not contain any sidecar
    /// fields (pre-ecotone).
    pub fn from_block<T, H>(block: &Block<T, H>) -> Self
    where
        T: Transaction,
        H: BlockHeader,
    {
        let ecotone =
            block.parent_beacon_block_root().map(|parent_beacon_block_root| CancunPayloadFields {
                parent_beacon_block_root,
                versioned_hashes: block.body.blob_versioned_hashes_iter().copied().collect(),
            });

        let isthmus = block.requests_hash().map(PraguePayloadFields::new);

        match (ecotone, isthmus) {
            (Some(ecotone), Some(isthmus)) => Self::v4(ecotone, isthmus),
            (Some(ecotone), None) => Self::v3(ecotone),
            _ => Self::default(),
        }
    }

    /// Creates a new instance for ecotone with the ecotone fields for `engine_newPayloadV3`
    pub fn v3(ecotone: CancunPayloadFields) -> Self {
        Self { ecotone: ecotone.into(), isthmus: Default::default() }
    }

    /// Creates a new instance post prague for `engine_newPayloadV4`
    pub fn v4(ecotone: CancunPayloadFields, isthmus: PraguePayloadFields) -> Self {
        Self { ecotone: ecotone.into(), isthmus: isthmus.into() }
    }

    /// Returns a reference to the [`CancunPayloadFields`].
    pub const fn ecotone(&self) -> Option<&CancunPayloadFields> {
        self.ecotone.as_ref()
    }

    /// Consumes the type and returns the [`CancunPayloadFields`]
    pub fn into_ecotone(self) -> Option<CancunPayloadFields> {
        self.ecotone.into_inner()
    }

    /// Returns a reference to the [`PraguePayloadFields`].
    pub const fn isthmus(&self) -> Option<&PraguePayloadFields> {
        self.isthmus.as_ref()
    }

    /// Consumes the type and returns the [`PraguePayloadFields`]
    pub fn into_isthmus(self) -> Option<PraguePayloadFields> {
        self.isthmus.into_inner()
    }

    /// Returns the parent beacon block root, if any.
    pub fn parent_beacon_block_root(&self) -> Option<B256> {
        self.ecotone.parent_beacon_block_root()
    }

    /// Returns the EL request hash. Should always be empty root hash, see docs for
    /// [`BaseExecutionPayloadSidecar`] isthmus fields.
    pub fn requests_hash(&self) -> Option<B256> {
        self.isthmus.requests_hash()
    }

    /// Returns the blob versioned hashes. Should always be empty array, see docs for
    /// [`BaseExecutionPayloadSidecar`] ecotone fields.
    pub fn versioned_hashes(&self) -> Option<&Vec<B256>> {
        self.ecotone.versioned_hashes()
    }
}
