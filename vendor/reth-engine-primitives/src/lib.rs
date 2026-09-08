//! Traits, validation methods, and helper types used to abstract over engine types.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

use base_common_consensus::BaseTxEnvelope;
use base_execution_payload_types::BasePayloadBuilderAttributes;
extern crate alloc;

use base_common_consensus::BlockHeader;
// Re-export [`ExecutionPayload`] moved to `base_execution_payload_types`
#[cfg(feature = "std")]
pub use base_execution_evm::ConvertTx;
pub use base_execution_evm::{ExecutableTxIterator, ExecutableTxTuple};
pub use base_execution_payload_types::ExecutionPayload;
use base_execution_payload_types::{
    EngineApiMessageVersion, EngineObjectValidationError, InvalidPayloadAttributesError,
    NewPayloadError, PayloadAttributes, PayloadOrAttributes,
};
use reth_primitives_traits::{Block, RecoveredBlock, SealedBlock, SealedHeader};
use reth_storage_api::{StateProviderBox, errors::ProviderResult};
use reth_trie_common::HashedPostState;

mod error;
pub use error::*;

mod forkchoice;
pub use forkchoice::{ForkchoiceStateHash, ForkchoiceStateTracker, ForkchoiceStatus};

#[cfg(feature = "std")]
mod message;
#[cfg(feature = "std")]
pub use message::*;

mod event;
pub use event::*;

mod invalid_block_hook;
pub use invalid_block_hook::{InvalidBlockHook, InvalidBlockHooks, NoopInvalidBlockHook};

pub mod config;
pub use config::*;

/// Validates engine API requests at the RPC layer, before payloads and attributes
/// are forwarded to the engine for processing.
///
/// - [`validate_version_specific_fields`](Self::validate_version_specific_fields): Enforced in each
///   `engine_newPayloadVN` RPC handler to verify the payload contains the correct fields for the
///   engine API version (e.g., blob fields in V3+, requests in V4+).
///
/// - [`ensure_well_formed_attributes`](Self::ensure_well_formed_attributes): Enforced in
///   `engine_forkchoiceUpdatedVN` RPC handlers to validate payload attributes are well-formed for
///   the given version before forwarding to the engine.
///
/// After this validation passes, the engine performs the full consensus validation
/// pipeline (header, pre-execution, execution, post-execution).
pub trait EngineApiValidator: Send + Sync + Unpin + 'static {
    /// Validates the presence or exclusion of fork-specific fields based on the payload attributes
    /// and the message version.
    fn validate_version_specific_fields(
        &self,
        version: EngineApiMessageVersion,
        payload_or_attrs: PayloadOrAttributes<
            '_,
            base_common_rpc_types_engine::ExecutionData,
            BasePayloadBuilderAttributes<BaseTxEnvelope>,
        >,
    ) -> Result<(), EngineObjectValidationError>;

    /// Ensures that the payload attributes are valid for the given [`EngineApiMessageVersion`].
    fn ensure_well_formed_attributes(
        &self,
        version: EngineApiMessageVersion,
        attributes: &BasePayloadBuilderAttributes<BaseTxEnvelope>,
    ) -> Result<(), EngineObjectValidationError>;
}

/// Type that validates an [`ExecutionPayload`].
///
/// This trait handles validation at the engine API boundary — converting payloads
/// into blocks and validating payload attributes for block building.
///
/// # Methods and when they're used
///
/// - [`convert_payload_to_block`](Self::convert_payload_to_block): Used during `engine_newPayload`
///   processing to decode the payload into a [`SealedBlock`]. Also used to validate payload
///   structure during backfill buffering. In the engine tree, this runs concurrently with state
///   setup on a background thread.
///
/// - [`ensure_well_formed_payload`](Self::ensure_well_formed_payload): Converts payload and
///   recovers transaction signatures. Used when recovered senders are needed immediately.
///
/// - [`validate_payload_attributes_against_header`](Self::validate_payload_attributes_against_header):
///   Enforced as part of the engine's `forkchoiceUpdated` handling when payload attributes
///   are provided. Gates whether a payload build job is started.
///
/// - [`validate_block_post_execution_with_hashed_state`](Self::validate_block_post_execution_with_hashed_state):
///   Called after block execution in the engine's payload validation pipeline.
///   No-op on L1, used by L2s (e.g., OP Stack) for additional post-execution checks.
///
/// # Relationship to consensus traits
///
/// This trait does NOT replace the Base consensus validator (`BaseBeaconConsensus`). Those handle the actual consensus rule
/// validation (header checks, pre/post-execution). This trait handles engine API-specific
/// concerns: payload encoding/decoding and attribute validation.
#[auto_impl::auto_impl(&, Arc)]
pub trait PayloadValidator: Send + Sync + Unpin + 'static {
    /// The block type used by the engine.
    type Block: Block;

    /// Converts the given payload into a sealed block without recovering signatures.
    ///
    /// This function validates the payload and converts it into a [`SealedBlock`] which contains
    /// the block hash but does not perform signature recovery on transactions.
    ///
    /// This is more efficient than [`Self::ensure_well_formed_payload`] when signature recovery
    /// is not needed immediately or will be performed later.
    ///
    /// Implementers should ensure that the checks are done in the order that conforms with the
    /// engine-API specification.
    fn convert_payload_to_block(
        &self,
        payload: base_common_rpc_types_engine::ExecutionData,
    ) -> Result<SealedBlock, NewPayloadError>;

    /// Ensures that the given payload does not violate any consensus rules that concern the block's
    /// layout.
    ///
    /// This function must convert the payload into the executable block and pre-validate its
    /// fields.
    ///
    /// Implementers should ensure that the checks are done in the order that conforms with the
    /// engine-API specification.
    fn ensure_well_formed_payload(
        &self,
        payload: base_common_rpc_types_engine::ExecutionData,
    ) -> Result<RecoveredBlock, NewPayloadError> {
        let sealed_block = self.convert_payload_to_block(payload)?;
        sealed_block.try_recover().map_err(|e| NewPayloadError::Other(e.into()))
    }

    /// Verifies payload post-execution w.r.t. hashed state updates.
    ///
    /// `state_updates` lazily yields the block's hashed post-state; call it only if the
    /// implementation needs the executed state changes (the L1 default does not).
    ///
    /// `parent_header` is the parent header the engine resolved for the block.
    ///
    /// `parent_state` lazily builds the overlay-aware provider for the block's parent that the
    /// engine used for execution — resolving even a not-yet-canonical in-memory parent. It is only
    /// built if the implementation needs it (the L1 default does not).
    fn validate_block_post_execution_with_hashed_state<'a>(
        &self,
        _state_updates: impl FnOnce() -> &'a HashedPostState,
        _block: &RecoveredBlock,
        _parent_header: &SealedHeader,
        _parent_state: impl FnOnce() -> ProviderResult<StateProviderBox>,
    ) -> Result<(), InsertBlockErrorKind>
    where
        Self: Sized,
    {
        // method not used by l1
        Ok(())
    }

    /// Validates the payload attributes with respect to the header.
    ///
    /// By default, this enforces that the payload attributes timestamp is greater than the
    /// timestamp according to:
    ///   > 7. Client software MUST ensure that payloadAttributes.timestamp is greater than
    ///   > timestamp
    ///   > of a block referenced by forkchoiceState.headBlockHash.
    ///
    /// See also: <https://github.com/ethereum/execution-apis/blob/main/src/engine/common.md#specification-1>
    ///
    /// Enforced as part of the engine's `forkchoiceUpdated` handling when the consensus layer
    /// provides payload attributes. If this returns an error, the forkchoice state update itself
    /// is NOT rolled back, but no payload build job is started — the response includes
    /// `INVALID_PAYLOAD_ATTRIBUTES`.
    fn validate_payload_attributes_against_header(
        &self,
        attr: &BasePayloadBuilderAttributes<BaseTxEnvelope>,
        header: &base_common_consensus::Header,
    ) -> Result<(), InvalidPayloadAttributesError> {
        if attr.timestamp() <= header.timestamp() {
            return Err(InvalidPayloadAttributesError::InvalidTimestamp);
        }
        Ok(())
    }
}

/// Fixtures for tests of the shared execution infrastructure.
#[cfg(feature = "test-utils")]
pub mod test_utils;
pub use error::EngineRequestError;
#[cfg(feature = "test-utils")]
pub use test_utils::TestEngineValidator;
