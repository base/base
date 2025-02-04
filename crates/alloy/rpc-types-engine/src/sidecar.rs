use alloy_consensus::{Block, BlockHeader, Transaction, EMPTY_ROOT_HASH};
use alloy_primitives::B256;
use alloy_rpc_types_engine::{CancunPayloadFields, MaybeCancunPayloadFields};
use derive_more::{Constructor, From, Into};

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
    /// Storage root of `L2ToL1MessagePasser.sol`, aka l2 withdrawals root, requires state to
    /// compute, hence root is passed in sidecar.
    ///
    /// <https://specs.optimism.io/protocol/isthmus/exec-engine.html#update-to-executabledata>
    isthmus: MaybeIsthmusPayloadFields,
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

        let isthmus = block
            .withdrawals_root()
            .filter(|root| *root != EMPTY_ROOT_HASH)
            .map(IsthmusPayloadFields::new);

        match (canyon, isthmus) {
            (Some(canyon), Some(isthmus)) => Self::v4(canyon, isthmus),
            (Some(canyon), None) => Self::v3(canyon),
            _ => Self::default(),
        }
    }

    /// Creates a new instance for canyon with the canyon fields for `engine_newPayloadV3`
    pub fn v3(canyon: CancunPayloadFields) -> Self {
        Self { canyon: canyon.into(), ..Default::default() }
    }

    /// Creates a new instance post prague for `engine_newPayloadV4`
    pub fn v4(canyon: CancunPayloadFields, isthmus: IsthmusPayloadFields) -> Self {
        Self { canyon: canyon.into(), isthmus: isthmus.into() }
    }

    /// Returns a reference to the [`CancunPayloadFields`].
    pub const fn canyon(&self) -> Option<&CancunPayloadFields> {
        self.canyon.as_ref()
    }

    /// Consumes the type and returns the [`CancunPayloadFields`]
    pub fn into_canyon(self) -> Option<CancunPayloadFields> {
        self.canyon.into_inner()
    }

    /// Returns a reference to the [`IsthmusPayloadFields`].
    pub const fn isthmus(&self) -> Option<&IsthmusPayloadFields> {
        self.isthmus.fields.as_ref()
    }

    /// Consumes the type and returns the [`IsthmusPayloadFields`].
    pub fn into_isthmus(self) -> Option<IsthmusPayloadFields> {
        self.isthmus.into()
    }

    /// Returns the parent beacon block root, if any.
    pub fn parent_beacon_block_root(&self) -> Option<B256> {
        self.canyon.parent_beacon_block_root()
    }

    /// Returns the withdrawals root, if any.
    pub fn withdrawals_root(&self) -> Option<&B256> {
        self.isthmus().map(|fields| &fields.withdrawals_root)
    }
}

/// Fields introduced in `engine_newPayloadV4` that are not present in the
/// [`ExecutionPayload`](alloy_rpc_types_engine::ExecutionPayload) RPC object.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Constructor)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct IsthmusPayloadFields {
    /// EIP-7685 requests.
    pub withdrawals_root: B256,
}

/// A container type for [`IsthmusPayloadFields`] that may or may not be present.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, From, Into)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[from(IsthmusPayloadFields)]
pub struct MaybeIsthmusPayloadFields {
    fields: Option<IsthmusPayloadFields>,
}
